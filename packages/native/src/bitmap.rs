//! Per-chunk bitmap construction.
//!
//! A chunk is a fixed-size contiguous slice of source bytes.
//! Within a chunk, we process 64-byte windows back-to-back, chaining
//! [`ScanCarry`] from each window to the next.
//!
//! The basic bitmaps (`quote`, `in_string`) are always built up front because
//! the structural bitmaps depend on `in_string` to mask out characters that
//! happen to appear inside string literals. The structural bitmaps
//! themselves (`:`, `,`, `{`, `}`, `[`, `]`) are built **lazily**: each kind
//! is computed and cached on first access via [`ChunkBitmaps::ensure_structural`].

use std::collections::HashMap;
use std::simd::cmp::SimdPartialEq;
use std::simd::Simd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::simd::{scan_block, ScanCarry, WINDOW};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum Structural {
  // for diagnostic purposes only
  Colon = 0,
  Comma = 1,
  LBrace = 2,
  RBrace = 3,
  LBracket = 4,
  RBracket = 5,
}

impl Structural {
  const COUNT: usize = 6;
  #[cfg(test)]
  const ALL: [Structural; Self::COUNT] = [
    Structural::Colon,
    Structural::Comma,
    Structural::LBrace,
    Structural::RBrace,
    Structural::LBracket,
    Structural::RBracket,
  ];

  pub fn byte(self) -> u8 {
    match self {
      Self::Colon => b':',
      Self::Comma => b',',
      Self::LBrace => b'{',
      Self::RBrace => b'}',
      Self::LBracket => b'[',
      Self::RBracket => b']',
    }
  }
}

/// Bitmaps for a single chunk. Each bitmap is one `u64` per 64-byte window.
pub struct ChunkBitmaps {
  pub n_words: usize,
  pub in_string: Box<[u64]>,
  exit_carry: ScanCarry,
  structural: [Option<Box<[u64]>>; Structural::COUNT],
}

impl ChunkBitmaps {
  /// Build the basic `in_string` bitmap for a chunk, chaining from
  /// `entry_carry`. Structural bitmaps are deferred to first access.
  pub fn build_basic(chunk: &[u8], entry_carry: ScanCarry) -> Self {
    let n_words = chunk.len().div_ceil(WINDOW);
    let mut in_string = vec![0u64; n_words].into_boxed_slice();
    let mut carry = entry_carry;
    for (w, slot) in in_string.iter_mut().enumerate() {
      let window = window_at(chunk, w);
      let (bits, next) = scan_block(&window, carry);
      *slot = bits;
      carry = next;
    }
    Self {
      n_words,
      exit_carry: carry,
      in_string,
      structural: Default::default(),
    }
  }

  /// Carry-out from this chunk's bitmaps; feed it into the next chunk's
  /// `build_basic` to preserve string-mask continuity.
  pub fn exit_carry(&self) -> ScanCarry {
    self.exit_carry
  }

  /// Return the structural bitmap for `kind`, building it on first access.
  pub fn ensure_structural(&mut self, chunk: &[u8], kind: Structural) -> &[u64] {
    self.structural[kind as usize]
      .get_or_insert_with(|| build_structural(chunk, &self.in_string, kind))
  }

  #[cfg(test)]
  fn has_structural(&self, kind: Structural) -> bool {
    self.structural[kind as usize].is_some()
  }

  /// Immutable accessor for a structural bitmap that's already been built.
  /// Returns `None` if [`ensure_structural`] hasn't been called for `kind`.
  pub fn structural(&self, kind: Structural) -> Option<&[u64]> {
    self.structural[kind as usize].as_deref()
  }

  /// Total bytes of bitmap storage currently held by this chunk
  /// (in_string + each built structural).
  pub fn bytes(&self) -> usize {
    let basic = self.in_string.len() * 8;
    let structural: usize = self
      .structural
      .iter()
      .filter_map(|s| s.as_deref().map(|b| b.len() * 8))
      .sum();
    basic + structural
  }
}

fn build_structural(chunk: &[u8], in_string: &[u64], kind: Structural) -> Box<[u64]> {
  let byte = kind.byte();
  let n = in_string.len();
  let mut out = vec![0u64; n].into_boxed_slice();
  for (w, slot) in out.iter_mut().enumerate() {
    let window = window_at(chunk, w);
    let v: Simd<u8, WINDOW> = Simd::from_array(window);
    let raw = v.simd_eq(Simd::splat(byte)).to_bitmask();
    *slot = raw & !in_string[w];
  }
  out
}

