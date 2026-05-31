//! Path evaluator.
//!
//! Walks a [`&[Segment]`] over a scan-aligned [`Walker`], descending one segment
//! at a time into objects (by member name) and arrays (by index). Returns the
//! byte range covering the resolved value or `None`, when any segment along the
//! path doesn't address an existing member.

use crate::path::Segment;
use crate::simd::ScanCarry;
use crate::walker::{AdvanceCommas, TraverseError, Walker};

/// Byte range `[start, end)` covering a JSON value in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueLocation {
  pub start: u64,
  pub end: u64,
}

/// Per-query resolver state. Persisted across `ChunkMiss` retries so a long
/// array walk doesn't restart from the anchor every time a new chunk is faulted
/// in - see [`resolve_step`].
#[derive(Debug, Clone)]
pub struct ResolveState {
  /// 0-based index of the segment we're currently processing. Reaches
  /// `path.len()` once all segments are resolved.
  segment_idx: usize,
  /// Byte offset where the current segment's value starts.
  start: u64,
  /// Per-segment scan state. `None` before descending into a container or after
  /// a segment has been fully resolved.
  loop_state: Option<LoopState>,
}

/// Per-iteration scan state for `step_object` / `step_array`. Flattened across
/// both container kinds: object scans read only `offset`; array scans use all
/// fields (the comma-bitmap fast path needs `index`, `depth`, and `carry` so a
/// `ChunkMiss` mid-scan can resume without losing them).
#[derive(Debug, Clone)]
struct LoopState {
  kind: ContainerKind,
  /// Byte offset where the next iteration begins.
  offset: u64,
  /// Array element index considered next. Always 0 for objects.
  index: usize,
  /// Container-nesting depth at `offset`, relative to the container we entered.
  /// Always 0 for objects (unused).
  depth: u32,
  /// String-scan carry at `offset` for the array comma fast path. Default at
  /// element boundaries; the fast path may commit a mid-string carry at a block
  /// boundary. Unused for objects.
  carry: ScanCarry,
}

impl ResolveState {
  pub fn new(start: u64) -> Self {
    Self {
      segment_idx: 0,
      start,
      loop_state: None,
    }
  }

  /// Lowest offset a resumed step might still read, including the key
  /// `read_range` (which reads behind the scan frontier but never behind the
  /// current iteration's start). The retention floor for the byte window.
  pub fn floor(&self) -> u64 {
    match &self.loop_state {
      Some(ls) => ls.offset,
      None => self.start,
    }
  }
}

/// Drive the resolver forward against the current `state`.
pub fn resolve_step(
  walker: &mut Walker,
  path: &[Segment],
  state: &mut ResolveState,
) -> Result<Option<u64>, TraverseError> {
  while state.segment_idx < path.len() {
    if state.loop_state.is_none() {
      // First entry into this segment - figure out the container kind. Commit
      // `state.start` to the skipped-whitespace position before the byte fetch
      // so a `ChunkMiss` from `byte_at` doesn't re-skip on retry.
      let s = walker.skip_whitespace(state.start)?;
      state.start = s;
      let b = walker.byte_at(s)?.ok_or(TraverseError::UnexpectedEof(s))?;
      let kind = match b {
        b'{' => ContainerKind::Object,
        b'[' => ContainerKind::Array,
        _ => return Ok(None),
      };
      state.loop_state = Some(LoopState {
        kind,
        offset: s + 1,
        index: 0,
        depth: 0,
        carry: ScanCarry::default(),
      });
    }
    let segment = &path[state.segment_idx];
    let ls = state.loop_state.as_mut().expect("set just above");
    let descend = match (ls.kind, segment) {
      (ContainerKind::Object, Segment::Member(name)) => step_object(walker, name, ls)?,
      (ContainerKind::Array, Segment::Element(idx)) => step_array(walker, *idx, ls)?,
      // Type mismatch (member-name into array, index into object) is a miss, not
      // an error - mirrors RFC 6901 where `/0` against an object resolves to
      // nothing.
      _ => return Ok(None),
    };
    match descend {
      Some(value_start) => {
        state.start = value_start;
        state.segment_idx += 1;
        state.loop_state = None;
      }
      None => return Ok(None),
    }
  }
  Ok(Some(state.start))
}

/// Given the offset of a member key's closing quote, skip the `:` separator
/// (with surrounding whitespace) and return the offset of the value's first
/// byte. Shared by [`step_object`] and [`next_object_member`].
fn member_value_start(walker: &mut Walker, key_close: u64) -> Result<u64, TraverseError> {
  let post_key = walker.skip_whitespace(key_close + 1)?;
  if walker.byte_at(post_key)? != Some(b':') {
    return Err(TraverseError::Malformed(post_key));
  }
  Ok(walker.skip_whitespace(post_key + 1)?)
}

