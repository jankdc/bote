//! Path evaluator.
//!
//! Walks a [`&[Segment]`] over a scan-aligned [`Walker`], descending one segment
//! at a time into objects (by member name) and arrays (by index).

use napi_derive::napi;

use crate::keys;
use crate::path::Segment;
use crate::simd::ScanCarry;
use crate::walker::{ArrayMemberSink, CommaStop, TraverseError, Walker};

/// Drive the resolver forward against the current `state`.
pub fn resolve_step(
  walker: &mut Walker,
  path: &[Segment],
  state: &mut ResolveState,
) -> Result<Resolved, TraverseError> {
  // Destructure for disjoint field borrows: `segment_scan` and `scan_record` are
  // mutated together below. The `Copy` config fields are read out first.
  let record_objects = state.record_objects;
  let array_stride = state.array_stride;
  let ResolveState {
    segment_idx,
    start,
    segment_scan,
    seed,
    scan_record,
    ..
  } = state;

  while *segment_idx < path.len() {
    let ls = match segment_scan {
      Some(ls) => ls,
      None => {
        let (kind, value_start, ls) = if let Some(hint) = seed.take() {
          // Seeded resume: kind and position come from the array member, so the open
          // byte is never read. Only the first entered container is ever seeded.
          let ls = match hint {
            ResumePoint::Object { offset } => SegmentScan {
              kind: ContainerKind::Object,
              offset,
              index: 0,
              depth: 0,
              carry: ScanCarry::default(),
            },
            ResumePoint::Array { index, offset } => SegmentScan {
              kind: ContainerKind::Array,
              offset,
              index,
              depth: 0,
              carry: ScanCarry::default(),
            },
          };
          (ls.kind, *start, ls)
        } else {
          // Cold entry. Commit `start` to the skipped-whitespace position before the
          // byte fetch so a `ChunkMiss` from `byte_at` doesn't re-skip on retry.
          let s = walker.skip_whitespace(*start)?;
          *start = s;
          let b = walker.byte_at(s)?.ok_or(TraverseError::UnexpectedEof(s))?;
          let kind = match b {
            b'{' => ContainerKind::Object,
            b'[' => ContainerKind::Array,
            _ => {
              return Ok(Resolved::NotNavigable(PathFault::ThroughScalar {
                segment: *segment_idx,
              }))
            }
          };
          let ls = SegmentScan {
            kind,
            offset: s + 1,
            index: 0,
            depth: 0,
            carry: ScanCarry::default(),
          };
          (kind, s, ls)
        };
        if let Some(h) = scan_record.as_mut() {
          h.containers.push(ContainerRecord {
            seg: *segment_idx,
            value_start,
            body: match kind {
              ContainerKind::Object => RecordBody::Object {
                members: Vec::new(),
                resume: None,
              },
              ContainerKind::Array => RecordBody::Array {
                members: Vec::new(),
              },
            },
          });
        }
        segment_scan.insert(ls)
      }
    };

    let segment = &path[*segment_idx];
    let cs = scan_record.as_mut().and_then(|h| h.containers.last_mut());
    let descend = match (ls.kind, segment) {
      // Recording is gated per kind: objects skip it (and key decode) when the
      // object cap is 0; arrays skip the array-member sink when the stride is 0.
      // The push above fixed the record's variant from the same kind, so the
      // mismatched arms are unreachable.
      (ContainerKind::Object, Segment::Member(name)) => {
        let rec = if record_objects {
          cs.map(|c| match &mut c.body {
            RecordBody::Object { members, resume } => (members, resume),
            RecordBody::Array { .. } => unreachable!("record variant fixed at push"),
          })
        } else {
          None
        };
        step_object(walker, name, ls, rec)?
      }
      (ContainerKind::Array, Segment::Element(idx)) => {
        let rec = if array_stride > 0 {
          cs.map(|c| match &mut c.body {
            RecordBody::Array { members } => members,
            RecordBody::Object { .. } => unreachable!("record variant fixed at push"),
          })
        } else {
          None
        };
        step_array(walker, *idx, ls, array_stride, rec)?
      }
      _ => {
        return Ok(Resolved::NotNavigable(PathFault::WrongKind {
          segment: *segment_idx,
        }))
      }
    };
    match descend {
      Some(vs) => {
        *start = vs;
        *segment_idx += 1;
        *segment_scan = None;
      }
      None => return Ok(Resolved::Absent),
    }
  }
  Ok(Resolved::Found(*start))
}

