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
    let byte = self
      .byte_at(from)?
      .ok_or(TraverseError::UnexpectedEof(from))?;
    match byte {
      b'"' => {
        let close = self
          .next_string_close(from + 1)?
          .ok_or(TraverseError::UnexpectedEof(from))?;
        Ok(close + 1)
      }
      b'{' => self.skip_container(from, Structural::LBrace, Structural::RBrace),
      b'[' => self.skip_container(from, Structural::LBracket, Structural::RBracket),
      _ => Ok(self.skip_primitive(from)?),
    }
  }

  /// Skip past a `{...}` or `[...]` value by tracking the open/close balance
  /// over the corresponding structural bitmaps. `from` must point at the
  /// opening brace/bracket.
  pub fn skip_container(
    &mut self,
    from: u64,
    open: Structural,
    close: Structural,
  ) -> Result<u64, TraverseError> {
    let mut depth: u32 = 1;
    let mut offset = from + 1;
    while offset < self.source_size {
      let co = self.chunk_offset_for(offset);
      self.ensure(co)?;
      let data = self.fetch(co)?;
      self.store.ensure_structural(co, data, open);
      self.store.ensure_structural(co, data, close);
      let bm = self.store.get(co).expect("ensured");
      let n_words = bm.n_words;
      let opens = bm.structural(open).expect("ensured");
      let closes = bm.structural(close).expect("ensured");

      let local = (offset - co) as usize;
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
        if c < depth {
          depth = depth + opens_w.count_ones() - c;
          continue;
        }
        let mut bits = opens_w | closes_w;
        while bits != 0 {
          let bit_idx = bits.trailing_zeros();
          let bit = 1u64 << bit_idx;
          let abs = co + (w * WINDOW + bit_idx as usize) as u64;
          if opens_w & bit != 0 {
            depth += 1;
          } else {
            depth = depth.checked_sub(1).ok_or(TraverseError::Malformed(abs))?;
            if depth == 0 {
              return Ok(abs + 1);
            }
          }
          bits &= bits - 1;
        }
      }
      offset = co + self.chunk_size;
    }
    Err(TraverseError::UnexpectedEof(offset))
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
}
