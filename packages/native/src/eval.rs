//! Per-child projection of compiled IR against the source, via the session.
//!
//! Sits *above* [`Session`]: it drives the session's resolver/reader to apply
//! a compiled [`CompiledSelect`] (projection) to one child at `child_start`.
//! Kept out of the IR module (`select`) so it stays pure data below the session.

use std::collections::HashMap;

use crate::cache::ChunkRef;
use crate::path::Segment;
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
    CompiledSelect::Map(fields) => {
      let mut obj = serde_json::Map::new();
      for (key, path) in fields {
        let value = project_one(session, path, child_start, pinned).await?;
        obj.insert(key.clone(), value);
      }
      Ok(serde_json::Value::Object(obj))
    }
  }
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
