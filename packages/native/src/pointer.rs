//! RFC 6901 JSON Pointer parser.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPointer {
  tokens: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PointerParseError {
  #[error("JSON pointer must be empty or start with '/'")]
  MissingLeadingSlash,
  #[error("invalid escape '~{escape}' (only ~0 and ~1 are valid)")]
  InvalidEscape { escape: char },
  #[error("dangling '~' at end of reference token")]
  DanglingTilde,
}

impl JsonPointer {
  pub fn parse(input: &str) -> Result<Self, PointerParseError> {
    if input.is_empty() {
      return Ok(Self { tokens: Vec::new() });
    }
    if !input.starts_with('/') {
      return Err(PointerParseError::MissingLeadingSlash);
    }
    let mut tokens = Vec::new();
    for raw in input[1..].split('/') {
      tokens.push(unescape_token(raw)?);
    }
    Ok(Self { tokens })
  }

  pub fn tokens(&self) -> &[String] {
    &self.tokens
  }
}

pub fn token_as_array_index(token: &str) -> Option<usize> {
  if token.is_empty() {
    return None;
  }
  let bytes = token.as_bytes();
  if bytes[0] == b'0' {
    return if bytes.len() == 1 { Some(0) } else { None };
  }
  if !bytes.iter().all(u8::is_ascii_digit) {
    return None;
  }
  token.parse().ok()
}

fn unescape_token(raw: &str) -> Result<String, PointerParseError> {
  if !raw.contains('~') {
    return Ok(raw.to_owned());
  }
  let mut out = String::with_capacity(raw.len());
  let mut chars = raw.chars();
  while let Some(ch) = chars.next() {
    if ch != '~' {
      out.push(ch);
      continue;
    }
    match chars.next() {
      Some('0') => out.push('~'),
      Some('1') => out.push('/'),
      Some(escape) => return Err(PointerParseError::InvalidEscape { escape }),
      None => return Err(PointerParseError::DanglingTilde),
    }
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_root_pointer_has_no_tokens() {
    let p = JsonPointer::parse("").unwrap();
    assert!(p.tokens().is_empty());
  }

  #[test]
  fn parse_rfc6901_section_5_examples() {
    let cases: &[(&str, &[&str])] = &[
      ("/foo", &["foo"]),
      ("/foo/0", &["foo", "0"]),
      ("/", &[""]),
      ("/a~1b", &["a/b"]),
      ("/c%d", &["c%d"]),
      ("/e^f", &["e^f"]),
      ("/g|h", &["g|h"]),
      ("/i\\j", &["i\\j"]),
      ("/k\"l", &["k\"l"]),
      ("/ ", &[" "]),
      ("/m~0n", &["m~n"]),
    ];
    for (input, expected) in cases {
      let parsed = JsonPointer::parse(input).unwrap_or_else(|_| panic!("parse failed: {input}"));
      let actual: Vec<&str> = parsed.tokens().iter().map(String::as_str).collect();
      assert_eq!(actual, *expected, "input: {input}");
    }
  }

  #[test]
  fn parse_rejects_missing_leading_slash() {
    assert_eq!(
      JsonPointer::parse("foo"),
      Err(PointerParseError::MissingLeadingSlash)
    );
    assert_eq!(
      JsonPointer::parse("a/b"),
      Err(PointerParseError::MissingLeadingSlash)
    );
  }

  #[test]
  fn parse_rejects_dangling_tilde() {
    assert_eq!(
      JsonPointer::parse("/~"),
      Err(PointerParseError::DanglingTilde)
    );
    assert_eq!(
      JsonPointer::parse("/foo/~"),
      Err(PointerParseError::DanglingTilde)
    );
  }

  #[test]
  fn parse_rejects_invalid_tilde_escape() {
    assert_eq!(
      JsonPointer::parse("/~2"),
      Err(PointerParseError::InvalidEscape { escape: '2' })
    );
    assert_eq!(
      JsonPointer::parse("/~a"),
      Err(PointerParseError::InvalidEscape { escape: 'a' })
    );
  }

  #[test]
  fn parse_nested_escapes_resolve_left_to_right() {
    // ~01 means "~0" then "1", decoding to "~1"
    let p = JsonPointer::parse("/~01").unwrap();
    assert_eq!(p.tokens(), &["~1"]);
    // ~10 means "~1" then "0", decoding to "/0"
    let p = JsonPointer::parse("/~10").unwrap();
    assert_eq!(p.tokens(), &["/0"]);
  }

  #[test]
  fn parse_unicode_tokens_pass_through() {
    let p = JsonPointer::parse("/日本語/αβγ").unwrap();
    assert_eq!(p.tokens(), &["日本語", "αβγ"]);
  }

  #[test]
  fn parse_empty_intermediate_tokens_preserved() {
    let p = JsonPointer::parse("/a//b").unwrap();
    assert_eq!(p.tokens(), &["a", "", "b"]);
    let p = JsonPointer::parse("//").unwrap();
    assert_eq!(p.tokens(), &["", ""]);
  }

  #[test]
  fn array_index_zero() {
    assert_eq!(token_as_array_index("0"), Some(0));
  }

  #[test]
  fn array_index_positive() {
    assert_eq!(token_as_array_index("1"), Some(1));
    assert_eq!(token_as_array_index("42"), Some(42));
    assert_eq!(token_as_array_index("1234567890"), Some(1234567890));
  }

  #[test]
  fn array_index_rejects_leading_zero() {
    assert_eq!(token_as_array_index("01"), None);
    assert_eq!(token_as_array_index("007"), None);
  }

  #[test]
  fn array_index_rejects_non_digit() {
    assert_eq!(token_as_array_index(""), None);
    assert_eq!(token_as_array_index("-1"), None);
    assert_eq!(token_as_array_index("+1"), None);
    assert_eq!(token_as_array_index("1.0"), None);
    assert_eq!(token_as_array_index("1e3"), None);
    assert_eq!(token_as_array_index("foo"), None);
    assert_eq!(token_as_array_index("-"), None);
  }
}
