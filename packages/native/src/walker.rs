//! Walker: scan-aligned, store-free traversal of a chunked JSON document.
//!
//! [`Walker`] is the seam between bitmap construction (sync, pure) and the
//! async source. It exposes synchronous primitives that consume already-loaded
//! chunks from a [`ChunkWindow`]; when one needs a chunk that isn't loaded, it
//! returns [`ChunkMiss`] with the offset to fetch and the async caller retries.
//!
//! Bitmaps are NOT stored. Each primitive builds its 64-byte-block bitmaps on
//! the fly at its own scan position, threading [`ScanCarry`] block to block. The
//! entry carry is known structurally - `ScanCarry::default()` (outside-string)
//! at every value/element/container boundary, [`INSIDE_STRING`] one byte past an
//! opening quote - so a scan never rebuilds state from the document start.
//! Resumable scans commit their `(offset, carry)` at a block boundary, so a
//! chunk fault never rewinds work or loses the carry.

use thiserror::Error;

use crate::bitmap::{structural_word, Structural};
pub use crate::chunks::ChunkMiss;
use crate::chunks::ChunkWindow;
use crate::simd::{scan_block, ScanCarry, BLOCK_BYTES};

/// Carry one byte past an opening quote: inside a string, no pending escape
/// (the opening quote is never a backslash, so `prev_escaped` is 0).
pub const INSIDE_STRING: ScanCarry = ScanCarry {
  prev_escaped: 0,
  inside_string: !0,
};

/// Errors from higher-level traversal helpers. `ChunkMiss` is folded in so
/// callers can use `?` uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TraverseError {
  #[error("{0}")]
  Pending(#[from] ChunkMiss),
  #[error("unexpected end of input at offset {0}")]
  UnexpectedEof(u64),
  #[error("malformed JSON at offset {0}")]
  Malformed(u64),
}

pub struct Walker<'a> {
  bytes: &'a ChunkWindow,
  /// Most-recently-touched chunk, checked before the window's HashMap. Borrow
  /// is tied to `'a` (the window), not to `&mut self`.
  cached: Option<CachedChunk<'a>>,
}

#[derive(Clone, Copy)]
struct CachedChunk<'a> {
  offset: u64,
  data: &'a [u8],
}

impl<'a> Walker<'a> {
  pub fn new(bytes: &'a ChunkWindow) -> Self {
    Self {
      bytes,
      cached: None,
    }
  }

  #[inline]
  pub fn source_size(&self) -> u64 {
    self.bytes.source_size()
  }

  #[inline]
  pub fn chunk_bytes(&self) -> u64 {
    self.bytes.chunk_bytes()
  }

