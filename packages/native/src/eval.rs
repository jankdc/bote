//! Per-child projection of compiled IR against the source, via the session.
//!
//! Sits *above* [`Session`] and outside the IR module (`select`), so `select`
//! stays pure data below the session.

use crate::chunks::ChunkWindow;
use crate::keys::compare;
use crate::path::Segment;
use crate::resolve::{ChildKey, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};
use crate::walker::TraverseError;

/// Project a child into its yielded value per `select`: a single sub-path
/// yields the bare sub-value; a map yields an object of named sub-values in
/// declared order. A missing sub-path yields `null` (projection is lossy,
/// not a filter). Only the projected bytes materialize.
pub async fn project(
  session: &Session,
  select: &CompiledSelect,
  child_start: u64,
  window: &mut ChunkWindow,
  out: &mut Vec<u8>,
) -> Result<(), SessionError> {
  match select {
    CompiledSelect::One(path) => project_one(session, path, child_start, window, out).await,
    CompiledSelect::Map(fields) => project_map(session, fields, child_start, window, out).await,
  }
}

async fn project_map(
  session: &Session,
  fields: &[(String, Vec<Segment>)],
  child_start: u64,
  window: &mut ChunkWindow,
  out: &mut Vec<u8>,
) -> Result<(), SessionError> {
  let mut matched: Vec<Option<ValueLocation>> = vec![None; fields.len()];
  let mut remaining = fields.iter().filter(|(_, p)| !p.is_empty()).count();

  if remaining > 0 {
    if let Some(mut cursor) = session.enter_container(child_start, window).await? {
      let mut hits: Vec<usize> = Vec::new();
      while remaining > 0 {
        let Some(entry) = session.next_child(&mut cursor, window).await? else {
          break; // unmatched fields stay null
        };
        match entry.key {
          ChildKey::Member { start, close } if wants_member(&matched, fields) => {
            session
              .probe_member_key(start, close, window, |interior| {
                hits.clear();
                for (i, (_, path)) in fields.iter().enumerate() {
                  if matched[i].is_none() {
                    if let Some(Segment::Member(name)) = path.first() {
                      if compare(interior, name).map_err(|()| TraverseError::Malformed(start))? {
                        hits.push(i);
                      }
                    }
                  }
                }
                Ok(())
              })
              .await?;
            for &i in &hits {
              matched[i] = Some(entry.location);
              remaining -= 1;
            }
          }
          ChildKey::Index(index) => {
            for (i, (_, path)) in fields.iter().enumerate() {
              if matched[i].is_none() {
                if let Some(Segment::Element(idx)) = path.first() {
                  if *idx == index {
                    matched[i] = Some(entry.location);
                    remaining -= 1;
                  }
                }
              }
            }
          }
          // a member key, but no unmatched member field wants it: skip the read
          ChildKey::Member { .. } => {}
        }
        session.prune_window(window, cursor.next_offset);
      }
    }
  }

  out.push(b'{');
  for (i, ((key, path), loc)) in fields.iter().zip(matched).enumerate() {
    if i > 0 {
      out.push(b',');
    }
    emit_json_string(out, key);
    out.push(b':');
    match (path.len(), loc) {
      // defensive: facade rejects empty sub-paths; treat as the whole child
      (0, _) => project_one(session, path, child_start, window, out).await?,
      (_, None) => out.extend_from_slice(b"null"),
      // single segment: the matched entry is already the value
      (1, Some(loc)) => session.materialize(loc, window, out).await?,
      // resolve the tail from the matched entry's start
      (_, Some(loc)) => project_one(session, &path[1..], loc.start, window, out).await?,
    }
  }
  out.push(b'}');
  Ok(())
}

async fn project_one(
  session: &Session,
  path: &[Segment],
  child_start: u64,
  window: &mut ChunkWindow,
  out: &mut Vec<u8>,
) -> Result<(), SessionError> {
  match session.run_resolve_direct(path, child_start, window).await {
    Ok(Some(loc)) => session.materialize(loc, window, out).await,
    // absent sub-path or shape mismatch projects to null (lossy, not a filter)
    Ok(None) | Err(SessionError::Path(_)) => {
      out.extend_from_slice(b"null");
      Ok(())
    }
    Err(e) => Err(e),
  }
}

fn emit_json_string(out: &mut Vec<u8>, key: &str) {
  serde_json::to_writer(out, key).expect("serializing a &str to JSON is infallible");
}

/// Whether any still-unmatched field is addressed by a member name (so the
/// entry's raw key span is worth fetching).
fn wants_member(matched: &[Option<ValueLocation>], fields: &[(String, Vec<Segment>)]) -> bool {
  matched
    .iter()
    .zip(fields)
    .any(|(slot, (_, p))| slot.is_none() && matches!(p.first(), Some(Segment::Member(_))))
}
