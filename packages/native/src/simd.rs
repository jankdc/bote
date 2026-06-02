//! Per-block SIMD core: derive structural bitmaps from a 64-byte window.

use std::simd::cmp::SimdPartialEq;
use std::simd::Simd;

/// Width of one scan window, in bytes. One window produces 64 bitmap bits.
pub const BLOCK_BYTES: usize = 64;

/// Carry state propagated between consecutive 64-byte blocks.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanCarry {
  /// `1` iff the first byte of the next block is escaped by an odd-length
  /// backslash run that ended on the boundary; else `0`.
  pub prev_escaped: u64,
  /// All-ones if the next block begins inside a string literal; else 0.
  /// Stored as a full word so it can be XORed into the next block's
  /// parity-prefix in one op.
  pub inside_string: u64,
}

/// Scan one 64-byte window. Returns the `in_string` mask - bits set for
/// bytes inside a string literal, including the opening `"` but excluding
/// the closing `"`. Use `!in_string` to mask string contents out of
/// structural-character bitmaps.
pub fn scan_block(block: &[u8; BLOCK_BYTES], carry: ScanCarry) -> (u64, ScanCarry) {
  let v: Simd<u8, BLOCK_BYTES> = Simd::from_array(*block);
  let quote = v.simd_eq(Simd::splat(b'"')).to_bitmask();
  let backslash = v.simd_eq(Simd::splat(b'\\')).to_bitmask();

  let (escaped, prev_escaped) = find_escaped(backslash, carry.prev_escaped);
  let real_quote = quote & !escaped;
  let in_string = parity_prefix(real_quote) ^ carry.inside_string;

  let new_carry = ScanCarry {
    prev_escaped,
    inside_string: ((in_string as i64) >> 63) as u64,
  };
  (in_string, new_carry)
}

/// Ported from simdjson's `find_escaped`. Returns `(escaped, prev_escaped_out)`:
/// `escaped` is a bitmap of byte positions that are escaped by an odd-length
/// backslash run, and `prev_escaped_out` is the carry-out bit (0 or 1) for
/// the next block.
fn find_escaped(mut backslash: u64, prev_escaped: u64) -> (u64, u64) {
  const EVEN_BITS: u64 = 0x5555_5555_5555_5555;

  backslash &= !prev_escaped;
  let follows_escape = (backslash << 1) | prev_escaped;
  let odd_sequence_starts = backslash & !EVEN_BITS & !follows_escape;
  let (sequences_starting_on_even_bits, overflow) = odd_sequence_starts.overflowing_add(backslash);
  let invert_mask = sequences_starting_on_even_bits << 1;

  let escaped = (EVEN_BITS ^ invert_mask) & follows_escape;
  (escaped, u64::from(overflow))
}

