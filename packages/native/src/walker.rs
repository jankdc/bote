//! Walker: bitmap-driven traversal of a chunked JSON document.
//!
//! [`Walker`] is the seam between bitmap construction (sync, pure) and the
//! async source / chunk cache. It exposes synchronous primitives -
//! `byte_at`, `next_string_close`, `skip_value` - that consume already-
//! loaded chunks via a [`ChunkBytes`] source. When a primitive needs a chunk
//! that isn't loaded, it returns [`ChunkMiss`] with the offset to fetch; the
//! async caller pulls the chunk through the cache and retries.
//!
//! Bitmap construction is sequential: a chunk's `entry_carry` is the previous
//! chunk's `exit_carry`. [`Walker::ensure`] handles that chain by walking
//! back to the earliest chunk without bitmaps, then building forward,
//! returning `ChunkMiss` for the first unloaded chunk it encounters.

use thiserror::Error;

use crate::bitmap::{BitmapStore, ChunkBitmaps, Structural};
use crate::simd::{ScanCarry, WINDOW};

/// Returned when a primitive cannot proceed because a required chunk hasn't
/// been loaded. The async caller is expected to fetch the chunk at this
/// offset and retry the primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("chunk at offset {0} not loaded")]
pub struct ChunkMiss(pub u64);

/// Errors raised by higher-level traversal helpers (`skip_value`, container
/// balancing). `ChunkMiss` is folded in so callers can use `?` uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TraverseError {
  #[error("{0}")]
  Pending(#[from] ChunkMiss),
  #[error("unexpected end of input at offset {0}")]
  UnexpectedEof(u64),
  #[error("malformed JSON at offset {0}")]
  Malformed(u64),
}

/// Synchronous accessor for chunk byte data. Implementations look up chunks
/// by chunk-aligned offset and return `None` if not currently resident.
pub trait ChunkBytes {
  fn get_chunk(&self, chunk_offset: u64) -> Option<&[u8]>;
}

pub struct Walker<'a, P: ChunkBytes + ?Sized> {
  pub source_size: u64,
  pub chunk_size: u64,
  pub store: &'a mut BitmapStore,
  pub provider: &'a P,
  /// Cache of the most-recently-touched chunk's data. `byte_at`, `read_range`,
  /// and `ensure` all check this before going through the provider's HashMap.
  /// Consecutive accesses within the same chunk - the common case during
  /// array iteration - turn into a single comparison.
  cached: Option<CachedChunk<'a>>,
}

struct CachedChunk<'a> {
  offset: u64,
  end: u64,
  data: &'a [u8],
}

impl<'a, P: ChunkBytes + ?Sized> Walker<'a, P> {
  pub fn new(
    source_size: u64,
    chunk_size: u64,
    store: &'a mut BitmapStore,
    provider: &'a P,
  ) -> Self {
    Self {
      source_size,
      chunk_size,
      store,
      provider,
      cached: None,
    }
  }

  #[inline]
  pub fn chunk_offset_for(&self, offset: u64) -> u64 {
    (offset / self.chunk_size) * self.chunk_size
  }

  /// Get the chunk data containing `offset`, returning `(data, local_offset)`.
  /// Hits the provider only when the requested offset isn't in the cached
  /// chunk's range. The returned `&'a [u8]` borrows from the provider, not
  /// from `self`, so the `&mut self` borrow is released on return.
  #[inline]
  fn locate(&mut self, offset: u64) -> Result<(&'a [u8], usize), ChunkMiss> {
    if let Some(c) = &self.cached {
      if offset >= c.offset && offset < c.end {
        return Ok((c.data, (offset - c.offset) as usize));
      }
    }
    let co = self.chunk_offset_for(offset);
    let data = self.fetch_slow(co)?;
    Ok((data, (offset - co) as usize))
  }

