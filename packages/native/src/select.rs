//! Projection IR for `scan`'s `select`: extract a single sub-value, or a map
//! of named sub-values, from each child before it crosses - so the
//! non-selected parts of the child never materialize. Mirrors the TS
//! `serializeSelect` output (`{"one": ptr}` or `{"map": [[key, ptr], ...]}`).

use serde::Deserialize;

use crate::pointer::{JsonPointer, PointerParseError};

/// Wire form, externally tagged so `{"one": "/x"}` / `{"map": [...]}` decode
/// directly. Lowercase to match the JSON the facade emits.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SelectIr {
  One(String),
  Map(Vec<(String, String)>),
}

/// Compiled projection: sub-pointers parsed once.
pub enum CompiledSelect {
  /// A single sub-pointer - yield the bare sub-value.
  One(JsonPointer),
  /// Output-key -> sub-pointer pairs - yield an object of sub-values in the
  /// declared key order (relies on serde_json `preserve_order`).
  Map(Vec<(String, JsonPointer)>),
}

#[derive(Debug, thiserror::Error)]
pub enum SelectError {
  #[error("invalid select JSON: {0}")]
  Json(String),
  #[error("invalid select pointer: {0}")]
  Pointer(#[from] PointerParseError),
}

impl CompiledSelect {
  pub fn parse(json: &str) -> Result<Self, SelectError> {
    let ir: SelectIr = serde_json::from_str(json).map_err(|e| SelectError::Json(e.to_string()))?;
    Ok(match ir {
      SelectIr::One(p) => CompiledSelect::One(JsonPointer::parse(&p)?),
      SelectIr::Map(fields) => {
        let mut out = Vec::with_capacity(fields.len());
        for (key, p) in fields {
          out.push((key, JsonPointer::parse(&p)?));
        }
        CompiledSelect::Map(out)
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_one() {
    assert!(matches!(
      CompiledSelect::parse(r#"{"one":"/total"}"#).unwrap(),
      CompiledSelect::One(_)
    ));
  }

  #[test]
  fn parse_map_preserves_declared_order() {
    let s =
      CompiledSelect::parse(r#"{"map":[["total","/total"],["country","/c/country"]]}"#).unwrap();
    match s {
      CompiledSelect::Map(fields) => {
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["total", "country"]);
      }
      _ => panic!("expected map"),
    }
  }

  #[test]
  fn invalid_pointer_errors() {
    assert!(CompiledSelect::parse(r#"{"one":"bad"}"#).is_err());
  }
}