/// SWAR parity-prefix scan: for each bit position `i`, output bit `i` equals
/// the XOR of input bits `0..=i`. Six shift-XOR pairs over a 64-bit word.
fn parity_prefix(mut x: u64) -> u64 {
  x ^= x << 1;
  x ^= x << 2;
  x ^= x << 4;
  x ^= x << 8;
  x ^= x << 16;
  x ^= x << 32;
  x
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Scalar reference implementation that mirrors the SIMD algorithm exactly.
  fn scalar_reference(bytes: &[u8; BLOCK_BYTES], carry: ScanCarry) -> (u64, ScanCarry) {
    let mut quote = 0u64;
    let mut escaped = 0u64;

    let mut run_length: u32 = u32::from(carry.prev_escaped != 0);
    for (i, &b) in bytes.iter().enumerate() {
      let bit = 1u64 << i;
      if b == b'"' {
        quote |= bit;
      }
      if run_length % 2 == 1 {
        escaped |= bit;
      }
      if b == b'\\' {
        run_length += 1;
      } else {
        run_length = 0;
      }
    }

    let real_quote = quote & !escaped;
    let in_string = parity_prefix(real_quote) ^ carry.inside_string;

    // Next block's bit 0 is escaped iff the run ending at bit 63 has odd
    // length (and therefore bit 63 was a `\`).
    let prev_escaped_out =
      u64::from(bytes[BLOCK_BYTES - 1] == b'\\' && !run_length.is_multiple_of(2));

    let new_carry = ScanCarry {
      prev_escaped: prev_escaped_out,
      inside_string: ((in_string as i64) >> 63) as u64,
    };

    (in_string, new_carry)
  }

  fn pad_to_window(s: &[u8]) -> [u8; BLOCK_BYTES] {
    let mut out = [b' '; BLOCK_BYTES];
    out[..s.len()].copy_from_slice(s);
    out
  }

  fn assert_matches_reference(bytes: &[u8; BLOCK_BYTES], carry: ScanCarry) {
    let (in_string, out_carry) = scan_block(bytes, carry);
    let (ref_in_string, ref_carry) = scalar_reference(bytes, carry);
    assert_eq!(in_string, ref_in_string, "in_string bitmap");
    assert_eq!(out_carry, ref_carry, "carry out");
  }

  /// Advance the LCG `state` and fill one 64-byte block from `alphabet`.
  /// Shared by the fuzz tests so the generator constants live in one place.
  fn fill_block(state: &mut u64, alphabet: &[u8]) -> [u8; BLOCK_BYTES] {
    let mut block = [0u8; BLOCK_BYTES];
    for slot in &mut block {
      *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
      *slot = alphabet[((*state >> 33) as usize) % alphabet.len()];
    }
    block
  }

  #[test]
  fn scan_empty_block_has_zero_bitmaps() {
    let block = [b' '; BLOCK_BYTES];
    let (in_string, c) = scan_block(&block, ScanCarry::default());
    assert_eq!(in_string, 0);
    assert_eq!(c, ScanCarry::default());
  }

  #[test]
  fn string_simple_marks_in_string() {
    // `"hello"` at the start of the window
    let block = pad_to_window(b"\"hello\"");
    assert_matches_reference(&block, ScanCarry::default());
    let (in_string, _) = scan_block(&block, ScanCarry::default());
    // bits 0..=5 in_string (opening quote + "hello"), bit 6 closing quote (not in_string)
    assert_eq!(in_string, 0b0011_1111u64);
  }

  #[test]
  fn string_escaped_quote_does_not_close() {
    // `"a\"b"` - content is a"b, the middle quote is escaped
    let block = pad_to_window(b"\"a\\\"b\"");
    assert_matches_reference(&block, ScanCarry::default());
    let (in_string, _) = scan_block(&block, ScanCarry::default());
    // bytes:        0=" 1=a 2=\ 3=" 4=b 5="
    // in_string bits 0..=4 (opening quote + content); bit 5 is the closing
    // quote and is not in_string.
    assert_eq!(in_string, 0b0001_1111u64);
  }

  #[test]
  fn escape_double_backslash_is_not_an_escape() {
    // `"\\"` - content is one backslash; the second backslash is escaped by the first
    let block = pad_to_window(b"\"\\\\\"");
    assert_matches_reference(&block, ScanCarry::default());
    let (in_string, _) = scan_block(&block, ScanCarry::default());
    // bytes: 0=" 1=\ 2=\ 3=" - in_string covers the open quote + two `\`.
    assert_eq!(in_string, 0b0000_0111u64);
  }

  #[test]
  fn escape_triple_backslash_quote() {
    // `"\\\""` - backslash escapes backslash; then escaped quote
    // bytes: 0=" 1=\ 2=\ 3=\ 4=" 5="
    // bit 2 escaped by bit 1; bit 4 escaped by bit 3; closing quote at bit 5
    let block = pad_to_window(b"\"\\\\\\\"\"");
    assert_matches_reference(&block, ScanCarry::default());
  }

  #[test]
  fn escape_position_based_ignores_string_context() {
    // Algorithmically, escape detection is context-free: any byte after an
    // odd-length `\` run is "escaped". For well-formed JSON this only matters
    // inside string literals, but the SIMD bitmap covers all positions -
    // and the scalar reference matches that convention exactly.
    let block = pad_to_window(b"\\\"abc\"");
    assert_matches_reference(&block, ScanCarry::default());
  }

  #[test]
  fn string_quote_outside_opens_string() {
    let block = pad_to_window(b"hello \"world\" rest");
    assert_matches_reference(&block, ScanCarry::default());
  }

  #[test]
  fn carry_inside_string_propagates() {
    // Block starts inside a string; ends still inside. in_string should be
    // all-1s up to the closing quote.
    let block = pad_to_window(b"continues here\" then");
    let carry = ScanCarry {
      prev_escaped: 0,
      inside_string: !0,
    };
    let (in_string, out_carry) = scan_block(&block, carry);
    // Bits 0..=13 inside string ('continues here' = 14 chars); bit 14 is closing quote (not in_string)
    assert_eq!(in_string & 0xFFFF, 0x3FFFu64);
    // The block ends outside string, so carry-out should be 0.
    assert_eq!(out_carry.inside_string, 0);
  }

  #[test]
  fn carry_out_set_when_ends_inside_string() {
    // String starts in this block and is not closed by end-of-block.
    let mut block = [b'x'; BLOCK_BYTES];
    block[0] = b'"'; // opens a string never closed in this block
    let (in_string, out_carry) = scan_block(&block, ScanCarry::default());
    assert_eq!(in_string, !0u64);
    assert_eq!(out_carry.inside_string, !0u64);
  }

  #[test]
  fn carry_escape_across_blocks() {
    // Block 1 ends with a single `\`; block 2 starts with `"`. The quote
    // in block 2 must be marked escaped via the carry.
    let mut b1 = [b' '; BLOCK_BYTES];
    b1[0] = b'"'; // open a string
    b1[63] = b'\\'; // trailing backslash, no escape carry-in
    let (_, carry1) = scan_block(&b1, ScanCarry::default());
    // Inside string at bit 63, trailing backslash → next block's first byte is escaped
    assert_eq!(carry1.prev_escaped, 1);
    assert_eq!(carry1.inside_string, !0);

    let mut b2 = [b' '; BLOCK_BYTES];
    b2[0] = b'"'; // would close the string, but it's escaped
    b2[5] = b'"'; // this one closes
    let (in_string2, _carry2) = scan_block(&b2, carry1);
    // bit 0 is escaped → still in string; bits 0..=4 in_string, bit 5 closes.
    assert_eq!(in_string2 & 0x3F, 0b0001_1111u64);
  }

  #[test]
  fn parity_prefix_basic() {
    assert_eq!(parity_prefix(0), 0);
    assert_eq!(parity_prefix(1), !0); // bit 0 propagates to all higher bits
                                      // bits 0,2 set → parity: bit 0=1, bit 1=1, bit 2=0
    assert_eq!(parity_prefix(0b101), 0b011);
    assert_eq!(parity_prefix(1 << 63), 1 << 63);
  }

  #[test]
  fn fuzz_matches_reference_on_random_inputs() {
    // Deterministic LCG-driven fuzz: 256 random 64-byte blocks of a small
    // alphabet biased toward structural chars and backslashes.
    let alphabet: &[u8] = b"abc{}[],:\"\\ \n";
    let mut state: u64 = 0xdead_beef_cafe_babe;
    for _ in 0..256 {
      let block = fill_block(&mut state, alphabet);
      assert_matches_reference(&block, ScanCarry::default());
    }
  }

  #[test]
  fn fuzz_streaming_matches_reference_over_concatenation() {
    // Generate N blocks, run the SIMD scan with chained carries, and verify
    // every block's bitmaps + carry match the scalar reference run with the
    // same chained carries. This is the cross-block correctness gate.
    let alphabet: &[u8] = b"abc{}[],:\"\\";
    let mut state: u64 = 0xfeed_face_dead_c0de;
    let mut simd_carry = ScanCarry::default();
    let mut ref_carry = ScanCarry::default();
    for block_idx in 0..32 {
      let block = fill_block(&mut state, alphabet);
      let (simd_in_string, simd_next) = scan_block(&block, simd_carry);
      let (ref_in_string, ref_next) = scalar_reference(&block, ref_carry);
      assert_eq!(
        simd_in_string, ref_in_string,
        "in_string differs at block {block_idx}"
      );
      assert_eq!(simd_next, ref_next, "carry differs at block {block_idx}");
      simd_carry = simd_next;
      ref_carry = ref_next;
    }
  }

  #[test]
  fn fuzz_matches_reference_with_inside_carry() {
    let alphabet: &[u8] = b"abc{}[],:\"\\";
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let carry = ScanCarry {
      prev_escaped: 0,
      inside_string: !0,
    };
    for _ in 0..256 {
      let block = fill_block(&mut state, alphabet);
      assert_matches_reference(&block, carry);
    }
  }
}