  #[inline]
  fn chunk_slice(&mut self, chunk_start: u64) -> Result<&'a [u8], ChunkMiss> {
    if let Some(c) = self.cached {
      if c.offset == chunk_start {
        return Ok(c.data);
      }
    }
    let data: &'a [u8] = self.bytes.chunk_for(chunk_start)?;
    self.cached = Some(CachedChunk {
      offset: chunk_start,
      data,
    });
    Ok(data)
  }

  /// The 64-byte block at `offset` (aligned to `offset`, not a chunk boundary),
  /// space-padding any tail past end-of-source. It straddles at most two chunks;
  /// `ChunkMiss` names the lower absent one.
  fn block_at(&mut self, offset: u64) -> Result<[u8; BLOCK_BYTES], ChunkMiss> {
    let source_size = self.source_size();
    let cs = self.chunk_bytes();
    let mut block = [b' '; BLOCK_BYTES];
    let end = (offset + BLOCK_BYTES as u64).min(source_size);
    let mut o = offset;
    while o < end {
      let chunk_start = (o / cs) * cs;
      let data = self.chunk_slice(chunk_start)?;
      let chunk_end = chunk_start + data.len() as u64;
      let take_end = end.min(chunk_start + cs).min(chunk_end);
      if take_end <= o {
        break; // short tail chunk; leave the rest padded
      }
      let local = (o - chunk_start) as usize;
      let dst = (o - offset) as usize;
      let n = (take_end - o) as usize;
      block[dst..dst + n].copy_from_slice(&data[local..local + n]);
      o = chunk_start + cs;
    }
    Ok(block)
  }

  #[inline]
  pub fn byte_at(&mut self, offset: u64) -> Result<Option<u8>, ChunkMiss> {
    let source_size = self.source_size();
    if offset >= source_size {
      return Ok(None);
    }
    let cs = self.chunk_bytes();
    let chunk_start = (offset / cs) * cs;
    let data = self.chunk_slice(chunk_start)?;
    Ok(data.get((offset - chunk_start) as usize).copied())
  }

  /// Copy bytes in `[from, to)` into an owned buffer, or `ChunkMiss` for the
  /// first chunk that isn't resident.
  pub fn read_range(&mut self, from: u64, to: u64) -> Result<Vec<u8>, ChunkMiss> {
    let end = to.min(self.source_size());
    let cs = self.chunk_bytes();
    let mut out = Vec::with_capacity(end.saturating_sub(from) as usize);
    let mut offset = from;
    while offset < end {
      let chunk_start = (offset / cs) * cs;
      let data = self.chunk_slice(chunk_start)?;
      let chunk_end = chunk_start + data.len() as u64;
      let local_start = (offset - chunk_start) as usize;
      let local_end = (end.min(chunk_start + cs).min(chunk_end) - chunk_start) as usize;
      if local_end > local_start {
        out.extend_from_slice(&data[local_start..local_end]);
      }
      offset = chunk_start + cs;
    }
    Ok(out)
  }

  /// Advance from `from` while `pred` holds, stopping at the first failing byte,
  /// end-of-source, or end-of-loaded-data.
  #[inline]
  fn skip_while(&mut self, from: u64, pred: impl Fn(u8) -> bool) -> Result<u64, ChunkMiss> {
    let mut offset = from;
    while offset < self.source_size() {
      match self.byte_at(offset)? {
        None => return Ok(offset),
        Some(b) if pred(b) => offset += 1,
        Some(_) => return Ok(offset),
      }
    }
    Ok(offset)
  }

  #[inline]
  pub fn skip_whitespace(&mut self, from: u64) -> Result<u64, ChunkMiss> {
    self.skip_while(from, |b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
  }

  #[inline]
  pub fn skip_primitive(&mut self, from: u64) -> Result<u64, ChunkMiss> {
    self.skip_while(from, is_primitive_byte)
  }

  /// Offset of the closing `"` for the string whose interior begins at
  /// `interior` (one past the opening quote). Non-resumable wrapper around
  /// [`Walker::next_string_close_step`]: a chunk fault re-runs from `interior`.
  /// Fine for object keys (short); resumable string *values* use the step directly.
  pub fn next_string_close(&mut self, interior: u64) -> Result<Option<u64>, ChunkMiss> {
    let mut scan = StringScan {
      offset: interior,
      carry: INSIDE_STRING,
    };
    self.next_string_close_step(&mut scan)
  }

  /// Drive a [`StringScan`] forward until `in_string` first goes 0 (the closing
  /// quote) or end-of-source. Commits `(offset, carry)` at each block boundary,
  /// so a `ChunkMiss` resumes mid-string without rescanning.
  pub fn next_string_close_step(
    &mut self,
    scan: &mut StringScan,
  ) -> Result<Option<u64>, ChunkMiss> {
    let source_size = self.source_size();
    while scan.offset < source_size {
      let block = self.block_at(scan.offset)?;
      let (in_string, next) = scan_block(&block, scan.carry);
      // Mask off the space-padded tail past end-of-source so its 0 in_string
      // bits aren't mistaken for a closing quote.
      let valid = (source_size - scan.offset).min(BLOCK_BYTES as u64);
      let mask = if valid >= BLOCK_BYTES as u64 {
        !0u64
      } else {
        (1u64 << valid) - 1
      };
      let m = !in_string & mask;
      if m != 0 {
        return Ok(Some(scan.offset + m.trailing_zeros() as u64));
      }
      scan.offset += BLOCK_BYTES as u64;
      scan.carry = next;
    }
    Ok(None)
  }

  /// Skip the JSON value whose first byte is at `from` (whitespace already
  /// consumed), returning the offset just after it.
  pub fn skip_value(&mut self, from: u64) -> Result<u64, TraverseError> {
    let mut state = SkipState::start(from);
    skip_value_step(self, &mut state)
  }

  fn skip_container_step(&mut self, state: &mut ContainerSkipState) -> Result<u64, TraverseError> {
    let open = state.open;
    let close = state.close;
    let source_size = self.source_size();
    while state.offset < source_size {
      // `state` is committed at the last block boundary, so a ChunkMiss `?`
      // propagates without losing progress.
      let block = self.block_at(state.offset)?;
      let (in_string, next) = scan_block(&block, state.carry);
      let opens = structural_word(&block, in_string, open);
      let closes = structural_word(&block, in_string, close);

      let c = closes.count_ones();
      // Fast path: too few closes to exhaust depth here, so it can't hit zero -
      // bulk-update instead of walking bit-by-bit.
      if c < state.depth {
        state.depth = state.depth + opens.count_ones() - c;
      } else {
        let mut bits = opens | closes;
        while bits != 0 {
          let bit_idx = bits.trailing_zeros();
          let bit = 1u64 << bit_idx;
          let abs = state.offset + bit_idx as u64;
          if opens & bit != 0 {
            state.depth += 1;
          } else {
            state.depth = state
              .depth
              .checked_sub(1)
              .ok_or(TraverseError::Malformed(abs))?;
            if state.depth == 0 {
              return Ok(abs + 1);
            }
          }
          bits &= bits - 1;
        }
      }
      state.offset += BLOCK_BYTES as u64;
      state.carry = next;
    }
    Err(TraverseError::UnexpectedEof(state.offset))
  }

  /// Advance past `needed` depth-0 (element-boundary) commas of the current
  /// array, returning the offset just past the last one (the next element's
  /// first byte). `entry_carry` is the string-scan carry at `from` (default at
  /// element boundaries, or the committed carry from a prior [`CommaStop::Partial`]).
  ///
  /// When `sink` is `Some` (the structural-index cache is collecting), a landmark
  /// is appended at each element-index stride multiple crossed, snapped to an
  /// exact element boundary, so a later index can resume from the nearest landmark
  /// instead of the array open. `None` leaves both scan paths byte-for-byte unchanged.
  pub fn advance_top_level_commas(
    &mut self,
    from: u64,
    initial_depth: u32,
    needed: usize,
    entry_carry: ScanCarry,
    mut sink: Option<LandmarkSink>,
  ) -> Result<CommaStop, TraverseError> {
    if needed == 0 {
      return Ok(CommaStop::Found {
        offset_after_comma: from,
        consumed: 0,
      });
    }
    let source_size = self.source_size();
    let mut scan = CommaScan {
      depth: initial_depth,
      remaining: needed,
      consumed: 0,
    };
    let mut offset = from;
    let mut carry = entry_carry;
    while offset < source_size {
      let block = match self.block_at(offset) {
        Ok(w) => w,
        // No progress yet: propagate the miss to retry unchanged. After progress,
        // commit the boundary via `Partial` so accumulated counts and carry survive.
        Err(miss) => {
          if offset == from {
            return Err(miss.into());
          }
          return Ok(CommaStop::Partial {
            offset,
            depth: scan.depth,
            consumed: scan.consumed,
            carry,
          });
        }
      };
      let (in_string, next) = scan_block(&block, carry);
      let words = StructuralWords::new(&block, in_string);
      let outcome = if scan.depth == 0 && words.nesting_free() {
        scan.scan_flat(words.commas, offset, sink.as_mut())
      } else {
        scan.scan_nested(&words, offset, sink.as_mut())
      };
      match outcome {
        BlockOutcome::Found { offset_after_comma } => {
          return Ok(CommaStop::Found {
            offset_after_comma,
            consumed: scan.consumed,
          })
        }
        BlockOutcome::ArrayClosed => {
          return Ok(CommaStop::ArrayClosed {
            consumed: scan.consumed,
          })
        }
        BlockOutcome::Continue => {}
      }
      offset += BLOCK_BYTES as u64;
      carry = next;
    }
    Err(TraverseError::UnexpectedEof(offset))
  }
}

