//! Per-block structural bitmap construction.
//!
//! Bitmaps are built one 64-byte block at a time, on the fly, by the walker -
//! never stored. [`structural_word`] turns a block into the bitmask of a single
//! structural byte's positions outside string literals, masking with the
//! `in_string` mask produced by [`crate::simd::scan_block`].

use std::simd::cmp::SimdPartialEq;
use std::simd::Simd;

use crate::simd::BLOCK_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Structural {
  Comma,
  LBrace,
  RBrace,
  LBracket,
  RBracket,
}

impl Structural {
  pub fn byte(self) -> u8 {
    match self {
      Self::Comma => b',',
      Self::LBrace => b'{',
      Self::RBrace => b'}',
      Self::LBracket => b'[',
      Self::RBracket => b']',
    }
  }

  #[cfg(test)]
  const ALL: [Structural; 5] = [
    Structural::Comma,
    Structural::LBrace,
    Structural::RBrace,
    Structural::LBracket,
    Structural::RBracket,
  ];
}

/// Structural bitmap for a single 64-byte block: bits set where `kind`'s byte
/// occurs outside a string literal. Masks out positions covered by `in_string`
/// (the closing-quote-exclusive string mask from [`crate::simd::scan_block`]).
pub fn structural_word(block: &[u8; BLOCK_BYTES], in_string: u64, kind: Structural) -> u64 {
  let v: Simd<u8, BLOCK_BYTES> = Simd::from_array(*block);
  let raw = v.simd_eq(Simd::splat(kind.byte())).to_bitmask();
  raw & !in_string
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::simd::{scan_block, ScanCarry};

  fn bit_positions(word: u64) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut bits = word;
    while bits != 0 {
      positions.push(bits.trailing_zeros() as usize);
      bits &= bits - 1;
    }
    positions
  }

  fn block(s: &[u8]) -> [u8; BLOCK_BYTES] {
    let mut b = [b' '; BLOCK_BYTES];
    b[..s.len()].copy_from_slice(s);
    b
  }

  #[test]
  fn masks_in_string_and_picks_byte() {
    // `{"k":"a,b{c}"}` - structural chars inside the string value must be masked.
    let b = block(b"{\"k\":\"a,b{c}\"}");
    let (in_string, _) = scan_block(&b, ScanCarry::default());
    assert_eq!(
      bit_positions(structural_word(&b, in_string, Structural::Comma)),
      Vec::<usize>::new(),
      "interior comma masked",
    );
    assert_eq!(
      bit_positions(structural_word(&b, in_string, Structural::LBrace)),
      vec![0],
      "only the outer {{",
    );
    assert_eq!(
      bit_positions(structural_word(&b, in_string, Structural::RBrace)),
      vec![13],
      "only the outer }}",
    );
  }

  #[test]
  fn each_kind_picks_only_its_byte() {
    let b = block(b"[1,2,3]");
    let (in_string, _) = scan_block(&b, ScanCarry::default());
    for kind in Structural::ALL {
      let positions = bit_positions(structural_word(&b, in_string, kind));
      let expected: Vec<usize> = (0..7).filter(|&i| b[i] == kind.byte()).collect();
      assert_eq!(positions, expected, "kind {kind:?}");
    }
  }

  #[test]
  fn carry_inside_string_masks_leading_structurals() {
    // Block begins inside a string (carry) that closes at the `"`, then `,1`.
    let b = block(b"end\",1");
    let entry = ScanCarry {
      prev_escaped: 0,
      inside_string: !0,
    };
    let (in_string, _) = scan_block(&b, entry);
    // The comma at byte 4 is outside the string; nothing before it.
    assert_eq!(
      bit_positions(structural_word(&b, in_string, Structural::Comma)),
      vec![4],
    );
  }
}
