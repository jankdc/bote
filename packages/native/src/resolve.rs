//! Path evaluator.
//!
//! Walks a [`&[Segment]`] over a scan-aligned [`Walker`], descending one segment
//! at a time into objects (by member name) and arrays (by index). Returns the
//! byte range covering the resolved value or `None`, when any segment along the
//! path doesn't address an existing member.

use crate::path::Segment;
use crate::simd::ScanCarry;
use crate::walker::{ArrayMemberSink, CommaStop, TraverseError, Walker};

/// Drive the resolver forward against the current `state`.
pub fn resolve_step(
  walker: &mut Walker,
  path: &[Segment],
  state: &mut ResolveState,
) -> Result<Option<u64>, TraverseError> {
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
    if segment_scan.is_none() {
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
          _ => return Ok(None),
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
          kind,
          value_start,
          members: Vec::new(),
          object_resume: None,
          array_members: Vec::new(),
        });
      }
      *segment_scan = Some(ls);
    }
    let segment = &path[*segment_idx];
    let ls = segment_scan.as_mut().expect("set just above");
    let cs = scan_record.as_mut().and_then(|h| h.containers.last_mut());
    let descend = match (ls.kind, segment) {
      // Recording is gated per kind: objects skip it (and key decode) when the
      // object cap is 0; arrays skip the array-member sink when the stride is 0.
      (ContainerKind::Object, Segment::Member(name)) => {
        step_object(walker, name, ls, if record_objects { cs } else { None })?
      }
      (ContainerKind::Array, Segment::Element(idx)) => step_array(
        walker,
        *idx,
        ls,
        array_stride,
        if array_stride > 0 { cs } else { None },
      )?,
      // Type mismatch (member into array, index into object) is a miss, not an
      // error - mirrors RFC 6901, where `/0` against an object resolves to nothing.
      _ => return Ok(None),
    };
    match descend {
      Some(vs) => {
        *start = vs;
        *segment_idx += 1;
        *segment_scan = None;
      }
      None => return Ok(None),
    }
  }
  Ok(Some(*start))
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

  /// Lowest offset a resumed step might still read (a key `read_range` reads
  /// behind the scan position but never behind the iteration's start). The
  /// retention bound for the byte window.
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
  pub kind: ContainerKind,
  /// Offset of the container's `{`/`[`.
  pub value_start: u64,
  /// Object members seen, in scan order: `(name, key_start, value_start)`.
  /// Empty for arrays.
  pub members: Vec<(Box<str>, u64, u64)>,
  /// Object only: high-water resume offset - the matched member's start, or the
  /// close `}`.
  pub object_resume: Option<u64>,
  /// Array only: `(index, element_offset)` array members (one per stride
  /// multiple) plus the resolved target, in ascending index order.
  pub array_members: Vec<(usize, u64)>,
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

