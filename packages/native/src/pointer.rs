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
