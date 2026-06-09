//! Per-child projection of compiled IR against the source, via the session.
//!
//! Sits *above* [`Session`] and outside the IR module (`select`), so `select`
//! stays pure data below the session.

use crate::chunks::ChunkWindow;
use crate::path::Segment;
use crate::resolve::{ChildEntry, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};

/// Project a child into its yielded value per `select`: a single sub-path
/// yields the bare sub-value; a map yields an object of named sub-values in
/// declared order. A missing sub-path yields `null` (projection is lossy,
/// not a filter). Only the projected bytes materialize.
pub async fn project(
  session: &Session,
  select: &CompiledSelect,
  child_start: u64,
  base_depth: u32,
  window: &mut ChunkWindow,
) -> Result<Vec<u8>, SessionError> {
  match select {
    CompiledSelect::One(path) => project_one(session, path, child_start, base_depth, window).await,
    CompiledSelect::Map(fields) => {
      project_map(session, fields, child_start, base_depth, window).await
    }
  }
}

async fn project_map(
  session: &Session,
  fields: &[(String, Vec<Segment>)],
  child_start: u64,
  base_depth: u32,
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
        for (slot, (_, path)) in matched.iter_mut().zip(fields) {
          if slot.is_none() && path.first().is_some_and(|seg| segment_matches(seg, &entry)) {
            *slot = Some(entry.location());
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
        let v = project_one(session, path, child_start, base_depth, window).await?;
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
        let v = project_one(session, &path[1..], loc.start, base_depth + 1, window).await?;
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
  base_depth: u32,
  window: &mut ChunkWindow,
) -> Result<Vec<u8>, SessionError> {
  match session
    .run_resolve(path, child_start, base_depth, window)
    .await
  {
    Ok(None) => Ok(b"null".to_vec()),
    Ok(Some(loc)) => session.materialize(loc, window).await,
    Err(SessionError::Path(_)) => Ok(b"null".to_vec()),
    Err(e) => Err(e),
  }
}

fn emit_json_string(out: &mut Vec<u8>, key: &str) {
  serde_json::to_writer(out, key).expect("serializing a &str to JSON is infallible");
}

fn segment_matches(seg: &Segment, entry: &ChildEntry) -> bool {
  match (seg, entry) {
    (Segment::Member(name), ChildEntry::Member { key, .. }) => key == name,
    (Segment::Element(idx), ChildEntry::Element { index, .. }) => index == idx,
    _ => false,
  }
}