impl ResolveState {
  /// Start resolving `path` from segment `segment_idx` at `start` (the value
  /// start of container `path[..segment_idx]`), optionally seeded at a cached
  /// array member. Collects child offsets when `record_objects` or `array_stride > 0`.
  pub fn resume(
    start: u64,
    segment_idx: usize,
    seed: Option<ResumePoint>,
    record_objects: bool,
    array_stride: usize,
  ) -> Self {
    Self {
      segment_idx,
      start,
      segment_scan: None,
      seed,
      scan_record: (record_objects || array_stride > 0).then(ScanRecord::default),
      record_objects,
      array_stride,
    }
  }

  /// Lowest offset a resumed step might still read (a key lookup reads behind
  /// the scan position but never behind the iteration's start). The retention
  /// bound for the byte window.
  pub fn min_reachable(&self) -> u64 {
    match &self.segment_scan {
      Some(ls) => ls.offset,
      None => self.start,
    }
  }

  /// Take the collected child offsets (if caching was enabled) for write-back.
  pub fn take_scan_record(&mut self) -> Option<ScanRecord> {
    self.scan_record.take()
  }
}

/// Per-query resolver state, persisted across `ChunkMiss` retries so a long
/// array walk doesn't restart from the anchor on every faulted-in chunk.
#[derive(Debug, Clone)]
pub struct ResolveState {
  /// Segment currently being processed. Reaches `path.len()` once all resolved.
  segment_idx: usize,
  /// Byte offset where the current segment's value starts.
  start: u64,
  /// `None` before descending into a container or after a segment is resolved.
  segment_scan: Option<SegmentScan>,
  /// Cache seed for the first container entered; consumed when its `segment_scan`
  /// is created. `None` for a cold resolve.
  seed: Option<ResumePoint>,
  /// Child offsets collected for the cache, or `None` when caching is disabled
  /// (no per-member decode/alloc on the hot path).
  scan_record: Option<ScanRecord>,
  /// Whether object members are tabled (object cap `> 0`). When `false`, object
  /// key decoding is skipped on the hot path even while arrays are collecting.
  record_objects: bool,
  /// Element-index stride for array-member sampling (`0` = no array members).
  array_stride: usize,
}

/// A saved position a re-entering scan jumps to instead of rescanning from the
/// container's open. Stored in the structural-index cache, consumed as a seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePoint {
  /// Object: resume the member scan from this high-water offset.
  Object { offset: u64 },
  /// Array: resume the comma popcount from element `index` at `offset`.
  Array { index: usize, offset: u64 },
}

/// Child offsets collected for the structural-index cache, one
/// [`ContainerRecord`] per container the resolver entered.
#[derive(Debug, Clone, Default)]
pub struct ScanRecord {
  pub containers: Vec<ContainerRecord>,
}

/// What one entered container yielded during a scan.
#[derive(Debug, Clone)]
pub struct ContainerRecord {
  /// The container is `path[..seg]` from the scan's anchor.
  pub seg: usize,
  /// Offset of the container's `{`/`[`.
  pub value_start: u64,
  pub body: RecordBody,
}

/// Kind-specific payload of a [`ContainerRecord`], mirroring the cache's
/// object/array body split.
#[derive(Debug, Clone)]
pub enum RecordBody {
  Object {
    /// Members seen, in scan order: `(name, key_start, value_start)`.
    members: Vec<(Box<str>, u64, u64)>,
    /// High-water resume offset - the matched member's start, or the close `}`.
    resume: Option<u64>,
  },
  Array {
    /// `(index, element_offset)` array members (one per stride multiple) plus
    /// the resolved target, in ascending index order.
    members: Vec<(usize, u64)>,
  },
}

/// Byte range `[start, end)` covering a JSON value in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueLocation {
  pub start: u64,
  pub end: u64,
}

/// Position a [`ContainerCursor`] at the first child of the container that begins at
/// `value_start`. Returns `Ok(None)` if the value isn't a container.
pub fn enter_container(
  walker: &mut Walker,
  value_start: u64,
) -> Result<Option<ContainerCursor>, TraverseError> {
  let open = walker.skip_whitespace(value_start)?;
  let byte = walker
    .byte_at(open)?
    .ok_or(TraverseError::UnexpectedEof(open))?;
  let kind = match byte {
    b'{' => ContainerKind::Object,
    b'[' => ContainerKind::Array,
    _ => return Ok(None),
  };
  Ok(Some(ContainerCursor {
    kind,
    next_offset: open + 1,
    index: 0,
  }))
}

