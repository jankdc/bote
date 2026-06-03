//! Child-counting for `count`. Sits above [`Session`] in the operations layer
//! alongside [`crate::eval`].
//!
//! [`at`] is the entry point. A depth-0 comma popcount over the container bytes
//! (the same scan `step_array` uses), driven by the resumable [`count_step`]
//! state machine - bounded in document size regardless of container size.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::chunks::ChunkWindow;
use crate::path::Segment;
use crate::session::{doubling_burst, Query, Session, SessionError};
use crate::simd::ScanCarry;
use crate::walker::{CommaStop, TraverseError, Walker};

/// Count the children of the container `path` resolves to, with no
/// materialization. A missing path or a non-container value is `0` (total and
/// non-throwing, like `has`).
pub async fn at(
  session: &Session,
  path: &[Segment],
  anchor_start: u64,
  base_depth: u32,
) -> Result<u64, SessionError> {
  // O(1) on a repeat: a prior scan cached it, no chunk faults.
  if let Some(n) = session.cached_child_count(anchor_start, path) {
    return Ok(n);
  }
  let mut q = Query::new(session);
  // run_locate, not run_resolve: we only need the container's start; the count
  // comes from a per-comma scan started at the opener.
  let Some(start) = session
    .run_locate(path, anchor_start, base_depth, &mut q.window)
    .await?
  else {
    return Ok(0);
  };
  let Some(cursor) = session.enter_container(start, &mut q.window).await? else {
    return Ok(0);
  };
  let kind = cursor.kind;
  let count = children(session, cursor.next_offset, &mut q.window).await?;
  // Store the count, not the close offset: the comma scan never sees the close.
  session.store_child_count(base_depth, anchor_start, path, kind, start, count);
  Ok(count)
}

/// Count the children of the container whose body starts at `start` (the byte
/// just past the opening `{`/`[`) via comma popcount, with no materialization.
async fn children(
  session: &Session,
  start: u64,
  window: &mut ChunkWindow,
) -> Result<u64, SessionError> {
  let mut state = CountState::new(start);
  let min_reachable = AtomicU64::new(start);
  session
    .drive(window, &min_reachable, doubling_burst(), |walker| {
      let r = count_step(walker, &mut state);
      min_reachable.store(state.offset, Ordering::Relaxed);
      r
    })
    .await
}

/// Sync step for [`children`]: returns the child count once the container's
/// close is reached, or surfaces `ChunkMiss` via `?` to fault the next chunk.
/// `state` carries progress across faults.
fn count_step(walker: &mut Walker, state: &mut CountState) -> Result<u64, TraverseError> {
  if !state.peeked {
    // Empty-container short-circuit: a close right after the opener is 0
    // children. Commit the skipped offset before `byte_at` so a fault here
    // doesn't re-skip.
    let off = walker.skip_whitespace(state.offset)?;
    state.offset = off;
    match walker.byte_at(off)? {
      None => return Err(TraverseError::UnexpectedEof(off)),
      Some(b']' | b'}') => return Ok(0),
      Some(_) => {}
    }
    state.peeked = true;
  }
  loop {
    match walker.advance_top_level_commas(
      state.offset,
      state.depth,
      usize::MAX,
      state.carry,
      None,
    )? {
      // Non-empty container: child count is depth-0 commas + 1.
      CommaStop::ArrayClosed { consumed } => return Ok(state.consumed + consumed as u64 + 1),
      CommaStop::Partial {
        offset,
        depth,
        consumed,
        carry,
      } => {
        state.consumed += consumed as u64;
        state.offset = offset;
        state.depth = depth;
        state.carry = carry;
      }
      // Unreachable with `needed == usize::MAX` (the count never bottoms out),
      // but stay total: keep scanning past the comma.
      CommaStop::Found {
        offset_after_comma,
        consumed,
      } => {
        state.consumed += consumed as u64;
        state.offset = offset_after_comma;
        state.depth = 0;
        state.carry = ScanCarry::default();
      }
    }
  }
}

/// Persisted across `ChunkMiss` retries so a fault mid-count resumes at the
/// last committed block boundary instead of recounting from the container start.
struct CountState {
  /// Next byte to scan: the byte past the opener before the peek, a
  /// block-boundary commit point from `Partial` after.
  offset: u64,
  /// Nesting depth at `offset`, relative to the container being counted.
  depth: u32,
  /// String-scan carry at `offset`, threaded across `Partial` commits.
  carry: ScanCarry,
  /// Depth-0 commas counted so far across resumes.
  consumed: u64,
  /// Set once the container is confirmed non-empty.
  peeked: bool,
}

impl CountState {
  fn new(start: u64) -> Self {
    Self {
      offset: start,
      depth: 0,
      carry: ScanCarry::default(),
      consumed: 0,
      peeked: false,
    }
  }
}