  /// Get the bytes of the chunk at chunk-aligned `chunk_offset`. Hot-path
  /// inline check against the cache; cold provider call on miss.
  #[inline]
  fn fetch(&mut self, chunk_offset: u64) -> Result<&'a [u8], ChunkMiss> {
    if let Some(c) = &self.cached {
      if c.offset == chunk_offset {
        return Ok(c.data);
      }
    }
    self.fetch_slow(chunk_offset)
  }

  #[cold]
  fn fetch_slow(&mut self, chunk_offset: u64) -> Result<&'a [u8], ChunkMiss> {
    let data = self
      .provider
      .get_chunk(chunk_offset)
      .ok_or(ChunkMiss(chunk_offset))?;
    self.cached = Some(CachedChunk {
      offset: chunk_offset,
      end: chunk_offset + data.len() as u64,
      data,
    });
    Ok(data)
  }

  /// Ensure bitmaps for `chunk_offset` are materialized AND the underlying
  /// chunk bytes are currently resident in the provider, chaining carries
  /// from the start of the source.
  ///
  /// We probe the provider even when bitmaps are already cached because the
  /// BitmapStore is session-wide and persists across queries, while pinned
  /// chunk bytes are per-query. After a prior query released its pins, the
  /// chunk may have been evicted while its bitmaps remained cached - the
  /// caller must fetch the chunk before downstream operations that read
  /// raw bytes (`read_range`, `ensure_structural`) try to use it.
  pub fn ensure(&mut self, chunk_offset: u64) -> Result<(), ChunkMiss> {
    // Probe the provider via the cache (cheap on the hot path) so we surface
    // ChunkMiss early - see the doc comment above.
    self.fetch(chunk_offset)?;
    if self.store.get(chunk_offset).is_some() {
      return Ok(());
    }
    // Walk back to the earliest chunk lacking bitmaps; build forward from
    // there, threading exit_carry into the next chunk's entry. Iterative to
    // avoid blowing the stack on large sources.
    let mut start = chunk_offset;
    while start > 0 {
      let prev = start - self.chunk_size;
      if self.store.get(prev).is_some() {
        break;
      }
      start = prev;
    }
    let mut entry = if start == 0 {
      ScanCarry::default()
    } else {
      self
        .store
        .get(start - self.chunk_size)
        .expect("loop invariant")
        .exit_carry()
    };
    let mut co = start;
    loop {
      let data = self.fetch(co)?;
      let bm = ChunkBitmaps::build_basic(data, entry);
      entry = bm.exit_carry();
      self.store.insert(co, bm);
      if co == chunk_offset {
        return Ok(());
      }
      co += self.chunk_size;
    }
  }

  /// Return the byte at `offset`, or `None` if past end-of-source. Requires
  /// the chunk containing `offset` to be loaded but does not build bitmaps.
  #[inline]
  pub fn byte_at(&mut self, offset: u64) -> Result<Option<u8>, ChunkMiss> {
    if offset >= self.source_size {
      return Ok(None);
    }
    let (data, local) = self.locate(offset)?;
    Ok(data.get(local).copied())
  }

  /// Inside-string scan: find the offset of the closing `"` for the string
  /// whose interior begins at `from` (i.e. `from` should be one past the
  /// opening quote, or the first byte of a string carried over from a prior
  /// chunk). The closing quote is the next byte where `in_string` is 0.
  pub fn next_string_close(&mut self, from: u64) -> Result<Option<u64>, ChunkMiss> {
    let mut offset = from;
    while offset < self.source_size {
      let co = self.chunk_offset_for(offset);
      self.ensure(co)?;
      let chunk_len = self.fetch(co)?.len();
      let bm = self.store.get(co).expect("ensured");
      let local = (offset - co) as usize;
      let from_word = local / WINDOW;
      let from_bit = local % WINDOW;
      if from_word < bm.n_words {
        // Bytes where in_string is 0 are outside-string positions. We want
        // the first such position at or after (from_word, from_bit). The
        // partial tail of the chunk is space-padded; mask it off so we don't
        // return a phantom close offset past end-of-chunk-data.
        let cap = co + chunk_len as u64;
        let pos = scan_first_zero_in(&bm.in_string, from_word, from_bit, co, cap);
        if let Some(p) = pos {
          return Ok(Some(p));
        }
      }
      offset = co + self.chunk_size;
    }
    Ok(None)
  }

  /// Copy bytes in `[from, to)` out of loaded chunks into an owned buffer.
  /// Returns `ChunkMiss` for the first chunk that isn't resident.
  pub fn read_range(&mut self, from: u64, to: u64) -> Result<Vec<u8>, ChunkMiss> {
    let cap = (to.saturating_sub(from)) as usize;
    let mut out = Vec::with_capacity(cap);
    let mut offset = from;
    let end = to.min(self.source_size);
    while offset < end {
      let co = self.chunk_offset_for(offset);
      let data = self.fetch(co)?;
      let local_start = (offset - co) as usize;
      let chunk_end = co + (data.len() as u64);
      let local_end = (end.min(co + self.chunk_size).min(chunk_end) - co) as usize;
      if local_end > local_start {
        out.extend_from_slice(&data[local_start..local_end]);
      }
      offset = co + self.chunk_size;
    }
    Ok(out)
  }

  /// Skip ASCII whitespace (`' '`, `'\t'`, `'\n'`, `'\r'`) starting at
  /// `from`. Returns the offset of the first non-whitespace byte or
  /// `source_size` if the input ends in whitespace.
  #[inline]
  pub fn skip_whitespace(&mut self, from: u64) -> Result<u64, ChunkMiss> {
    let mut offset = from;
    while offset < self.source_size {
      match self.byte_at(offset)? {
        None => return Ok(offset),
        Some(b' ' | b'\t' | b'\n' | b'\r') => offset += 1,
        Some(_) => return Ok(offset),
      }
    }
    Ok(offset)
  }

  /// Skip a JSON primitive (number, `true`, `false`, `null`) starting at
  /// `from` whose first byte has already been determined to belong to a
  /// primitive. Returns the offset of the first byte not part of the
  /// primitive's lexical form.
  pub fn skip_primitive(&mut self, from: u64) -> Result<u64, ChunkMiss> {
    let mut offset = from;
    while offset < self.source_size {
      match self.byte_at(offset)? {
        None => return Ok(offset),
        Some(b) if is_primitive_byte(b) => offset += 1,
        Some(_) => return Ok(offset),
      }
    }
    Ok(offset)
  }

  /// Skip past a JSON value starting at `from` (which must point at the
  /// value's first byte, whitespace already consumed). Returns the offset
  /// of the byte immediately after the value.
  pub fn skip_value(&mut self, from: u64) -> Result<u64, TraverseError> {
    let mut state = SkipState::start(from);
    skip_value_step(self, &mut state)
  }

  pub fn skip_container_step(
    &mut self,
    state: &mut ContainerSkipState,
  ) -> Result<u64, TraverseError> {
    let open = state.open;
    let close = state.close;
    while state.offset < self.source_size {
      let co = self.chunk_offset_for(state.offset);
      // ensure() / fetch() may return ChunkMiss; `state` is already at the
      // chunk-boundary commit point from the previous iteration, so the
      // `?` propagates without losing progress.
      self.ensure(co)?;
      let data = self.fetch(co)?;
      self.store.ensure_structural(co, data, open);
      self.store.ensure_structural(co, data, close);
      let bm = self.store.get(co).expect("ensured");
      let n_words = bm.n_words;
      let opens = bm.structural(open).expect("ensured");
      let closes = bm.structural(close).expect("ensured");

      let local = (state.offset - co) as usize;
      let from_word = local / WINDOW;
      let from_bit = local % WINDOW;

      for w in from_word..n_words {
        let mask = if w == from_word {
          word_mask_from(from_bit)
        } else {
          !0u64
        };
        let opens_w = opens[w] & mask;
        let closes_w = closes[w] & mask;
        let c = closes_w.count_ones();
        // Net-popcount fast path: if the word's closes can't exhaust the
        // current depth even when stacked first, depth cannot hit zero in
        // this word and we can bulk-update without walking individual bits.
        if c < state.depth {
          state.depth = state.depth + opens_w.count_ones() - c;
          continue;
        }
        let mut bits = opens_w | closes_w;
        while bits != 0 {
          let bit_idx = bits.trailing_zeros();
          let bit = 1u64 << bit_idx;
          let abs = co + (w * WINDOW + bit_idx as usize) as u64;
          if opens_w & bit != 0 {
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
      state.offset = co + self.chunk_size;
    }
    Err(TraverseError::UnexpectedEof(state.offset))
  }

  /// Advance past `needed` depth-0 commas of the array currently being
  /// scanned, returning the offset one byte past the last consumed comma -
  /// i.e. the first byte of the next element. Caller passes the position
  /// of the current element (one past `[` on first entry, or one past the
  /// previous element's terminating `,` on a mid-scan resume) and the
  /// current nesting depth relative to that array (0 on first entry).
  ///
  /// Returns:
  ///   - `Found { offset_after_comma, consumed }` - `consumed == needed`,
  ///     state should be advanced to `offset_after_comma`.
  ///   - `ArrayClosed { consumed }` - the array's terminating `]` was hit
  ///     before consuming `needed` commas; the target index doesn't exist.
  ///   - `Partial { offset, depth, consumed }` - chunk-boundary commit
  ///     point; caller should update its state with these values and
  ///     resume. Used so a `ChunkMiss` mid-scan doesn't lose progress.
  ///
  /// Bitmap scan: ORs the `{`/`[` opens and `}`/`]` closes per 64-bit word
  /// and tracks depth bit-by-bit; depth-0 commas in the comma bitmap are
  /// element boundaries. The depth-0 fast path skips the bit walk entirely
  /// when a word has no opens or closes - at top level, every comma is a
  /// boundary and we can popcount and skip whole words.
  pub fn advance_top_level_commas(
    &mut self,
    from: u64,
    initial_depth: u32,
    needed: usize,
  ) -> Result<AdvanceCommas, TraverseError> {
    if needed == 0 {
      return Ok(AdvanceCommas::Found {
        offset_after_comma: from,
        consumed: 0,
      });
    }
    let mut depth = initial_depth;
    let mut remaining = needed;
    let mut consumed: usize = 0;
    let mut offset = from;
    while offset < self.source_size {
      let co = self.chunk_offset_for(offset);
      self.ensure(co)?;
      let data = self.fetch(co)?;
      self.store.ensure_structural(co, data, Structural::LBrace);
      self.store.ensure_structural(co, data, Structural::RBrace);
      self.store.ensure_structural(co, data, Structural::LBracket);
      self.store.ensure_structural(co, data, Structural::RBracket);
      self.store.ensure_structural(co, data, Structural::Comma);
      let bm = self.store.get(co).expect("ensured");
      let n_words = bm.n_words;
      let lbrace = bm.structural(Structural::LBrace).expect("ensured");
      let rbrace = bm.structural(Structural::RBrace).expect("ensured");
      let lbracket = bm.structural(Structural::LBracket).expect("ensured");
      let rbracket = bm.structural(Structural::RBracket).expect("ensured");
      let comma = bm.structural(Structural::Comma).expect("ensured");

      let local = (offset - co) as usize;
      let from_word = local / WINDOW;
      let from_bit = local % WINDOW;

      for w in from_word..n_words {
        let mask = if w == from_word {
          word_mask_from(from_bit)
        } else {
          !0u64
        };
        let opens_w = (lbrace[w] | lbracket[w]) & mask;
        let closes_w = (rbrace[w] | rbracket[w]) & mask;
        let commas_w = comma[w] & mask;

        // Depth-0 fast path: with no nesting transitions in this word and
        // depth already 0, every comma bit is an element boundary. Popcount
        // tells us how many we can skip in bulk; if more than `remaining`
        // are present, isolate the `remaining`th bit via repeated
        // lowest-bit-clear.
        if depth == 0 && opens_w == 0 && closes_w == 0 {
          let c = commas_w.count_ones() as usize;
          if c < remaining {
            remaining -= c;
            consumed += c;
            continue;
          }
          let mut bits = commas_w;
          for _ in 0..remaining - 1 {
            bits &= bits - 1;
          }
          let bit_idx = bits.trailing_zeros() as usize;
          let abs = co + (w * WINDOW + bit_idx) as u64;
          consumed += remaining;
          return Ok(AdvanceCommas::Found {
            offset_after_comma: abs + 1,
            consumed,
          });
        }

        // General path: depth changes inside this word, so walk relevant
        // structural bits in offset order.
        let mut bits = opens_w | closes_w | commas_w;
        while bits != 0 {
          let bit_idx = bits.trailing_zeros() as usize;
          let bit = 1u64 << bit_idx;
          let abs = co + (w * WINDOW + bit_idx) as u64;
          if opens_w & bit != 0 {
            depth += 1;
          } else if closes_w & bit != 0 {
            if depth == 0 {
              // The array's own `]` - target index doesn't exist.
              return Ok(AdvanceCommas::ArrayClosed { consumed });
            }
            depth -= 1;
          } else {
            // Comma; only counts as an element boundary at depth 0.
            if depth == 0 {
              remaining -= 1;
              consumed += 1;
              if remaining == 0 {
                return Ok(AdvanceCommas::Found {
                  offset_after_comma: abs + 1,
                  consumed,
                });
              }
            }
          }
          bits &= bits - 1;
        }
      }
      // Chunk fully scanned without resolution. Commit the chunk-boundary
      // state so a ChunkMiss on the next iteration doesn't lose work.
      let next_offset = co + self.chunk_size;
      if next_offset >= self.source_size {
        break;
      }
      // Probe the next chunk before re-looping so a miss surfaces here
      // with `Partial` state, not by re-running the just-completed chunk.
      if self.provider.get_chunk(next_offset).is_none() {
        return Ok(AdvanceCommas::Partial {
          offset: next_offset,
          depth,
          consumed,
        });
      }
      offset = next_offset;
    }
    Err(TraverseError::UnexpectedEof(offset))
  }
}

/// Resumable state for [`skip_value_step`]. Persisted across `ChunkMiss`
/// retries by [`crate::session::Session::drive`] so a long skip survives
/// chunk faults without restarting from the value's first byte.
///
/// Lifecycle: created via [`SkipState::start`]; the first `skip_value_step`
/// call peeks the value's opening byte to commit a [`SkipKind`]; subsequent
/// calls (after `ChunkMiss` resumption) re-enter the kind-specific step
/// with `(offset, depth)` already at the last committed boundary.
#[derive(Debug, Clone)]
pub struct SkipState {
  kind: SkipKind,
}

#[derive(Debug, Clone)]
enum SkipKind {
  /// First-call state: still need to read the opening byte at `from` to
  /// decide which kind we're skipping.
  Pending { from: u64 },
  /// Skipping a `"..."` string: `interior` is one past the opening quote.
  /// Non-resumable in detail (a chunk fault re-runs `next_string_close`
  /// from `interior`), but bounded by string length.
  String { interior: u64 },
  /// Skipping a JSON primitive (number, `true`, `false`, `null`).
  /// Non-resumable in detail; bounded by primitive length.
  Primitive { offset: u64 },
  /// Skipping a `{...}` or `[...]` container. Resumable: `state` is the
  /// container scan state committed to the last chunk boundary.
  Container(ContainerSkipState),
}

/// Resumable state for skipping a JSON container, used by both
/// [`Walker::skip_container_step`] (per chunk) and [`SkipState`] (per
/// value). Mirrors the `(offset, depth)` shape of
/// [`ResolveState::ArrayLoopState`](crate::resolve::ResolveState) and
/// `CountState`.
#[derive(Debug, Clone, Copy)]
pub struct ContainerSkipState {
  /// Next byte to scan. Committed to a chunk boundary before any
  /// `ChunkMiss` propagates, so resumption picks up here.
  pub offset: u64,
  /// Nesting depth at `offset`, relative to the container being skipped.
  pub depth: u32,
  pub open: Structural,
  pub close: Structural,
}

impl SkipState {
  /// Start a skip at `from` (which must point at the value's first byte,
  /// whitespace already consumed).
  pub fn start(from: u64) -> Self {
    Self {
      kind: SkipKind::Pending { from },
    }
  }
}

/// Drive a [`SkipState`] forward against the current chunks. Returns the
/// offset of the byte immediately after the value, or propagates
/// `ChunkMiss` (via `?`) with `state` committed so the next call resumes.
///
/// Wrap in [`Session::drive`](crate::session::Session::drive) (see
/// [`Session::skip_value_at`](crate::session::Session::skip_value_at))
/// for the async fault-and-retry plumbing.
pub fn skip_value_step<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  state: &mut SkipState,
) -> Result<u64, TraverseError> {
  // First entry per value: read the opener and commit a concrete kind so
  // any subsequent ChunkMiss can resume without re-classifying.
  if let SkipKind::Pending { from } = state.kind {
    let byte = walker
      .byte_at(from)?
      .ok_or(TraverseError::UnexpectedEof(from))?;
    state.kind = match byte {
      b'"' => SkipKind::String { interior: from + 1 },
      b'{' => SkipKind::Container(ContainerSkipState {
        offset: from + 1,
        depth: 1,
        open: Structural::LBrace,
        close: Structural::RBrace,
      }),
      b'[' => SkipKind::Container(ContainerSkipState {
        offset: from + 1,
        depth: 1,
        open: Structural::LBracket,
        close: Structural::RBracket,
      }),
      _ => SkipKind::Primitive { offset: from },
    };
  }
  match &mut state.kind {
    SkipKind::Pending { .. } => unreachable!("committed above"),
    SkipKind::String { interior } => {
      let close = walker
        .next_string_close(*interior)?
        .ok_or(TraverseError::UnexpectedEof(*interior))?;
      Ok(close + 1)
    }
    SkipKind::Primitive { offset } => Ok(walker.skip_primitive(*offset)?),
    SkipKind::Container(c) => walker.skip_container_step(c),
  }
}

/// Outcome of [`Walker::advance_top_level_commas`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceCommas {
  /// Target reached: `consumed == needed`, `offset_after_comma` is the
  /// first byte of the target element (before whitespace).
  Found {
    offset_after_comma: u64,
    consumed: usize,
  },
  /// Array's terminating `]` reached before consuming `needed` commas.
  ArrayClosed { consumed: usize },
  /// Chunk-boundary commit; caller resumes with these values once the
  /// next chunk is loaded. Surfaces ahead of a ChunkMiss so the caller
  /// doesn't lose comma counts already accumulated.
  Partial {
    offset: u64,
    depth: u32,
    consumed: usize,
  },
}