/// Advance `cursor` to the next child entry, or `Ok(None)` when exhausted.
/// Idempotent past exhaustion (see [`ContainerCursor`]).
pub fn next_child(
  walker: &mut Walker,
  cursor: &mut ContainerCursor,
) -> Result<Option<ChildEntry>, TraverseError> {
  match cursor.kind {
    ContainerKind::Object => next_object_member(walker, cursor),
    ContainerKind::Array => next_array_element(walker, cursor),
  }
}

/// Kind of JSON container being iterated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
  Object,
  Array,
}

/// Cursor over the children of an object or array. Created by [`enter_container`],
/// advanced one entry at a time by [`next_child`].
///
/// After exhaustion `next_offset` points AT the closing `}`/`]` (not past it), so
/// repeated `next_child` calls re-see the close byte and return `None` idempotently.
#[derive(Debug, Clone)]
pub struct ContainerCursor {
  pub kind: ContainerKind,
  pub next_offset: u64,
  pub index: usize,
}

impl ContainerCursor {
  /// Offset just past the container's closing `}`/`]`. Valid only after
  /// [`next_child`] has returned `None` - mid-iteration `next_offset` points at
  /// the next child, not the close.
  pub fn close_offset(&self) -> u64 {
    self.next_offset + 1
  }
}

/// One yielded child of a container.
#[derive(Debug)]
pub struct ChildEntry {
  pub key: ChildKey,
  /// Byte range of the child's value.
  pub location: ValueLocation,
}

/// How a child is addressed within its container. Object keys are carried as
/// their raw source span - consumers that need the name fetch and decode it;
/// nothing here validates the key bytes.
#[derive(Debug, Clone, Copy)]
pub enum ChildKey {
  /// Object member: span of the key string, opening quote through closing quote.
  Member { start: u64, close: u64 },
  /// Array element: zero-based index.
  Index(usize),
}