/// Copy window `w` out of `chunk` into a fixed-size array, padding the tail
/// with spaces when the chunk doesn't fill the window.
fn window_at(chunk: &[u8], w: usize) -> [u8; WINDOW] {
  let mut window = [b' '; WINDOW];
  let start = w * WINDOW;
  let end = chunk.len().min(start + WINDOW);
  if end > start {
    window[..end - start].copy_from_slice(&chunk[start..end]);
  }
  window
}

/// Session-wide cache mapping chunk offsets to their bitmap data.
///
/// The traversal layer feeds chunks through here keyed by absolute offset;
/// repeated visits to the same chunk pay the SIMD scan cost only once.
///
/// The store maintains a running byte counter behind an `Arc<AtomicUsize>`;
/// the chunk cache shares this handle so its eviction loop can see bitmap
/// growth without re-entering the bitmap store's lock.
pub struct BitmapStore {
  chunks: HashMap<u64, ChunkBitmaps>,
  bytes_counter: Arc<AtomicUsize>,
}

impl Default for BitmapStore {
  fn default() -> Self {
    Self::new()
  }
}

impl BitmapStore {
  pub fn new() -> Self {
    Self::with_bytes_counter(Arc::new(AtomicUsize::new(0)))
  }

  pub fn with_bytes_counter(bytes_counter: Arc<AtomicUsize>) -> Self {
    Self {
      chunks: HashMap::new(),
      bytes_counter,
    }
  }

  pub fn get(&self, chunk_offset: u64) -> Option<&ChunkBitmaps> {
    self.chunks.get(&chunk_offset)
  }

  /// Insert pre-built bitmaps for a chunk. The caller is responsible for
  /// providing the correct `entry_carry` for the chunk's starting position.
  pub fn insert(&mut self, chunk_offset: u64, bitmaps: ChunkBitmaps) -> &mut ChunkBitmaps {
    let added = bitmaps.bytes();
    let entry = self
      .chunks
      .entry(chunk_offset)
      .insert_entry(bitmaps)
      .into_mut();
    self.bytes_counter.fetch_add(added, Ordering::Relaxed);
    entry
  }

  /// Build the structural bitmap for `kind` on `chunk_offset` if it isn't
  /// resident yet, updating byte accounting. No-op if the chunk isn't in
  /// the store (caller is responsible for `insert`-ing first).
  pub fn ensure_structural(&mut self, chunk_offset: u64, chunk: &[u8], kind: Structural) {
    let Some(bm) = self.chunks.get_mut(&chunk_offset) else {
      return;
    };
    if bm.structural(kind).is_some() {
      return;
    }
    bm.ensure_structural(chunk, kind);
    let added = bm.structural(kind).map(|b| b.len() * 8).unwrap_or(0);
    self.bytes_counter.fetch_add(added, Ordering::Relaxed);
  }

  /// Drop cached bitmaps for a chunk.
  pub fn evict(&mut self, chunk_offset: u64) {
    if let Some(bm) = self.chunks.remove(&chunk_offset) {
      self.bytes_counter.fetch_sub(bm.bytes(), Ordering::Relaxed);
    }
  }

  #[cfg(test)]
  fn len(&self) -> usize {
    self.chunks.len()
  }

