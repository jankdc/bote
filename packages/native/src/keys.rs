//! Matching and decoding JSON object keys - the raw bytes of a member name's
//! string interior, between the quotes, escapes and all. Callers pass that
//! interior span directly; the surrounding quotes are structural and play no
//! part in the comparison.

/// Does the raw interior of a JSON string equal `target`? Escape-free interiors
/// compare byte-for-byte; an escaped one is decoded first. `Err(())` on a
/// malformed escape or invalid UTF-8 - the caller maps it to its own error.
pub fn compare(interior: &[u8], target: &str) -> Result<bool, ()> {
  // A JSON escape only ever shortens the decoded string, so an interior shorter
  // than the target can never match; an escape-free one must match byte length.
  if interior.len() < target.len() {
    return Ok(false);
  }
  if is_escaped(interior) {
    Ok(decode_escaped(interior)? == target)
  } else {
    Ok(interior == target.as_bytes())
  }
}

pub fn decode_simple(interior: &[u8]) -> Result<String, ()> {
  std::str::from_utf8(interior)
    .map(str::to_owned)
    .map_err(|_| ())
}

pub fn decode_escaped(interior: &[u8]) -> Result<String, ()> {
  // serde_json unescapes a *quoted* token, so wrap the interior back in quotes.
  // can forgive this memory allocation since it's quite rare to have escaped keys
  let mut quoted = Vec::with_capacity(interior.len() + 2);
  quoted.push(b'"');
  quoted.extend_from_slice(interior);
  quoted.push(b'"');
  serde_json::from_slice(&quoted).map_err(|_| ())
}

pub fn is_escaped(interior: &[u8]) -> bool {
  interior.contains(&b'\\')
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn compare_escape_free_matches_by_bytes_and_length() {
    assert_eq!(compare(b"name", "name"), Ok(true));
    assert_eq!(compare(b"name", "nope"), Ok(false)); // same length, different bytes
    assert_eq!(compare(b"name", "nam"), Ok(false)); // longer interior
    assert_eq!(compare(b"nam", "name"), Ok(false)); // shorter interior
    assert_eq!(compare(b"", ""), Ok(true));
    assert_eq!(compare(b"x", ""), Ok(false));
  }

  #[test]
  fn compare_decodes_escaped_interiors() {
    // Interior `a\"b` decodes to `a"b`.
    assert_eq!(compare(br#"a\"b"#, "a\"b"), Ok(true));
    assert_eq!(compare(br#"a\"b"#, "axb"), Ok(false)); // decodes unequal
    assert_eq!(compare(br#"a\"bcd"#, "a\"b"), Ok(false)); // decodes longer
  }

  #[test]
  fn compare_rejects_malformed_escape() {
    assert!(compare(br#"\x"#, "x").is_err());
  }

  #[test]
  fn is_escaped_detects_a_backslash() {
    assert!(!is_escaped(b"name"));
    assert!(!is_escaped(b""));
    assert!(is_escaped(br#"a\"b"#));
  }

  #[test]
  fn decode_simple_owns_escape_free_bytes_and_validates_utf8() {
    assert_eq!(decode_simple(b"name"), Ok("name".to_string()));
    assert_eq!(decode_simple(b""), Ok(String::new()));
    assert!(decode_simple(&[b'a', 0xFF]).is_err()); // invalid UTF-8
  }

  #[test]
  fn decode_escaped_resolves_escapes() {
    assert_eq!(decode_escaped(br#"a\"b"#), Ok("a\"b".to_string())); // `a\"b` -> `a"b`
    assert!(decode_escaped(br#"\x"#).is_err()); // malformed escape
  }
}