/// Advance an array scan, committing `state` only after a fully successful
/// iteration.
fn step_array(
  walker: &mut Walker,
  target_index: usize,
  state: &mut SegmentScan,
  array_stride: usize,
  mut members: Option<&mut Vec<(usize, u64)>>,
) -> Result<Option<u64>, TraverseError> {
  // Fast path: jump to the target element by counting depth-0 commas, skipping
  // per-element skip_value calls. When collecting, an `ArrayMemberSink` samples
  // an array member per stride multiple; the stride grid is absolute, so sampling
  // is resume-safe.
  while state.index < target_index {
    let needed = target_index - state.index;
    let base_index = state.index;
    let from = state.offset;
    let depth = state.depth;
    let carry = state.carry;
    let stop = {
      let sink = members.as_deref_mut().map(|out| ArrayMemberSink {
        base_index,
        stride: array_stride,
        out,
      });
      walker.advance_top_level_commas(from, depth, needed, carry, sink)?
    };
    match stop {
      CommaStop::Found {
        offset_after_comma,
        consumed,
      } => {
        state.offset = offset_after_comma;
        state.index += consumed;
        state.depth = 0;
        // Just past a depth-0 comma is an element boundary - outside any string.
        state.carry = ScanCarry::default();
      }
      CommaStop::ContainerClosed { consumed: _ } => {
        // Array ended before the target index existed.
        return Ok(None);
      }
      CommaStop::Partial {
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
        // now-needed chunk.
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
      if let Some(out) = members.as_deref_mut() {
        out.push((target_index, offset));
      }
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

/// Mutable view into a [`RecordBody::Object`]'s fields, handed to
/// [`step_object`] while recording: `(members, resume)`.
type ObjectRecord<'a> = (&'a mut Vec<(Box<str>, u64, u64)>, &'a mut Option<u64>);

/// Advance an object scan, committing `state.offset` only after a fully
/// successful iteration, so a mid-iteration `ChunkMiss` redoes at most one key.
fn step_object(
  walker: &mut Walker,
  target: &str,
  state: &mut SegmentScan,
  mut rec: Option<ObjectRecord>,
) -> Result<Option<u64>, TraverseError> {
  let collecting = rec.is_some();
  loop {
    let iter_offset = state.offset;
    let offset = walker.skip_whitespace(iter_offset)?;
    match walker.byte_at(offset)? {
      None => return Err(TraverseError::UnexpectedEof(offset)),
      Some(b'}') => {
        if let Some((_, resume)) = rec.as_mut() {
          **resume = Some(offset);
        }
        return Ok(None);
      }
      Some(b'"') => {}
      Some(_) => return Err(TraverseError::Malformed(offset)),
    }
    let key_close = walker
      .next_string_close(offset + 1)?
      .ok_or(TraverseError::UnexpectedEof(offset))?;

    // The cache table needs the decoded key name, so collecting decodes every
    // member. A plain lookup just compares the key against the target, borrowing
    // its interior bytes in place (`read_slice` copies only across a chunk seam)
    // and decoding solely when an escape is present.
    let mut decoded: Option<String> = None;
    let matches = {
      let key = walker.read_slice(offset + 1, key_close)?;
      if collecting {
        let name = if keys::is_escaped(&key) {
          keys::decode_escaped(&key)
        } else {
          keys::decode_simple(&key)
        }
        .map_err(|()| TraverseError::Malformed(offset))?;

        let m = name == target;
        decoded = Some(name);
        m
      } else {
        keys::compare(&key, target).map_err(|()| TraverseError::Malformed(offset))?
      }
    };

    let value_start = member_value_start(walker, key_close)?;
    if matches {
      // Match commit point: no fallible call follows, so recording here is never
      // redone by a retry.
      if let Some((members, resume)) = rec.as_mut() {
        if let Some(name) = decoded.take() {
          members.push((name.into(), iter_offset, value_start));
        }
        **resume = Some(iter_offset);
      }
      return Ok(Some(value_start));
    }
    let value_end = walker.skip_value(value_start)?;
    let after = walker.skip_whitespace(value_end)?;

    match walker.byte_at(after)? {
      Some(b',') => {
        // Skip commit point: `state` advances past this member. A mid-iteration
        // fault leaves `state` at `iter_offset` recording nothing, so the retry
        // re-records exactly once.
        state.offset = after + 1;
        if let Some((members, _)) = rec.as_mut() {
          if let Some(name) = decoded.take() {
            members.push((name.into(), iter_offset, value_start));
          }
        }
      }
      Some(b'}') => {
        // Last member (no trailing comma): record it before the resume point
        // advances to the close, else the resume point would skip past it.
        if let Some((members, resume)) = rec.as_mut() {
          if let Some(name) = decoded.take() {
            members.push((name.into(), iter_offset, value_start));
          }
          **resume = Some(after);
        }
        return Ok(None);
      }
      _ => return Err(TraverseError::Malformed(after)),
    }
  }
}

/// From a member key's closing-quote offset, skip the `:` separator (and
/// surrounding whitespace) to the value's first byte.
fn member_value_start(walker: &mut Walker, key_close: u64) -> Result<u64, TraverseError> {
  let post_key = walker.skip_whitespace(key_close + 1)?;
  if walker.byte_at(post_key)? != Some(b':') {
    return Err(TraverseError::Malformed(post_key));
  }
  Ok(walker.skip_whitespace(post_key + 1)?)
}

/// Per-iteration scan state for `step_object` / `step_array`. Flattened across
/// both kinds: object scans read only `offset`; array scans use all fields (the
/// comma fast path needs `index`/`depth`/`carry` to resume after a mid-scan
/// `ChunkMiss`).
#[derive(Debug, Clone)]
struct SegmentScan {
  kind: ContainerKind,
  /// Where the next iteration begins.
  offset: u64,
  /// Array element index considered next. Always 0 for objects.
  index: usize,
  /// Container-nesting depth at `offset`, relative to the entered container.
  /// Always 0 for objects.
  depth: u32,
  /// String-scan carry at `offset` for the array comma fast path. Default at
  /// element boundaries; the fast path may commit a mid-string carry at a block
  /// boundary. Unused for objects.
  carry: ScanCarry,
}

fn next_object_member(
  walker: &mut Walker,
  cursor: &mut ContainerCursor,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cursor.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b'}') => {
      cursor.next_offset = offset;
      return Ok(None);
    }
    Some(b'"') => {}
    Some(_) => return Err(TraverseError::Malformed(offset)),
  }
  let key_close = walker
    .next_string_close(offset + 1)?
    .ok_or(TraverseError::UnexpectedEof(offset))?;
  let value_start = member_value_start(walker, key_close)?;
  let (location, next_offset) = advance_past_value(walker, value_start, b'}')?;
  cursor.next_offset = next_offset;
  Ok(Some(ChildEntry {
    key: ChildKey::Member {
      start: offset,
      close: key_close,
    },
    location,
  }))
}

fn next_array_element(
  walker: &mut Walker,
  cursor: &mut ContainerCursor,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cursor.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b']') => {
      cursor.next_offset = offset;
      return Ok(None);
    }
    _ => {}
  }
  let (location, next_offset) = advance_past_value(walker, offset, b']')?;
  cursor.next_offset = next_offset;
  let index = cursor.index;
  cursor.index += 1;
  Ok(Some(ChildEntry {
    key: ChildKey::Index(index),
    location,
  }))
}