/// Structural words of one block, string contents masked out: container opens
/// (`{`/`[`), closes (`}`/`]`), and commas.
struct StructuralWords {
  opens: u64,
  closes: u64,
  commas: u64,
}

impl StructuralWords {
  fn new(block: &[u8; BLOCK_BYTES], in_string: u64) -> Self {
    let lbrace = structural_word(block, in_string, Structural::LBrace);
    let rbrace = structural_word(block, in_string, Structural::RBrace);
    let lbracket = structural_word(block, in_string, Structural::LBracket);
    let rbracket = structural_word(block, in_string, Structural::RBracket);
    Self {
      opens: lbrace | lbracket,
      closes: rbrace | rbracket,
      commas: structural_word(block, in_string, Structural::Comma),
    }
  }

  /// No container transitions, so every comma is a depth-0 boundary and the
  /// popcount fast path applies.
  fn nesting_free(&self) -> bool {
    self.opens == 0 && self.closes == 0
  }
}

/// Outcome of scanning one block in [`Walker::advance_top_level_commas`].
enum BlockOutcome {
  /// Target comma consumed; the next element begins at `offset_after_comma`.
  Found { offset_after_comma: u64 },
  /// A depth-0 close was hit - the array ended before the target.
  ArrayClosed,
  /// Block exhausted, target not reached.
  Continue,
}

