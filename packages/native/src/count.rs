//! Child-counting for `count`. Sits above [`Session`] in the operations
//! layer alongside [`crate::eval`].
//!
//! [`at`] is the entry point. Two strategies, both bounded in document size:
//!   - [`children`]: no predicate - a depth-0 comma popcount over the
//!     container bytes (the same scan `step_array` uses), driven by the
//!     resumable [`count_step`] state machine.
//!   - [`matching`]: with a predicate - resolve each child and test it via
//!     [`crate::eval::matches`], advancing one element at a time so pins
//!     prune behind the frontier.

use std::collections::HashMap;

use crate::cache::ChunkRef;
use crate::eval;
use crate::pointer::JsonPointer;
use crate::predicate::CompiledPredicate;
use crate::resolve::Children;
use crate::session::{Query, Session, SessionError, MAX_BURST};
use crate::walker::{AdvanceCommas, ChunkBytes, TraverseError, Walker};

/// Count the children of the container `pointer_str` resolves to, with no
/// materialization. A missing pointer or a non-container value is `0` (total
/// and non-throwing, like `has`). With `pred`, only matching children count.
pub async fn at(
  session: &Session,
  pointer_str: &str,
  anchor_start: u64,
  pred: Option<&CompiledPredicate>,
) -> Result<u64, SessionError> {
  let pointer = JsonPointer::parse(pointer_str)?;
  let mut q = Query::new(session);
  let Some(loc) = session.run_resolve(&pointer, anchor_start, &mut q.pinned).await? else {
    return Ok(0); // missing pointer
  };
  let Some(cw) = session.enter_container(loc, &mut q.pinned).await? else {
    return Ok(0); // not an object or array
  };
  match pred {
    None => children(session, cw.next_offset, &mut q.pinned).await,
    Some(p) => matching(session, cw, p, &mut q.pinned).await,
  }
}

/// Count the children of the container whose body starts at `start` (the byte
/// just past the opening `{`/`[`) via comma popcount, with no materialization.
async fn children(
  session: &Session,
  start: u64,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<u64, SessionError> {
  let mut state = CountState::new(start);
  let mut burst = 1u64;
  session
    .drive(
      pinned,
      |_| {
        let n = burst;
        burst = burst.saturating_mul(2).min(MAX_BURST);
        n
      },
      |walker| count_step(walker, &mut state),
    )
    .await
}

/// Count the children matching `pred`. Unlike [`children`] there is no
/// comma-popcount shortcut - each child is resolved against the predicate.
/// Memory stays bounded because `next_child` advances one chunk at a time,
/// pruning pins behind the frontier as it goes.
async fn matching(
  session: &Session,
  mut cw: Children,
  pred: &CompiledPredicate,
  pinned: &mut HashMap<u64, ChunkRef>,
) -> Result<u64, SessionError> {
  let mut n = 0u64;
  loop {
    let Some(child) = session.next_child(&mut cw, pinned).await? else {
      return Ok(n);
    };
    if eval::matches(session, pred, child.location().start, pinned).await? {
      n += 1;
    }
  }
}

/// Persisted across `ChunkMiss` retries while counting a container's
/// children, so a chunk fault mid-count resumes at the last committed chunk
/// boundary instead of recounting from the container's start.
struct CountState {
  /// Next byte to scan. Before the peek, the byte just past the opening
  /// `{`/`[`; after, a chunk-boundary commit point from `Partial`.
  offset: u64,
  /// Nesting depth at `offset`, relative to the container being counted.
  depth: u32,
  /// Depth-0 commas counted so far across resumes.
  consumed: u64,
  /// Set once the container is confirmed non-empty (peeked past the opener
  /// and did not immediately hit the matching close).
  peeked: bool,
}

impl CountState {
  fn new(start: u64) -> Self {
    Self {
      offset: start,
      depth: 0,
      consumed: 0,
      peeked: false,
    }
  }
}

/// Sync step for [`children`]: returns the final child count once the
/// container's close is reached, or surfaces `ChunkMiss` (via `?`) to fault
/// the next chunk. `state` carries progress across faults.
fn count_step<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  state: &mut CountState,
) -> Result<u64, TraverseError> {
  if !state.peeked {
    // Empty-container short-circuit: a `}`/`]` immediately after the opener
    // means zero children (no value starts with a close). Commit the
    // whitespace-skipped offset before `byte_at` so a fault doesn't re-skip.
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
    match walker.advance_top_level_commas(state.offset, state.depth, usize::MAX)? {
      // Non-empty container: child count is depth-0 commas + 1.
      AdvanceCommas::ArrayClosed { consumed } => return Ok(state.consumed + consumed as u64 + 1),
      // Commit progress; the next call faults the unloaded chunk via `ensure`,
      // surfacing ChunkMiss to the drive loop so it resumes from here.
      AdvanceCommas::Partial {
        offset,
        depth,
        consumed,
      } => {
        state.consumed += consumed as u64;
        state.offset = offset;
        state.depth = depth;
      }
      // Unreachable with `needed == usize::MAX` (the count never bottoms out),
      // but stay total: keep scanning from past the comma.
      AdvanceCommas::Found {
        offset_after_comma,
        consumed,
      } => {
        state.consumed += consumed as u64;
        state.offset = offset_after_comma;
        state.depth = 0;
      }
    }
  }
}
