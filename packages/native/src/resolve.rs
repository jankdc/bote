//! JSON Pointer evaluator.
//!
//! Walks a parsed [`JsonPointer`] over a bitmap-driven [`Walker`], descending
//! one reference token at a time into objects (by key) and arrays (by
//! index). Returns the byte range covering the resolved value or `None`,
//! when any token along the path doesn't address an existing member.

use crate::pointer::{token_as_array_index, JsonPointer};
use crate::walker::{AdvanceCommas, ChunkBytes, TraverseError, Walker};

/// Byte range `[start, end)` covering a JSON value in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueLocation {
  pub start: u64,
  pub end: u64,
}

/// Per-query resolver state. Persisted across `ChunkMiss` retries so a long
/// array walk doesn't restart from the anchor every time a new chunk is
/// faulted in - see [`resolve_step`].
#[derive(Debug, Clone)]
pub struct ResolveState {
  /// 0-based index of the pointer token we're currently processing.
  /// Reaches `pointer.tokens().len()` once all tokens are resolved.
  token_idx: usize,
  /// Byte offset where the current token's value starts. Before descending
  /// this holds the offset of `{` or `[`; after descending into a member
  /// it points at the member value's first byte.
  start: u64,
  /// Per-token scan state. `None` before descending into a container or
  /// after a token has been fully resolved.
  loop_state: Option<LoopState>,
}

#[derive(Debug, Clone)]
enum LoopState {
  Object(ObjectLoopState),
  Array(ArrayLoopState),
}

#[derive(Debug, Clone)]
struct ObjectLoopState {
  /// Byte offset where the next iteration's key scan begins.
  offset: u64,
}

#[derive(Debug, Clone)]
struct ArrayLoopState {
  /// Byte offset where the next element scan begins.
  offset: u64,
  /// Index of the next element to be considered.
  index: usize,
  /// Container-nesting depth at `offset`, relative to the array we entered.
  /// Used by the comma-bitmap fast path so a `ChunkMiss` mid-scan can
  /// resume without losing depth state. Always 0 at iteration boundaries
  /// the slow path sets up; the fast path may commit mid-nesting at a
  /// chunk boundary.
  depth: u32,
}

impl ResolveState {
  pub fn new(start: u64) -> Self {
    Self {
      token_idx: 0,
      start,
      loop_state: None,
    }
  }
}

/// Drive the resolver forward against the current `state`.
pub fn resolve_step<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  pointer: &JsonPointer,
  state: &mut ResolveState,
) -> Result<Option<u64>, TraverseError> {
  let tokens = pointer.tokens();
  while state.token_idx < tokens.len() {
    if state.loop_state.is_none() {
      // First entry into this token - figure out the container kind. Commit
      // `state.start` to the skipped-whitespace position before the byte
      // fetch so a `ChunkMiss` from `byte_at` doesn't re-skip on retry.
      let s = walker.skip_whitespace(state.start)?;
      state.start = s;
      let b = walker.byte_at(s)?.ok_or(TraverseError::UnexpectedEof(s))?;
      match b {
        b'{' => {
          state.loop_state = Some(LoopState::Object(ObjectLoopState { offset: s + 1 }));
        }
        b'[' => {
          state.loop_state = Some(LoopState::Array(ArrayLoopState {
            offset: s + 1,
            index: 0,
            depth: 0,
          }));
        }
        _ => return Ok(None),
      }
    }
    let token = &tokens[state.token_idx];
    let descend = match state.loop_state.as_mut().expect("set just above") {
      LoopState::Object(o) => step_object(walker, token, o)?,
      LoopState::Array(a) => step_array(walker, token, a)?,
    };
    match descend {
      Some(value_start) => {
        state.start = value_start;
        state.token_idx += 1;
        state.loop_state = None;
      }
      None => return Ok(None),
    }
  }
  Ok(Some(state.start))
}