/// Per-structural-bit counters threaded into the scan helpers, distinct from
/// the per-block `offset`/`carry` that stay in the driver loop.
struct CommaScan {
  depth: u32,
  remaining: usize,
  consumed: usize,
}

impl CommaScan {
  /// Depth-0 fast path: bulk-count this block's commas, returning [`Found`] when
  /// the target falls inside it.
  ///
  /// [`Found`]: BlockOutcome::Found
  fn scan_flat(
    &mut self,
    commas: u64,
    offset: u64,
    sink: Option<&mut LandmarkSink>,
  ) -> BlockOutcome {
    if let Some(s) = sink {
      s.sample_block(commas, offset, self.consumed);
    }
    let c = commas.count_ones() as usize;
    if c < self.remaining {
      self.remaining -= c;
      self.consumed += c;
      BlockOutcome::Continue
    } else {
      let mut bits = commas;
      for _ in 0..self.remaining - 1 {
        bits &= bits - 1;
      }
      let bit_idx = bits.trailing_zeros() as u64;
      self.consumed += self.remaining;
      BlockOutcome::Found {
        offset_after_comma: offset + bit_idx + 1,
      }
    }
  }

  /// Bit-iterating path for blocks with nesting: track depth and count only
  /// depth-0 commas.
  fn scan_nested(
    &mut self,
    words: &StructuralWords,
    offset: u64,
    mut sink: Option<&mut LandmarkSink>,
  ) -> BlockOutcome {
    let mut bits = words.opens | words.closes | words.commas;
    while bits != 0 {
      let bit_idx = bits.trailing_zeros();
      let bit = 1u64 << bit_idx;
      let abs = offset + bit_idx as u64;
      if words.opens & bit != 0 {
        self.depth += 1;
      } else if words.closes & bit != 0 {
        if self.depth == 0 {
          return BlockOutcome::ArrayClosed;
        }
        self.depth -= 1;
      } else if self.depth == 0 {
        self.remaining -= 1;
        self.consumed += 1;
        if let Some(s) = sink.as_deref_mut() {
          let index = s.base_index + self.consumed;
          s.sample(index, abs + 1);
        }
        if self.remaining == 0 {
          return BlockOutcome::Found {
            offset_after_comma: abs + 1,
          };
        }
      }
      bits &= bits - 1;
    }
    BlockOutcome::Continue
  }
}

/// Sink for array landmarks sampled by [`Walker::advance_top_level_commas`].
/// Records `(index, offset)` for every element whose absolute index is a
/// multiple of `stride`, each snapped to an exact element boundary. The grid is
/// anchored at index 0, so sampling is stateless across `ChunkMiss` resumes -
/// the absolute index is recomputed from `base_index` each call.
pub struct LandmarkSink<'a> {
  /// Element index at the scan call's `from`. An element's absolute index is
  /// `base_index + (depth-0 commas consumed before it)`.
  pub base_index: usize,
  /// Stride between landmarks (`> 0`).
  pub stride: usize,
  /// Collected landmarks, appended in ascending index order.
  pub out: &'a mut Vec<(usize, u64)>,
}

impl LandmarkSink<'_> {
  /// Record element `index` starting at `after`, but only when it lands on the
  /// stride grid.
  fn sample(&mut self, index: usize, after: u64) {
    if index.is_multiple_of(self.stride) {
      self.out.push((index, after));
    }
  }

  /// Fast-path sampling: emit a landmark for every stride multiple crossed by
  /// this block's `commas`. `start = base_index + base_consumed` is the index at
  /// the block's first comma; the j-th comma (1-based) ends element `start + j`.
  /// Visiting only the j with `(start + j) % stride == 0` makes the work scale
  /// with landmarks-in-block, not commas.
  fn sample_block(&mut self, commas: u64, offset: u64, base_consumed: usize) {
    let c = commas.count_ones() as usize;
    if c == 0 {
      return;
    }
    let start = self.base_index + base_consumed;
    // First j (1-based, in `1..=stride`) with `(start + j) % stride == 0`.
    let mut j = self.stride - start % self.stride;
    while j <= c {
      // j-th set bit: clear the low `j - 1`, take the next.
      let mut bits = commas;
      for _ in 0..j - 1 {
        bits &= bits - 1;
      }
      let bit_idx = bits.trailing_zeros() as u64;
      self.out.push((start + j, offset + bit_idx + 1));
      j += self.stride;
    }
  }
}