/// Skip the value at `value_start` and the separator after it, returning the
/// value's location and the next child's offset (or the offset of `close` when
/// this was the last child).
fn advance_past_value(
  walker: &mut Walker,
  value_start: u64,
  close: u8,
) -> Result<(ValueLocation, u64), TraverseError> {
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  let next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b) if b == close => after,
    Some(_) | None => return Err(TraverseError::Malformed(after)),
  };
  Ok((
    ValueLocation {
      start: value_start,
      end: value_end,
    },
    next_offset,
  ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFault {
  /// A segment tried to descend into a value that isn't a container.
  ThroughScalar { segment: usize },
  /// A member-name segment addressed an array, or an index segment an object.
  WrongKind { segment: usize },
  /// A container operation (`count`/`iter`) was aimed at a scalar target.
  ScalarTarget,
}

#[napi(string_enum = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFaultCode {
  ThroughScalar,
  ScalarTarget,
  WrongKind,
}

impl PathFaultCode {
  pub(crate) fn as_str(self) -> &'static str {
    match self {
      Self::ThroughScalar => "through_scalar",
      Self::ScalarTarget => "scalar_target",
      Self::WrongKind => "wrong_kind",
    }
  }
}

impl PathFault {
  fn fault(&self) -> PathFaultCode {
    match self {
      Self::ThroughScalar { .. } => PathFaultCode::ThroughScalar,
      Self::WrongKind { .. } => PathFaultCode::WrongKind,
      Self::ScalarTarget => PathFaultCode::ScalarTarget,
    }
  }

  /// The offending segment index, where one is meaningful.
  fn segment(&self) -> Option<usize> {
    match self {
      Self::ThroughScalar { segment } | Self::WrongKind { segment } => Some(*segment),
      Self::ScalarTarget => None,
    }
  }

  /// Machine code carried across the napi boundary (see `session.rs`
  /// `SessionError::Path`): `<code>`, or `<code>:<segment>` where the offending
  /// segment is meaningful. Only this discriminant crosses; the facade owns the
  /// human message keyed off [`PathFaultCode`].
  pub(crate) fn code(&self) -> String {
    match self.segment() {
      Some(seg) => format!("{}:{}", self.fault().as_str(), seg),
      None => self.fault().as_str().to_string(),
    }
  }
}