/// Advance an object scan, updating `state.offset` only after each fully
/// successful iteration. A `ChunkMiss` mid-iteration leaves `state` at the
/// previous iteration's boundary, so resumption redoes at most one key.
fn step_object<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  target: &str,
  state: &mut ObjectLoopState,
) -> Result<Option<u64>, TraverseError> {
  loop {
    let iter_offset = state.offset;
    let offset = walker.skip_whitespace(iter_offset)?;
    match walker.byte_at(offset)? {
      None => return Err(TraverseError::UnexpectedEof(offset)),
      Some(b'}') => return Ok(None),
      Some(b'"') => {}
      Some(_) => return Err(TraverseError::Malformed(offset)),
    }
    let key_close = walker
      .next_string_close(offset + 1)?
      .ok_or(TraverseError::UnexpectedEof(offset))?;

    // fast path: JSON escapes (`\n`, `\"`, `\uXXXX`, …) only
    // ever *shrink* a string's byte count, so if the raw byte span between
    // the quotes is shorter than the pointer target, no decoding can make
    // them equal - skip the `read_range` allocation entirely. When lengths
    // could match, peek for a backslash: with none, the raw bytes equal the
    // decoded key and we byte-compare; otherwise fall through to a real
    // serde decode.
    let raw_len = (key_close - offset).saturating_sub(1) as usize;
    let target_bytes = target.as_bytes();
    let matches = if raw_len < target_bytes.len() {
      false
    } else {
      let raw = walker.read_range(offset, key_close + 1)?;
      let inner = &raw[1..raw.len() - 1];
      if !inner.contains(&b'\\') {
        inner == target_bytes
      } else {
        let decoded: String =
          serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(offset))?;
        decoded == target
      }
    };
    let post_key = walker.skip_whitespace(key_close + 1)?;
    if walker.byte_at(post_key)? != Some(b':') {
      return Err(TraverseError::Malformed(post_key));
    }
    let value_start = walker.skip_whitespace(post_key + 1)?;
    if matches {
      return Ok(Some(value_start));
    }
    let value_end = walker.skip_value(value_start)?;
    let after = walker.skip_whitespace(value_end)?;
    match walker.byte_at(after)? {
      Some(b',') => state.offset = after + 1,
      Some(b'}') => return Ok(None),
      _ => return Err(TraverseError::Malformed(after)),
    }
  }
}

/// Advance an array scan, updating `state.offset` / `state.index` only
/// after each fully successful iteration.
fn step_array<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  target: &str,
  state: &mut ArrayLoopState,
) -> Result<Option<u64>, TraverseError> {
  let Some(target_index) = token_as_array_index(target) else {
    return Ok(None);
  };

  // Fast path: jump to the target element by counting depth-0 commas in
  // the comma bitmap, skipping per-element skip_value calls.
  while state.index < target_index {
    let needed = target_index - state.index;
    match walker.advance_top_level_commas(state.offset, state.depth, needed)? {
      AdvanceCommas::Found {
        offset_after_comma,
        consumed,
      } => {
        state.offset = offset_after_comma;
        state.index += consumed;
        state.depth = 0;
      }
      AdvanceCommas::ArrayClosed { consumed: _ } => {
        // The array ended before the target index existed.
        return Ok(None);
      }
      AdvanceCommas::Partial {
        offset,
        depth,
        consumed,
      } => {
        state.offset = offset;
        state.index += consumed;
        state.depth = depth;
        // Loop: next iteration will re-enter the fast path. If the next
        // chunk is now unresident the call itself surfaces ChunkMiss
        // via `ensure`, which propagates to the session retry loop.
      }
    }
  }
  loop {
    let iter_offset = state.offset;
    let iter_index = state.index;
    let offset = walker.skip_whitespace(iter_offset)?;
    match walker.byte_at(offset)? {
      None => return Err(TraverseError::UnexpectedEof(offset)),
      Some(b']') => return Ok(None),
      _ => {}
    }
    if iter_index == target_index {
      return Ok(Some(offset));
    }
    let value_end = walker.skip_value(offset)?;
    let after = walker.skip_whitespace(value_end)?;
    match walker.byte_at(after)? {
      Some(b',') => {
        state.offset = after + 1;
        state.index = iter_index + 1;
      }
      Some(b']') => return Ok(None),
      _ => return Err(TraverseError::Malformed(after)),
    }
  }
}