  #[cfg(test)]
  fn is_empty(&self) -> bool {
    self.chunks.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn bit_positions(words: &[u64]) -> Vec<usize> {
    let mut positions = Vec::new();
    for (w_idx, &word) in words.iter().enumerate() {
      let mut bits = word;
      while bits != 0 {
        let lsb = bits.trailing_zeros() as usize;
        positions.push(w_idx * WINDOW + lsb);
        bits &= bits - 1;
      }
    }
    positions
  }

  #[test]
  fn build_basic_over_two_windows() {
    let mut chunk = vec![b' '; 128];
    // window 0: a string `"hello"` at offset 0..7
    chunk[0..7].copy_from_slice(b"\"hello\"");
    // window 1: an object `{"a":1}` at offset 64..71
    chunk[64..71].copy_from_slice(b"{\"a\":1}");
    let bm = ChunkBitmaps::build_basic(&chunk, ScanCarry::default());

    assert_eq!(bm.n_words, 2);
    // bits 0..=5
    // window 1: only the `a` between the quotes is in_string (bit 2), and
    // the opening quote at bit 1; closing quote at bit 3 is not.
    assert_eq!(bm.in_string[0], 0b0011_1111);
    assert_eq!(bm.in_string[1] & 0xFF, 0b0000_0110);
  }

  #[test]
  fn build_pads_partial_last_window() {
    // 70-byte chunk: occupies window 0 fully and 6 bytes of window 1. The
    // remaining 58 bytes of window 1 are padded with spaces, so the bitmap
    // for window 1 has only bits at positions < 6 possibly set.
    let mut chunk = vec![b' '; 70];
    chunk[64..70].copy_from_slice(b"[1,2,3");
    let mut bm = ChunkBitmaps::build_basic(&chunk, ScanCarry::default());
    let lbracket = bm.ensure_structural(&chunk, Structural::LBracket).to_vec();
    assert_eq!(bit_positions(&lbracket), vec![64]);
    let comma = bm.ensure_structural(&chunk, Structural::Comma).to_vec();
    assert_eq!(bit_positions(&comma), vec![66, 68]);
  }

  #[test]
  fn structural_masks_in_string_chars() {
    // Object with a value that itself contains structural chars in a string:
    // {"k":"a,b{c}"}
    // bytes:   0   1 2 3 4 5 6 7 8 9 10 11 12 13
    //          {   " k " : " a , b {  c  }  "  }
    let mut chunk = vec![b' '; 64];
    chunk[..14].copy_from_slice(b"{\"k\":\"a,b{c}\"}");
    let mut bm = ChunkBitmaps::build_basic(&chunk, ScanCarry::default());

    let colon = bm.ensure_structural(&chunk, Structural::Colon).to_vec();
    let comma = bm.ensure_structural(&chunk, Structural::Comma).to_vec();
    let lbrace = bm.ensure_structural(&chunk, Structural::LBrace).to_vec();
    let rbrace = bm.ensure_structural(&chunk, Structural::RBrace).to_vec();

    // Only the colon at byte 4 counts; nothing inside the string.
    assert_eq!(bit_positions(&colon), vec![4]);
    // The comma inside the string is masked out.
    assert_eq!(bit_positions(&comma), Vec::<usize>::new());
    // Only the outer `{` at byte 0 counts; the one inside the string is masked.
    assert_eq!(bit_positions(&lbrace), vec![0]);
    // Only the outer `}` at byte 13 counts.
    assert_eq!(bit_positions(&rbrace), vec![13]);
  }

  #[test]
  fn structural_caches_on_second_call() {
    let chunk = b"{\"a\":1}".repeat(8); // 56 bytes; fits in one window
    let mut bm = ChunkBitmaps::build_basic(&chunk, ScanCarry::default());
    assert!(!bm.has_structural(Structural::Comma));
    let first = bm.ensure_structural(&chunk, Structural::Comma).to_vec();
    assert!(bm.has_structural(Structural::Comma));
    let second = bm.ensure_structural(&chunk, Structural::Comma).to_vec();
    assert_eq!(first, second);
  }

  #[test]
  fn structural_each_kind_picks_only_its_byte() {
    let mut chunk = vec![b' '; 64];
    chunk[..7].copy_from_slice(b"[1,2,3]");
    let mut bm = ChunkBitmaps::build_basic(&chunk, ScanCarry::default());

    for kind in Structural::ALL {
      let positions = bit_positions(bm.ensure_structural(&chunk, kind));
      let expected: Vec<usize> = (0..7).filter(|&i| chunk[i] == kind.byte()).collect();
      assert_eq!(positions, expected, "kind {kind:?}");
    }
  }

  #[test]
  fn carry_entry_propagates_into_chunk() {
    // Chunk starts mid-string. Provide inside_string carry so the parser
    // knows to treat early bytes as string content.
    let mut chunk = vec![b' '; 64];
    chunk[..6].copy_from_slice(b"end\":1"); // `end":1` - closes the inherited string, then `:1`
    let entry = ScanCarry {
      prev_escaped: 0,
      inside_string: !0,
    };
    let mut bm = ChunkBitmaps::build_basic(&chunk, entry);
    // The string ends at the `"` at byte 3. Inside-string covers bytes 0..=2.
    assert_eq!(bm.in_string[0] & 0xFF, 0b0000_0111);
    // Colon at byte 4 should be picked up; nothing else.
    let colon = bm.ensure_structural(&chunk, Structural::Colon).to_vec();
    assert_eq!(bit_positions(&colon), vec![4]);
  }

  #[test]
  fn store_round_trip() {
    let chunk = b"{\"a\":1}".repeat(4);
    let mut store = BitmapStore::new();
    assert!(store.is_empty());
    let bm = ChunkBitmaps::build_basic(&chunk, ScanCarry::default());
    store.insert(0, bm);
    assert_eq!(store.len(), 1);
    assert!(store.get(0).is_some());
    store.evict(0);
    assert!(store.is_empty());
  }
}