impl ChildEntry {
  pub fn location(&self) -> ValueLocation {
    match self {
      Self::Member { location, .. } | Self::Element { location, .. } => *location,
    }
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

/// Advance an array scan, committing `state` only after a fully successful
/// iteration.
fn step_array(
  walker: &mut Walker,
  target_index: usize,
  state: &mut SegmentScan,
  array_stride: usize,
  mut cs: Option<&mut ContainerRecord>,
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
      let sink = cs.as_deref_mut().map(|cs| ArrayMemberSink {
        base_index,
        stride: array_stride,
        out: &mut cs.array_members,
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
      CommaStop::ArrayClosed { consumed: _ } => {
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
      if let Some(cs) = cs.as_deref_mut() {
        cs.array_members.push((target_index, offset));
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

/// Advance an object scan, committing `state.offset` only after a fully
/// successful iteration, so a mid-iteration `ChunkMiss` redoes at most one key.
fn step_object(
  walker: &mut Walker,
  target: &str,
  state: &mut SegmentScan,
  mut cs: Option<&mut ContainerRecord>,
) -> Result<Option<u64>, TraverseError> {
  let collecting = cs.is_some();
  loop {
    let iter_offset = state.offset;
    let offset = walker.skip_whitespace(iter_offset)?;
    match walker.byte_at(offset)? {
      None => return Err(TraverseError::UnexpectedEof(offset)),
      Some(b'}') => {
        if let Some(cs) = cs.as_deref_mut() {
          cs.object_resume = Some(offset);
        }
        return Ok(None);
      }
      Some(b'"') => {}
      Some(_) => return Err(TraverseError::Malformed(offset)),
    }
    let key_close = walker
      .next_string_close(offset + 1)?
      .ok_or(TraverseError::UnexpectedEof(offset))?;

    let target_bytes = target.as_bytes();
    let raw_len = (key_close - offset).saturating_sub(1) as usize;
    // Collecting needs the decoded key for the cache table. Otherwise: JSON
    // escapes only shrink a string's byte count, so a raw span shorter than the
    // target can't match and the `read_range` allocation is skipped.
    let mut decoded: Option<String> = None;
    let matches = if !collecting {
      if raw_len < target_bytes.len() {
        false
      } else {
        let raw = walker.read_range(offset, key_close + 1)?;
        quoted_string_eq(&raw, target).map_err(|()| TraverseError::Malformed(offset))?
      }
    } else {
      let raw = walker.read_range(offset, key_close + 1)?;
      let name: String =
        serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(offset))?;
      let m = name == target;
      decoded = Some(name);
      m
    };
    let value_start = member_value_start(walker, key_close)?;
    if matches {
      // Match commit point: no fallible call follows, so recording here is never
      // redone by a retry.
      if let Some(cs) = cs.as_deref_mut() {
        if let Some(name) = decoded.take() {
          cs.members.push((name.into(), iter_offset, value_start));
        }
        cs.object_resume = Some(iter_offset);
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
        if let Some(cs) = cs.as_deref_mut() {
          if let Some(name) = decoded.take() {
            cs.members.push((name.into(), iter_offset, value_start));
          }
        }
      }
      Some(b'}') => {
        // Last member (no trailing comma): record it before the resume point
        // advances to the close, else the resume point would skip past it.
        if let Some(cs) = cs.as_deref_mut() {
          if let Some(name) = decoded.take() {
            cs.members.push((name.into(), iter_offset, value_start));
          }
          cs.object_resume = Some(after);
        }
        return Ok(None);
      }
      _ => return Err(TraverseError::Malformed(after)),
    }
  }
}

/// Compare a JSON-encoded string value's raw bytes (quotes included) to a `&str`.
/// Escape-free interiors byte-compare directly; escaped ones go through
/// `serde_json`. `Err(())` means the escaped interior failed to decode
/// (malformed); the caller maps it to [`TraverseError::Malformed`].
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
  let raw = walker.read_range(offset, key_close + 1)?;
  let key: String = serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(offset))?;
  let value_start = member_value_start(walker, key_close)?;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cursor.next_offset = match walker.byte_at(after)? {
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
  let value_start = offset;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cursor.next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b']') => after,
    Some(_) | None => return Err(TraverseError::Malformed(after)),
  };
  let index = cursor.index;
  cursor.index += 1;
  Ok(Some(ChildEntry::Element {
    index,
    location: ValueLocation {
      start: value_start,
      end: value_end,
    },
  }))
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
    resolve_step(&mut w, path, state).expect("all chunks resident: no miss, no error")
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
    let names: Vec<&str> = cs.members.iter().map(|(n, _, _)| n.as_ref()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    // "c" starts at offset 13 - the high-water resume point.
    assert_eq!(cs.object_resume, Some(13));
  }

  #[test]
  fn records_array_member() {
    let src = b"[10,20,30,40]";
    let win = full_window(src);
    let mut st = ResolveState::resume(0, 0, None, false, 16);
    resolve_once(&win, &[Segment::Element(2)], &mut st);

    let h = st.take_scan_record().expect("collecting");
    let cs = &h.containers[0];
    assert!(cs.members.is_empty());
    // Stride 16 over 4 elements samples no grid array member, just the resolved
    // target. Element 2 ("30") starts at offset 7.
    assert_eq!(cs.array_members, vec![(2, 7)]);
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
    let cs = &h.containers[0];
    let names: Vec<&str> = cs.members.iter().map(|(n, _, _)| n.as_ref()).collect();
    assert_eq!(names, vec!["a", "b"], "the last member must be tabled too");
    assert_eq!(cs.object_resume, Some(12)); // the closing `}`
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
    assert_eq!(
      h.containers[1].array_members.last().map(|&(i, _)| i),
      Some(1)
    );
  }
}
