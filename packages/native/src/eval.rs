//! Per-child evaluation of compiled IR against the source, via the session.
//!
//! Sits *above* [`Session`]: it drives the session's resolver/reader to apply
//! a compiled [`CompiledPredicate`] (filter) or [`CompiledSelect`]
//! (projection) to one child at `child_start`. Kept out of the IR modules
//! (`predicate`, `select`) so those stay pure data below the session.

use std::collections::HashMap;

use crate::cache::ChunkRef;
use crate::pointer::JsonPointer;
use crate::predicate::CompiledPredicate;
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};

/// Evaluate `pred` against the child whose value starts at `child_start`.
/// Each leaf resolves its sub-pointer relative to `child_start` (its own
/// resumable drive) and compares off the resolved value's raw bytes; `and`
/// short-circuits on the first unsatisfied leaf. Total and non-throwing: a
/// missing sub-pointer fails the leaf, never errors.
///
/// Leaves resolve *back* to `child_start` (behind the container scan
/// frontier), so a child larger than the burst window can re-fault between
/// leaves; the doubling burst converges once it spans the child. In practice
/// predicate sub-pointers are shallow and children are small.
pub async fn matches(
  session: &Session,
  pred: &CompiledPredicate,
  child_start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<bool, SessionError> {
  for leaf in pred.leaves() {
    let Some(loc) = session.run_resolve(leaf.pointer(), child_start, pinned).await? else {
      return Ok(false);
    };
    if leaf.needs_value() {
      let raw = session.read_range(loc.start, loc.end, pinned).await?;
      if !leaf.satisfied_by(&raw) {
        return Ok(false);
      }
    }
  }
  Ok(true)
}

/// Project a matched child into its yielded value per `select`: a single
/// sub-pointer yields the bare sub-value; a map yields an object of named
/// sub-values in declared order. A missing sub-pointer yields `null`
/// (projection is lossy, not a filter). Only the projected `[start, end)`
/// bytes materialize - the rest of the child never does.
pub async fn project(
  session: &Session,
  select: &CompiledSelect,
  child_start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<serde_json::Value, SessionError> {
  match select {
    CompiledSelect::One(ptr) => project_one(session, ptr, child_start, pinned).await,
    CompiledSelect::Map(fields) => {
      let mut obj = serde_json::Map::new();
      for (key, ptr) in fields {
        let value = project_one(session, ptr, child_start, pinned).await?;
        obj.insert(key.clone(), value);
      }
      Ok(serde_json::Value::Object(obj))
    }
  }
}

async fn project_one(
  session: &Session,
  ptr: &JsonPointer,
  child_start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<serde_json::Value, SessionError> {
  match session.run_resolve(ptr, child_start, pinned).await? {
    None => Ok(serde_json::Value::Null),
    Some(loc) => session.materialize(loc, pinned).await,
  }
}