/// Outcome of resolving a path to its value's start offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
  /// The resolved value's start offset.
  Found(u64),
  /// A clean miss: a well-formed path addressed a member/index that isn't there.
  Absent,
  /// The path contradicts the document shape; see [`PathFault`].
  NotNavigable(PathFault),
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::chunks::ChunkWindow;
  use bytes::Bytes;

  /// A [`ChunkWindow`] holding every chunk of `source`, so a single
  /// [`resolve_step`] resolves without faulting.
  fn full_window(source: &[u8]) -> ChunkWindow {
    let cs = 64u64;
    let mut w = ChunkWindow::new(cs, source.len() as u64);
    let mut off = 0u64;
    while (off as usize) < source.len() {
      let end = (off as usize + cs as usize).min(source.len());
      w.insert(off, Bytes::copy_from_slice(&source[off as usize..end]));
      off += cs;
    }
    w
  }

  fn resolve_once(win: &ChunkWindow, path: &[Segment], state: &mut ResolveState) -> Option<u64> {
    let mut w = Walker::new(win);
    match resolve_step(&mut w, path, state).expect("all chunks resident: no miss, no error") {
      Resolved::Found(off) => Some(off),
      Resolved::Absent | Resolved::NotNavigable(_) => None,
    }
  }

  fn member(name: &str) -> Segment {
    Segment::Member(name.into())
  }

  #[test]
  fn resume_object_equals_cold() {
    // {"a":1,"b":2,"c":3} - "b" key starts at offset 7.
    let src = br#"{"a":1,"b":2,"c":3}"#;
    let win = full_window(src);

    let mut cold = ResolveState::resume(0, 0, None, false, 0);
    let cold_c = resolve_once(&win, &[member("c")], &mut cold);

    let mut seeded = ResolveState::resume(0, 0, Some(ResumePoint::Object { offset: 7 }), false, 0);
    let seeded_c = resolve_once(&win, &[member("c")], &mut seeded);

    assert_eq!(cold_c, seeded_c);
    assert_eq!(src[cold_c.unwrap() as usize], b'3');
  }

  #[test]
  fn resume_array_equals_cold() {
    // [10,20,30,40] - element 1 starts at offset 4.
    let src = b"[10,20,30,40]";
    let win = full_window(src);

    let mut cold = ResolveState::resume(0, 0, None, false, 0);
    let cold_3 = resolve_once(&win, &[Segment::Element(3)], &mut cold);

    let mut seeded = ResolveState::resume(
      0,
      0,
      Some(ResumePoint::Array {
        index: 1,
        offset: 4,
      }),
      false,
      0,
    );
    let seeded_3 = resolve_once(&win, &[Segment::Element(3)], &mut seeded);

    assert_eq!(cold_3, seeded_3);
    assert_eq!(src[cold_3.unwrap() as usize], b'4');
  }

  #[test]
  fn records_every_scanned_member() {
    let src = br#"{"a":1,"b":2,"c":3}"#;
    let win = full_window(src);
    let mut st = ResolveState::resume(0, 0, None, true, 16);
    resolve_once(&win, &[member("c")], &mut st);

    let h = st.take_scan_record().expect("collecting");
    assert_eq!(h.containers.len(), 1);
    let cs = &h.containers[0];
    assert_eq!(cs.seg, 0);
    assert_eq!(cs.value_start, 0);
    let RecordBody::Object { members, resume } = &cs.body else {
      panic!("expected an object record");
    };
    let names: Vec<&str> = members.iter().map(|(n, _, _)| n.as_ref()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    // "c" starts at offset 13 - the high-water resume point.
    assert_eq!(*resume, Some(13));
  }

  #[test]
  fn records_array_member() {
    let src = b"[10,20,30,40]";
    let win = full_window(src);
    let mut st = ResolveState::resume(0, 0, None, false, 16);
    resolve_once(&win, &[Segment::Element(2)], &mut st);

    let h = st.take_scan_record().expect("collecting");
    let RecordBody::Array { members } = &h.containers[0].body else {
      panic!("expected an array record");
    };
    // Stride 16 over 4 elements samples no grid array member, just the resolved
    // target. Element 2 ("30") starts at offset 7.
    assert_eq!(*members, vec![(2, 7)]);
  }

  #[test]
  fn records_last_member_on_miss() {
    // Target absent: scan runs to the close. The last member (ends with `}` not
    // `,`) must be tabled too, or a later lookup of it would resume past it.
    let src = br#"{"a":1,"b":2}"#;
    let win = full_window(src);
    let mut st = ResolveState::resume(0, 0, None, true, 16);
    assert_eq!(resolve_once(&win, &[member("zzz")], &mut st), None);

    let h = st.take_scan_record().unwrap();
    let RecordBody::Object { members, resume } = &h.containers[0].body else {
      panic!("expected an object record");
    };
    let names: Vec<&str> = members.iter().map(|(n, _, _)| n.as_ref()).collect();
    assert_eq!(names, vec!["a", "b"], "the last member must be tabled too");
    assert_eq!(*resume, Some(12)); // the closing `}`
  }

  #[test]
  fn records_spans_nested_containers() {
    let src = br#"{"users":[{"id":1},{"id":2,"name":"bo"}]}"#;
    let win = full_window(src);
    let mut st = ResolveState::resume(0, 0, None, true, 16);
    let got = resolve_once(
      &win,
      &[member("users"), Segment::Element(1), member("name")],
      &mut st,
    );
    assert!(got.is_some());

    let h = st.take_scan_record().expect("collecting");
    // One ContainerRecord per entered container: root object, users array, element object.
    assert_eq!(h.containers.len(), 3);
    assert_eq!(h.containers[0].seg, 0);
    assert_eq!(h.containers[1].seg, 1);
    assert_eq!(h.containers[2].seg, 2);
    let RecordBody::Array { members } = &h.containers[1].body else {
      panic!("expected an array record for the users array");
    };
    assert_eq!(members.last().map(|&(i, _)| i), Some(1));
  }
}