/// Advance an object scan, updating `state.offset` only after each fully
/// successful iteration. A `ChunkMiss` mid-iteration leaves `state` at the
/// previous iteration's boundary, so resumption redoes at most one key.
fn step_object(
  walker: &mut Walker,
  target: &str,
  state: &mut LoopState,
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

    // Fast path: JSON escapes only ever shrink a string's byte count, so if the
    // raw byte span between the quotes is shorter than the target name, no
    // decoding can make them equal - skip the `read_range` allocation entirely.
    let raw_len = (key_close - offset).saturating_sub(1) as usize;
    let target_bytes = target.as_bytes();
    let matches = if raw_len < target_bytes.len() {
      false
    } else {
      let raw = walker.read_range(offset, key_close + 1)?;
      quoted_string_eq(&raw, target).map_err(|()| TraverseError::Malformed(offset))?
    };
    let value_start = member_value_start(walker, key_close)?;
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

/// Compare the raw bytes of a JSON-encoded string value (including the
/// surrounding quotes) to a Rust `&str`. The hot path - interior contains no
/// backslash - byte-compares directly; escaped strings invoke
/// `serde_json::from_slice`. Returns `Err(())` when an escaped interior fails to
/// decode (malformed JSON); the caller maps that to [`TraverseError::Malformed`].
fn quoted_string_eq(value_raw: &[u8], target: &str) -> Result<bool, ()> {
  if value_raw.len() < 2 || value_raw[0] != b'"' || value_raw[value_raw.len() - 1] != b'"' {
    return Ok(false);
  }
  let interior = &value_raw[1..value_raw.len() - 1];
  if !interior.contains(&b'\\') {
    return Ok(interior == target.as_bytes());
  }
  serde_json::from_slice::<String>(value_raw)
    .map(|s| s == target)
    .map_err(|_| ())
}

/// Advance an array scan, updating `state` only after each fully successful
/// iteration.
fn step_array(
  walker: &mut Walker,
  target_index: usize,
  state: &mut LoopState,
) -> Result<Option<u64>, TraverseError> {
  // Fast path: jump to the target element by counting depth-0 commas, skipping
  // per-element skip_value calls.
  while state.index < target_index {
    let needed = target_index - state.index;
    match walker.advance_top_level_commas(state.offset, state.depth, needed, state.carry)? {
      AdvanceCommas::Found {
        offset_after_comma,
        consumed,
      } => {
        state.offset = offset_after_comma;
        state.index += consumed;
        state.depth = 0;
        // Just past a depth-0 comma is an element boundary - outside any string.
        state.carry = ScanCarry::default();
      }
      AdvanceCommas::ArrayClosed { consumed: _ } => {
        // The array ended before the target index existed.
        return Ok(None);
      }
      AdvanceCommas::Partial {
        offset,
        depth,
        consumed,
        carry,
      } => {
        state.offset = offset;
        state.index += consumed;
        state.depth = depth;
        state.carry = carry;
        // Loop: the next iteration re-enters the fast path and faults the
        // now-needed chunk via `window64`.
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
        state.depth = 0;
        state.carry = ScanCarry::default();
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
///
/// After exhaustion `next_offset` points AT the closing `}`/`]` (not past it),
/// so repeated `next_child` calls re-run `skip_whitespace` -> `byte_at` -> see
/// the close byte -> return `None` idempotently.
#[derive(Debug, Clone)]
pub struct Children {
  pub kind: ContainerKind,
  pub next_offset: u64,
  pub index: usize,
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

/// Position a [`Children`] at the first child of the container that begins at
/// `value_start`. Returns `Ok(None)` if the value isn't a container.
pub fn enter_container(
  walker: &mut Walker,
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
  }))
}

/// Advance `cw` to the next child entry. Returns `Ok(None)` when the container
/// is exhausted. Idempotent: after exhaustion `cw.next_offset` is left AT the
/// close byte, so subsequent calls re-detect it and keep returning `None`.
pub fn next_child(
  walker: &mut Walker,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  match cw.kind {
    ContainerKind::Object => next_object_member(walker, cw),
    ContainerKind::Array => next_array_element(walker, cw),
  }
}

fn next_object_member(
  walker: &mut Walker,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cw.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b'}') => {
      cw.next_offset = offset;
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
  let value_start = member_value_start(walker, key_close)?;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cw.next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b'}') => after,
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

fn next_array_element(
  walker: &mut Walker,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cw.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b']') => {
      cw.next_offset = offset;
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
