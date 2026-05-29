//! Per-child projection of compiled IR against the source, via the session.
//!
//! Sits *above* [`Session`]: it drives the session's resolver/reader to apply
//! a compiled [`CompiledSelect`] (projection) to one child at `child_start`.
//! Kept out of the IR module (`select`) so it stays pure data below the session.

use std::collections::HashMap;

use crate::cache::ChunkRef;
use crate::path::Segment;
use crate::resolve::{ChildEntry, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};

/// Project a child into its yielded value per `select`: a single sub-path
/// yields the bare sub-value; a map yields an object of named sub-values in
/// declared order. A missing sub-path yields `null` (projection is lossy,
/// not a filter). Only the projected `[start, end)` bytes materialize - the
/// rest of the child never does.
pub async fn project(
  session: &Session,
  select: &CompiledSelect,
  child_start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<serde_json::Value, SessionError> {
  match select {
    CompiledSelect::One(path) => project_one(session, path, child_start, pinned).await,
    CompiledSelect::Map(fields) => project_map(session, fields, child_start, pinned).await,
  }
}

async fn project_map(
  session: &Session,
  fields: &[(String, Vec<Segment>)],
  child_start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<serde_json::Value, SessionError> {
  let mut matched: Vec<Option<ValueLocation>> = vec![None; fields.len()];
  let mut remaining = fields.iter().filter(|(_, p)| !p.is_empty()).count();

  if remaining > 0 {
    if let Some(mut cw) = session.enter_container(child_start, pinned).await? {
      while remaining > 0 {
        let Some(entry) = session.next_child(&mut cw, pinned).await? else {
          break; // container exhausted; any still-unmatched fields stay null
        };
        for (slot, (_, path)) in matched.iter_mut().zip(fields) {
          if slot.is_none() && path.first().is_some_and(|seg| segment_matches(seg, &entry)) {
            *slot = Some(entry.location());
            remaining -= 1;
          }
        }
        session.prune_frontier_and_sync(pinned, cw.next_offset);
      }
    }
  }

  let mut obj = serde_json::Map::new();
  for ((key, path), loc) in fields.iter().zip(matched) {
    let value = match (path.len(), loc) {
      // Defensive: the facade rejects empty sub-paths; treat as the whole child.
      (0, _) => project_one(session, path, child_start, pinned).await?,
      (_, None) => serde_json::Value::Null,
      // Single segment: the matched entry's location is the value itself.
      (1, Some(loc)) => session.materialize(loc, pinned).await?,
      // Deeper sub-path: resolve the tail from the matched entry's start.
      (_, Some(loc)) => project_one(session, &path[1..], loc.start, pinned).await?,
    };
    obj.insert(key.clone(), value);
  }
  Ok(serde_json::Value::Object(obj))
}

async fn project_one(
  session: &Session,
  path: &[Segment],
  child_start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<serde_json::Value, SessionError> {
  match session.run_resolve(path, child_start, pinned).await? {
    None => Ok(serde_json::Value::Null),
    Some(loc) => session.materialize(loc, pinned).await,
  }
}

fn segment_matches(seg: &Segment, entry: &ChildEntry) -> bool {
  match (seg, entry) {
    (Segment::Member(name), ChildEntry::Member { key, .. }) => key == name,
    (Segment::Element(idx), ChildEntry::Element { index, .. }) => index == idx,
    _ => false,
  }
}