#[inline]
fn word_mask_from(bit: usize) -> u64 {
  if bit >= 64 {
    0
  } else {
    !0u64 << bit
  }
}

/// Find the first 0-bit in `in_string` at or after `(from_word, from_bit)`,
/// clamped so we never return an offset past `cap` (the chunk's real data
/// end). Returns the absolute offset of the first such bit, or `None`.
fn scan_first_zero_in(
  in_string: &[u64],
  from_word: usize,
  from_bit: usize,
  chunk_offset: u64,
  cap: u64,
) -> Option<u64> {
  let head = !in_string[from_word] & word_mask_from(from_bit);
  if head != 0 {
    let bit = head.trailing_zeros() as usize;
    let abs = chunk_offset + (from_word * WINDOW + bit) as u64;
    if abs < cap {
      return Some(abs);
    }
    return None;
  }
  for (w, &word) in in_string.iter().enumerate().skip(from_word + 1) {
    let m = !word;
    if m != 0 {
      let bit = m.trailing_zeros() as usize;
      let abs = chunk_offset + (w * WINDOW + bit) as u64;
      if abs < cap {
        return Some(abs);
      }
      return None;
    }
  }
  None
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
  use std::collections::HashMap;

  /// In-memory provider used by tests. Owns the chunk bytes.
  struct MemoryProvider {
    chunks: HashMap<u64, Vec<u8>>,
  }

  impl ChunkBytes for MemoryProvider {
    fn get_chunk(&self, chunk_offset: u64) -> Option<&[u8]> {
      self.chunks.get(&chunk_offset).map(Vec::as_slice)
    }
  }

  fn chunked(source: &[u8], chunk_size: usize) -> MemoryProvider {
    let mut chunks = HashMap::new();
    let mut offset = 0u64;
    while (offset as usize) < source.len() {
      let end = (offset as usize + chunk_size).min(source.len());
      chunks.insert(offset, source[offset as usize..end].to_vec());
      offset += chunk_size as u64;
    }
    MemoryProvider { chunks }
  }

  fn walker<'a>(
    provider: &'a MemoryProvider,
    store: &'a mut BitmapStore,
    source: &[u8],
    chunk_size: u64,
  ) -> Walker<'a, MemoryProvider> {
    Walker::new(source.len() as u64, chunk_size, store, provider)
  }

  #[test]
  fn byte_at_returns_chunk_byte() {
    let source = b"hello, world";
    let provider = chunked(source, 64);
    let mut store = BitmapStore::new();
    let mut walker = walker(&provider, &mut store, source, 64);
    assert_eq!(walker.byte_at(0).unwrap(), Some(b'h'));
    assert_eq!(walker.byte_at(7).unwrap(), Some(b'w'));
    assert_eq!(walker.byte_at(12).unwrap(), None);
  }

  #[test]
  fn byte_at_pending_when_chunk_missing() {
    let provider = MemoryProvider {
      chunks: HashMap::new(),
    };
    let mut store = BitmapStore::new();
    let mut walker = walker(&provider, &mut store, b"unused", 64);
    let err = walker.byte_at(0).unwrap_err();
    assert_eq!(err, ChunkMiss(0));
  }

  #[test]
  fn carry_chains_across_chunks() {
    // Build a source where chunk 0 ends mid-string and chunk 1 closes it.
    // If carries aren't chained, chunk 1's bitmaps would treat the opening
    // bytes as outside-string and find spurious structural chars.
    let mut source = vec![b'x'; 128];
    source[10] = b'"'; // open string at byte 10
    source[70] = b'"'; // close string at byte 70 (in chunk 1)
    let provider = chunked(&source, 64);
    let mut store = BitmapStore::new();
    let mut w = walker(&provider, &mut store, &source, 64);
    // String must terminate at byte 70 - if carries don't chain, chunk 1
    // would treat opening bytes as outside-string and the close would be
    // found at a wrong offset.
    assert_eq!(w.next_string_close(11).unwrap(), Some(70));
  }

  #[test]
  fn next_string_close_finds_closing_quote() {
    // String: `"hello"` at offset 5.
    let mut source = vec![b' '; 20];
    source[5] = b'"';
    source[6..11].copy_from_slice(b"hello");
    source[11] = b'"';
    let provider = chunked(&source, 64);
    let mut store = BitmapStore::new();
    let mut w = walker(&provider, &mut store, &source, 64);
    // We're given the position one past the opening quote.
    assert_eq!(w.next_string_close(6).unwrap(), Some(11));
  }

  #[test]
  fn next_string_close_with_escaped_inner_quote() {
    let source = b"  \"a\\\"b\"  ";
    let provider = chunked(source, 64);
    let mut store = BitmapStore::new();
    let mut w = walker(&provider, &mut store, source, 64);
    // String starts at offset 2; interior starts at 3. The middle `"` at
    // offset 5 is escaped; the real close is at offset 7.
    assert_eq!(w.next_string_close(3).unwrap(), Some(7));
  }

  #[test]
  fn next_string_close_across_chunks() {
    let mut source = vec![b'x'; 128];
    source[5] = b'"'; // open string at offset 5
                      // bytes 6..100 are 'x' inside string
    source[100] = b'"'; // close at offset 100 (chunk 1)
    let provider = chunked(&source, 64);
    let mut store = BitmapStore::new();
    let mut w = walker(&provider, &mut store, &source, 64);
    assert_eq!(w.next_string_close(6).unwrap(), Some(100));
  }

  #[test]
  fn word_mask_from_boundaries() {
    assert_eq!(word_mask_from(0), !0u64);
    assert_eq!(word_mask_from(1), !0u64 << 1);
    assert_eq!(word_mask_from(63), 1u64 << 63);
    assert_eq!(word_mask_from(64), 0);
    assert_eq!(word_mask_from(100), 0);
  }

  #[test]
  fn skip_value_step_resumes_after_chunk_miss() {
    // A flat `[...]` whose interior is 3 chunks long, so the closing `]`
    // lives in chunk 3. We load only chunk 0 initially and verify that
    // ChunkMiss commits state to chunk 1's boundary, then load chunk 1
    // and confirm we don't re-scan chunk 0.
    let chunk_size = 64usize;
    let mut source = Vec::with_capacity(chunk_size * 4);
    source.push(b'[');
    source.resize(chunk_size * 3 + 1, b' '); // pad with whitespace through chunk 2
    source.push(b']'); // closer in chunk 3 at offset 193
    let close_at = source.len() - 1;
    assert_eq!(close_at, chunk_size * 3 + 1);

    // Provider initially holds chunks 0 only. We add more between calls.
    let mut provider = MemoryProvider {
      chunks: HashMap::new(),
    };
    let load = |provider: &mut MemoryProvider, source: &[u8], co: usize| {
      let end = (co + chunk_size).min(source.len());
      provider.chunks.insert(co as u64, source[co..end].to_vec());
    };
    load(&mut provider, &source, 0);

    let mut store = BitmapStore::new();
    let mut state = SkipState::start(0);

    // First call: makes progress through chunk 0, then faults on chunk 64.
    let err = {
      let mut w = walker(&provider, &mut store, &source, chunk_size as u64);
      skip_value_step(&mut w, &mut state).unwrap_err()
    };
    assert_eq!(err, TraverseError::Pending(ChunkMiss(64)));
    // State must be committed to chunk 0's boundary (offset 64) - any value
    // lower would mean a retry re-scans bytes already inspected.
    match &state.kind {
      SkipKind::Container(c) => assert!(
        c.offset >= 64,
        "expected commit at chunk boundary 64, got offset={} depth={}",
        c.offset,
        c.depth
      ),
      _ => panic!("expected Container kind after opener seen"),
    }

    // Load chunk 1 and resume; should fault on chunk 128 next.
    load(&mut provider, &source, 64);
    let err = {
      let mut w = walker(&provider, &mut store, &source, chunk_size as u64);
      skip_value_step(&mut w, &mut state).unwrap_err()
    };
    assert_eq!(err, TraverseError::Pending(ChunkMiss(128)));

    // Load chunks 2 and 3; final call should complete at the `]` past it.
    load(&mut provider, &source, 128);
    load(&mut provider, &source, 192);
    let end = {
      let mut w = walker(&provider, &mut store, &source, chunk_size as u64);
      skip_value_step(&mut w, &mut state).unwrap()
    };
    assert_eq!(end, close_at as u64 + 1);
  }

  #[test]
  fn ensure_back_walk_stops_at_nearest_cached_chunk() {
    // `ensure(co)` walks back to the earliest chunk lacking bitmaps and builds
    // forward, threading carries. After bitmaps for some prefix are present,
    // a later ensure must NOT re-walk past the cached frontier - that's the
    // no-quadratic-rebuild property under steady-state eviction.
    let chunk_size: usize = 64;
    let source: Vec<u8> = vec![b'x'; chunk_size * 10];
    let provider = chunked(&source, chunk_size);
    let mut store = BitmapStore::new();

    // First call seeds chunks 0..=5 (back-walk reaches all the way to chunk 0
    // because the store is empty).
    {
      let mut w = walker(&provider, &mut store, &source, chunk_size as u64);
      w.ensure(5 * chunk_size as u64).unwrap();
    }
    for i in 0..=5u64 {
      assert!(
        store.get(i * chunk_size as u64).is_some(),
        "chunk {i} bitmaps should be built after ensure(5)",
      );
    }
    for i in 6..10u64 {
      assert!(
        store.get(i * chunk_size as u64).is_none(),
        "chunk {i} must not be built yet",
      );
    }

    // Second call: ensure(7). Back-walk hits cached chunk 5 immediately and
    // builds only 6 and 7. Chunks 8 and 9 must stay unbuilt.
    {
      let mut w = walker(&provider, &mut store, &source, chunk_size as u64);
      w.ensure(7 * chunk_size as u64).unwrap();
    }
    assert!(
      store.get(6 * chunk_size as u64).is_some(),
      "chunk 6 should be built forward from cached chunk 5",
    );
    assert!(
      store.get(7 * chunk_size as u64).is_some(),
      "chunk 7 should be built (the target)",
    );
    assert!(
      store.get(8 * chunk_size as u64).is_none(),
      "chunk 8 must not be built (past target)",
    );
    assert!(
      store.get(9 * chunk_size as u64).is_none(),
      "chunk 9 must not be built (past target)",
    );
  }
}