/// Resumable position of an in-string scan: next block offset and the carry
/// entering it. Threaded across faults so a long string isn't rescanned.
#[derive(Debug, Clone, Copy)]
pub struct StringScan {
  pub offset: u64,
  pub carry: ScanCarry,
}

/// Resumable state for [`skip_value_step`], persisted across faults by
/// [`crate::session::Session::drive`] so a long skip needn't restart.
#[derive(Debug, Clone)]
pub(crate) enum SkipState {
  /// Need the opening byte at `from` to pick a kind.
  Pending {
    from: u64,
  },
  String(StringScan),
  Primitive {
    offset: u64,
  },
  Container(ContainerSkipState),
}

/// Resumable state for skipping a container: `(offset, depth)` plus the
/// threaded `carry`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContainerSkipState {
  offset: u64,
  depth: u32,
  carry: ScanCarry,
  open: Structural,
  close: Structural,
}

impl SkipState {
  pub fn start(from: u64) -> Self {
    SkipState::Pending { from }
  }

  /// Lowest offset a resumed step might still read - the byte window's retention
  /// bound; nothing below is reachable again.
  pub fn min_reachable(&self) -> u64 {
    match self {
      SkipState::Pending { from } => *from,
      SkipState::String(s) => s.offset,
      SkipState::Primitive { offset } => *offset,
      SkipState::Container(c) => c.offset,
    }
  }
}

/// Drive a [`SkipState`] forward. Returns the offset just after the value, or
/// propagates `ChunkMiss` with `state` committed so the next call resumes.
pub(crate) fn skip_value_step(
  walker: &mut Walker,
  state: &mut SkipState,
) -> Result<u64, TraverseError> {
  if let SkipState::Pending { from } = *state {
    let byte = walker
      .byte_at(from)?
      .ok_or(TraverseError::UnexpectedEof(from))?;
    *state = match byte {
      b'"' => SkipState::String(StringScan {
        offset: from + 1,
        carry: INSIDE_STRING,
      }),
      b'{' => SkipState::Container(ContainerSkipState {
        offset: from + 1,
        depth: 1,
        carry: ScanCarry::default(),
        open: Structural::LBrace,
        close: Structural::RBrace,
      }),
      b'[' => SkipState::Container(ContainerSkipState {
        offset: from + 1,
        depth: 1,
        carry: ScanCarry::default(),
        open: Structural::LBracket,
        close: Structural::RBracket,
      }),
      _ => SkipState::Primitive { offset: from },
    };
  }
  match state {
    SkipState::Pending { .. } => unreachable!("committed above"),
    SkipState::String(scan) => {
      let close = walker
        .next_string_close_step(scan)?
        .ok_or(TraverseError::UnexpectedEof(scan.offset))?;
      Ok(close + 1)
    }
    SkipState::Primitive { offset } => Ok(walker.skip_primitive(*offset)?),
    SkipState::Container(c) => walker.skip_container_step(c),
  }
}

/// Outcome of [`Walker::advance_top_level_commas`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommaStop {
  /// Target reached: `consumed == needed`, `offset_after_comma` is the target
  /// element's first byte (before whitespace).
  Found {
    offset_after_comma: u64,
    consumed: usize,
  },
  /// Array's terminating `]` reached before consuming `needed` commas.
  ArrayClosed { consumed: usize },
  /// Block-boundary commit; caller resumes with these values once the next
  /// chunk loads.
  Partial {
    offset: u64,
    depth: u32,
    consumed: usize,
    carry: ScanCarry,
  },
}