/// Kind of JSON container being iterated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
  Object,
  Array,
}

/// Cursor over the children of an object or array value. Created from
/// [`enter_container`] and advanced one entry at a time by [`next_child`].
#[derive(Debug, Clone)]
pub struct Children {
  pub kind: ContainerKind,
  pub next_offset: u64,
  pub index: usize,
  /// Set once the container's closing `}`/`]` has been reached, so repeated
  /// `next_child` calls return `None` idempotently instead of trying to parse
  /// whatever follows the container (which would be malformed).
  done: bool,
}

/// One yielded child of a container.
#[derive(Debug, Clone)]
pub enum ChildEntry {
  /// Object member: decoded key plus the byte range of the value.
  Member {
    key: String,
    location: ValueLocation,
  },
  /// Array element: zero-based index plus the byte range of the value.
  Element {
    index: usize,
    location: ValueLocation,
  },
}

impl ChildEntry {
  pub fn location(&self) -> ValueLocation {
    match self {
      Self::Member { location, .. } | Self::Element { location, .. } => *location,
    }
  }
}

/// Position a [`Children`] at the first child of the container that begins
/// at `value_start`. Returns `Ok(None)` if the value isn't a container.
///
/// Takes a bare start offset (not a [`ValueLocation`]) because container
/// iteration doesn't need the value's end - skipping the closing brace is
/// what iteration *does*. Callers that have a `ValueLocation` should pass
/// `loc.start`.
pub fn enter_container<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  value_start: u64,
) -> Result<Option<Children>, TraverseError> {
  let open = walker.skip_whitespace(value_start)?;
  let byte = walker
    .byte_at(open)?
    .ok_or(TraverseError::UnexpectedEof(open))?;
  let kind = match byte {
    b'{' => ContainerKind::Object,
    b'[' => ContainerKind::Array,
    _ => return Ok(None),
  };
  Ok(Some(Children {
    kind,
    next_offset: open + 1,
    index: 0,
    done: false,
  }))
}

/// Advance `cw` to the next child entry. Returns `Ok(None)` when the
/// container is exhausted (closing `}` or `]` reached).
pub fn next_child<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  if cw.done {
    return Ok(None);
  }
  let entry = match cw.kind {
    ContainerKind::Object => next_object_member(walker, cw)?,
    ContainerKind::Array => next_array_element(walker, cw)?,
  };
  if entry.is_none() {
    cw.done = true;
  }
  Ok(entry)
}

fn next_object_member<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cw.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b'}') => {
      cw.next_offset = offset + 1;
      return Ok(None);
    }
    Some(b'"') => {}
    Some(_) => return Err(TraverseError::Malformed(offset)),
  }
  let key_close = walker
    .next_string_close(offset + 1)?
    .ok_or(TraverseError::UnexpectedEof(offset))?;
  let raw = walker.read_range(offset, key_close + 1)?;
  let key: String = serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(offset))?;
  let post_key = walker.skip_whitespace(key_close + 1)?;
  if walker.byte_at(post_key)? != Some(b':') {
    return Err(TraverseError::Malformed(post_key));
  }
  let value_start = walker.skip_whitespace(post_key + 1)?;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cw.next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b'}') => after, // next call lands on `}` and terminates
    Some(_) | None => return Err(TraverseError::Malformed(after)),
  };
  Ok(Some(ChildEntry::Member {
    key,
    location: ValueLocation {
      start: value_start,
      end: value_end,
    },
  }))
}

fn next_array_element<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cw.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b']') => {
      cw.next_offset = offset + 1;
      return Ok(None);
    }
    _ => {}
  }
  let value_start = offset;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cw.next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b']') => after,
    Some(_) | None => return Err(TraverseError::Malformed(after)),
  };
  let index = cw.index;
  cw.index += 1;
  Ok(Some(ChildEntry::Element {
    index,
    location: ValueLocation {
      start: value_start,
      end: value_end,
    },
  }))
}
