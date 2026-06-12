//! Per-child projection of compiled IR against the source, via the session.
//!
//! Sits *above* [`Session`] and outside the IR module (`select`), so `select`
//! stays pure data below the session.

use crate::chunks::ChunkWindow;
use crate::path::Segment;
use crate::resolve::{quoted_string_eq, ChildKey, ValueLocation};
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
) -> Result<Vec<u8>, SessionError> {
  match select {
    CompiledSelect::One(path) => project_one(session, path, child_start, window).await,
    CompiledSelect::Map(fields) => project_map(session, fields, child_start, window).await,
  }
}

async fn project_map(
  session: &Session,
  fields: &[(String, Vec<Segment>)],
  child_start: u64,
  window: &mut ChunkWindow,
) -> Result<Vec<u8>, SessionError> {
  let mut matched: Vec<Option<ValueLocation>> = vec![None; fields.len()];
  let mut remaining = fields.iter().filter(|(_, p)| !p.is_empty()).count();

  if remaining > 0 {
    if let Some(mut cursor) = session.enter_container(child_start, window).await? {
      while remaining > 0 {
        let Some(entry) = session.next_child(&mut cursor, window).await? else {
          break; // unmatched fields stay null
        };
        let raw_key: Option<Vec<u8>> = match entry.key {
          ChildKey::Member { start, close } if wants_member(&matched, fields) => Some(
            session
              .materialize(
                ValueLocation {
                  start,
                  end: close + 1,
                },
                window,
              )
              .await?,
          ),
          _ => None,
        };
        for (slot, (_, path)) in matched.iter_mut().zip(fields) {
          if slot.is_some() {
            continue;
          }
          let hit = match (path.first(), entry.key) {
            (Some(Segment::Member(name)), ChildKey::Member { start, .. }) => {
              let raw = raw_key
                .as_deref()
                .expect("fetched for unmatched member fields");
              quoted_string_eq(raw, name).map_err(|()| TraverseError::Malformed(start))?
            }
            (Some(Segment::Element(idx)), ChildKey::Index(index)) => *idx == index,
            _ => false,
          };
          if hit {
            *slot = Some(entry.location);
            remaining -= 1;
          }
        }
        session.prune_window(window, cursor.next_offset);
      }
    }
  }

  let mut out = Vec::new();
  out.push(b'{');
  for (i, ((key, path), loc)) in fields.iter().zip(matched).enumerate() {
    if i > 0 {
      out.push(b',');
    }
    emit_json_string(&mut out, key);
    out.push(b':');
    match (path.len(), loc) {
      // defensive: facade rejects empty sub-paths; treat as the whole child
      (0, _) => {
        let v = project_one(session, path, child_start, window).await?;
        out.extend_from_slice(&v);
      }
      (_, None) => out.extend_from_slice(b"null"),
      // single segment: the matched entry is already the value
      (1, Some(loc)) => {
        let v = session.materialize(loc, window).await?;
        out.extend_from_slice(&v);
      }
      // resolve the tail from the matched entry's start
      (_, Some(loc)) => {
        let v = project_one(session, &path[1..], loc.start, window).await?;
        out.extend_from_slice(&v);
      }
    }
  }
  out.push(b'}');
  Ok(out)
}

async fn project_one(
  session: &Session,
  path: &[Segment],
  child_start: u64,
  window: &mut ChunkWindow,
) -> Result<Vec<u8>, SessionError> {
  match session.run_resolve_direct(path, child_start, window).await {
    Ok(None) => Ok(b"null".to_vec()),
    Ok(Some(loc)) => session.materialize(loc, window).await,
    Err(SessionError::Path(_)) => Ok(b"null".to_vec()),
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