#[inline]
fn is_primitive_byte(b: u8) -> bool {
  matches!(
    b,
    b'0'
      ..=b'9'
        | b'-'
        | b'+'
        | b'.'
        | b'e'
        | b'E'
        | b't'
        | b'r'
        | b'u'
        | b'f'
        | b'a'
        | b'l'
        | b's'
        | b'n'
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use bytes::Bytes;

  /// Build a [`ChunkWindow`] holding every chunk of `source`.
  fn window(source: &[u8], chunk_bytes: u64) -> ChunkWindow {
    let mut w = ChunkWindow::new(chunk_bytes, source.len() as u64);
    let mut off = 0u64;
    while (off as usize) < source.len() {
      let end = (off as usize + chunk_bytes as usize).min(source.len());
      w.insert(off, Bytes::copy_from_slice(&source[off as usize..end]));
      off += chunk_bytes;
    }
    w
  }

  #[test]
  fn byte_at_returns_chunk_byte() {
    let source = b"hello, world";
    let win = window(source, 64);
    let mut w = Walker::new(&win);
    assert_eq!(w.byte_at(0).unwrap(), Some(b'h'));
    assert_eq!(w.byte_at(7).unwrap(), Some(b'w'));
    assert_eq!(w.byte_at(12).unwrap(), None);
  }

  #[test]
  fn byte_at_pending_when_chunk_missing() {
    let win = ChunkWindow::new(64, 6);
    let mut w = Walker::new(&win);
    assert_eq!(w.byte_at(0).unwrap_err(), ChunkMiss(0));
  }

  #[test]
  fn next_string_close_finds_closing_quote() {
    let mut source = vec![b' '; 20];
    source[5] = b'"';
    source[6..11].copy_from_slice(b"hello");
    source[11] = b'"';
    let win = window(&source, 64);
    let mut w = Walker::new(&win);
    assert_eq!(w.next_string_close(6).unwrap(), Some(11));
  }

  #[test]
  fn next_string_close_with_escaped_inner_quote() {
    let source = b"  \"a\\\"b\"  ";
    let win = window(source, 64);
    let mut w = Walker::new(&win);
    // Interior at 3; the middle `"` at 5 is escaped.
    assert_eq!(w.next_string_close(3).unwrap(), Some(7));
  }

  #[test]
  fn next_string_close_across_chunks() {
    let mut source = vec![b'x'; 128];
    source[5] = b'"';
    source[100] = b'"';
    let win = window(&source, 64);
    let mut w = Walker::new(&win);
    assert_eq!(w.next_string_close(6).unwrap(), Some(100));
  }

  #[test]
  fn next_string_close_empty_string() {
    // `""` at offset 0: interior is the closing quote itself.
    let source = b"\"\"rest";
    let win = window(source, 64);
    let mut w = Walker::new(&win);
    assert_eq!(w.next_string_close(1).unwrap(), Some(1));
  }

  #[test]
  fn string_value_skip_resumes_across_faults_without_rescan() {
    // String interior spanning 3 chunks: the resumable StringScan must thread
    // the carry and not rescan the prefix.
    let chunk_bytes = 64usize;
    let mut source = vec![b'x'; chunk_bytes * 4];
    source[0] = b'"';
    let close_at = chunk_bytes * 3 + 10;
    source[close_at] = b'"';
    source.truncate(close_at + 1);

    let mut win = ChunkWindow::new(chunk_bytes as u64, source.len() as u64);
    let load = |win: &mut ChunkWindow, chunk_start: usize| {
      let end = (chunk_start + chunk_bytes).min(source.len());
      win.insert(
        chunk_start as u64,
        Bytes::copy_from_slice(&source[chunk_start..end]),
      );
    };
    load(&mut win, 0);
    let mut state = SkipState::start(0);

    // Faults forward chunk by chunk; state commits at block boundaries.
    for chunk_start in [64usize, 128, 192] {
      let err = skip_value_step(&mut Walker::new(&win), &mut state).unwrap_err();
      assert_eq!(err, TraverseError::Pending(ChunkMiss(chunk_start as u64)));
      load(&mut win, chunk_start);
    }
    let end = skip_value_step(&mut Walker::new(&win), &mut state).unwrap();
    assert_eq!(end, close_at as u64 + 1);
  }

  #[test]
  fn skip_value_container_resumes_after_chunk_miss() {
    // Flat `[...]` whose closer is in chunk 3; load chunks lazily.
    let chunk_bytes = 64usize;
    let mut source = Vec::new();
    source.push(b'[');
    source.resize(chunk_bytes * 3 + 1, b' ');
    source.push(b']');
    let close_at = source.len() - 1;

    let mut win = ChunkWindow::new(chunk_bytes as u64, source.len() as u64);
    let load = |win: &mut ChunkWindow, chunk_start: usize| {
      let end = (chunk_start + chunk_bytes).min(source.len());
      win.insert(
        chunk_start as u64,
        Bytes::copy_from_slice(&source[chunk_start..end]),
      );
    };
    load(&mut win, 0);
    let mut state = SkipState::start(0);

    for chunk_start in [64usize, 128, 192] {
      let err = skip_value_step(&mut Walker::new(&win), &mut state).unwrap_err();
      assert_eq!(err, TraverseError::Pending(ChunkMiss(chunk_start as u64)));
      load(&mut win, chunk_start);
    }
    let end = skip_value_step(&mut Walker::new(&win), &mut state).unwrap();
    assert_eq!(end, close_at as u64 + 1);
  }

  #[test]
  fn advance_commas_finds_target_element() {
    // [10,20,30,40] - advance 2 commas from just past `[`.
    let source = b"[10,20,30,40]";
    let win = window(source, 64);
    let mut w = Walker::new(&win);
    match w
      .advance_top_level_commas(1, 0, 2, ScanCarry::default(), None)
      .unwrap()
    {
      CommaStop::Found {
        offset_after_comma,
        consumed,
      } => {
        assert_eq!(consumed, 2);
        assert_eq!(
          &source[offset_after_comma as usize..offset_after_comma as usize + 2],
          b"30"
        );
      }
      other => panic!("expected Found, got {other:?}"),
    }
  }

  #[test]
  fn advance_commas_skips_nested_and_strings() {
    // Commas inside the nested object and the string must not count.
    let source = br#"[{"a":1,"b":2},"x,y",7]"#;
    let win = window(source, 64);
    let mut w = Walker::new(&win);
    // element 0 = object, element 1 = "x,y", element 2 = 7
    match w
      .advance_top_level_commas(1, 0, 2, ScanCarry::default(), None)
      .unwrap()
    {
      CommaStop::Found {
        offset_after_comma,
        consumed,
      } => {
        assert_eq!(consumed, 2);
        assert_eq!(source[offset_after_comma as usize], b'7');
      }
      other => panic!("expected Found, got {other:?}"),
    }
  }

  #[test]
  fn advance_commas_array_closed_before_target() {
    let source = b"[1,2]";
    let win = window(source, 64);
    let mut w = Walker::new(&win);
    assert_eq!(
      w.advance_top_level_commas(1, 0, 5, ScanCarry::default(), None)
        .unwrap(),
      CommaStop::ArrayClosed { consumed: 1 }
    );
  }

  #[test]
  fn advance_commas_resumes_across_chunks() {
    // Flat array spanning several chunks, loaded lazily like the session drive:
    // `Partial` commits progress, a propagated `ChunkMiss` is fetched and retried.
    let chunk_bytes = 64u64;
    let n = 200usize;
    let mut s = String::from("[");
    for i in 0..n {
      if i > 0 {
        s.push(',');
      }
      s.push('7');
    }
    s.push(']');
    let source = s.into_bytes();
    let target_index = 150usize;

    let mut win = ChunkWindow::new(chunk_bytes, source.len() as u64);
    let load = |win: &mut ChunkWindow, chunk_start: u64| {
      let end = (chunk_start + chunk_bytes).min(source.len() as u64) as usize;
      win.insert(
        chunk_start,
        Bytes::copy_from_slice(&source[chunk_start as usize..end]),
      );
    };
    load(&mut win, 0);

    let mut off = 1u64;
    let mut depth = 0u32;
    let mut carry = ScanCarry::default();
    let mut consumed_total = 0usize;
    let found;
    loop {
      let result = {
        let mut w = Walker::new(&win);
        w.advance_top_level_commas(off, depth, target_index - consumed_total, carry, None)
      };
      match result {
        Ok(CommaStop::Found {
          offset_after_comma, ..
        }) => {
          found = offset_after_comma;
          break;
        }
        Ok(CommaStop::Partial {
          offset,
          depth: d,
          consumed,
          carry: c,
        }) => {
          off = offset;
          depth = d;
          carry = c;
          consumed_total += consumed;
        }
        Ok(other) => panic!("unexpected {other:?}"),
        Err(TraverseError::Pending(ChunkMiss(m))) => load(&mut win, m),
        Err(e) => panic!("error {e:?}"),
      }
    }
    assert_eq!(
      source[found as usize], b'7',
      "element {target_index} is a digit"
    );
    let commas_before = source[..found as usize]
      .iter()
      .filter(|&&b| b == b',')
      .count();
    assert_eq!(
      commas_before, target_index,
      "landed past {target_index} commas"
    );
  }

  #[test]
  fn read_range_across_chunks() {
    let source: Vec<u8> = (0..200).map(|i| (i % 251) as u8).collect();
    let win = window(&source, 64);
    let mut w = Walker::new(&win);
    assert_eq!(
      w.read_range(50, 150).unwrap(),
      (50..150).map(|i| (i % 251) as u8).collect::<Vec<_>>()
    );
  }

  /// A flat `[7,7,...,7]` of `n` single-digit elements.
  fn flat_digit_array(n: usize) -> Vec<u8> {
    let mut s = String::from("[");
    for i in 0..n {
      if i > 0 {
        s.push(',');
      }
      s.push('7');
    }
    s.push(']');
    s.into_bytes()
  }

  #[test]
  fn advance_commas_samples_landmarks_at_stride_multiples() {
    let stride = 16usize;
    let chunk_bytes = 64u64;
    let source = flat_digit_array(400);
    let win = window(&source, chunk_bytes);
    let mut w = Walker::new(&win);

    let mut out: Vec<(usize, u64)> = Vec::new();
    let sink = LandmarkSink {
      base_index: 0,
      stride,
      out: &mut out,
    };
    match w
      .advance_top_level_commas(1, 0, 350, ScanCarry::default(), Some(sink))
      .unwrap()
    {
      CommaStop::Found { consumed, .. } => assert_eq!(consumed, 350),
      other => panic!("expected Found, got {other:?}"),
    }

    assert!(!out.is_empty(), "expected landmarks across the scan");
    let mut last_idx: Option<usize> = None;
    for &(idx, off) in &out {
      // Each landmark offset is an exact element start (a digit, not a comma).
      assert_eq!(
        source[off as usize], b'7',
        "landmark not at an element start"
      );
      let commas_before = source[..off as usize]
        .iter()
        .filter(|&&b| b == b',')
        .count();
      assert_eq!(idx, commas_before, "landmark index must match its offset");
      assert_eq!(
        idx % stride,
        0,
        "landmark index must land on the stride grid"
      );
      if let Some(prev) = last_idx {
        assert!(idx > prev, "landmark indices must strictly increase");
      }
      last_idx = Some(idx);
    }
  }

  #[test]
  fn advance_commas_sampling_survives_chunk_faults() {
    // The landmark set collected while faulting chunk-by-chunk must equal the
    // set collected with the whole document resident (no dup/missing landmarks
    // across a mid-scan `ChunkMiss`).
    let stride = 16usize;
    let chunk_bytes = 64u64;
    let n = 400usize;
    let source = flat_digit_array(n);
    let target = 350usize;

    let full = window(&source, chunk_bytes);
    let mut full_out: Vec<(usize, u64)> = Vec::new();
    {
      let mut w = Walker::new(&full);
      let sink = LandmarkSink {
        base_index: 0,
        stride,
        out: &mut full_out,
      };
      w.advance_top_level_commas(1, 0, target, ScanCarry::default(), Some(sink))
        .unwrap();
    }

    let mut win = ChunkWindow::new(chunk_bytes, source.len() as u64);
    let load = |win: &mut ChunkWindow, chunk_start: u64| {
      let end = (chunk_start + chunk_bytes).min(source.len() as u64) as usize;
      win.insert(
        chunk_start,
        Bytes::copy_from_slice(&source[chunk_start as usize..end]),
      );
    };
    load(&mut win, 0);

    let mut off = 1u64;
    let mut depth = 0u32;
    let mut carry = ScanCarry::default();
    let mut consumed_total = 0usize;
    let mut faulted_out: Vec<(usize, u64)> = Vec::new();
    loop {
      let result = {
        let mut w = Walker::new(&win);
        let sink = LandmarkSink {
          base_index: consumed_total,
          stride,
          out: &mut faulted_out,
        };
        w.advance_top_level_commas(off, depth, target - consumed_total, carry, Some(sink))
      };
      match result {
        Ok(CommaStop::Found { .. }) => break,
        Ok(CommaStop::Partial {
          offset,
          depth: d,
          consumed,
          carry: c,
        }) => {
          off = offset;
          depth = d;
          carry = c;
          consumed_total += consumed;
        }
        Ok(other) => panic!("unexpected {other:?}"),
        Err(TraverseError::Pending(ChunkMiss(m))) => load(&mut win, m),
        Err(e) => panic!("error {e:?}"),
      }
    }
    assert_eq!(
      faulted_out, full_out,
      "faulted landmark set must equal the resident-window set"
    );
  }
}
