//! Projection IR for `iter`'s `select`: extract sub-values from each child
//! before it crosses, so non-selected parts never materialize. Mirrors the TS
//! `serializeSelect` output (`{"one": [...path]}` / `{"map": [[key, [...path]],
//! ...]}`); each sub-path is an array of `string | number` segments.

use serde::Deserialize;

use crate::path::Segment;

/// Externally tagged + lowercase so the facade's `{"one": [...]}` /
/// `{"map": [...]}` JSON decodes straight into it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompiledSelect {
  /// Yield the bare sub-value at this sub-path.
  One(Vec<Segment>),
  /// Yield an object of sub-values in declared key order (relies on
  /// serde_json `preserve_order`).
  Map(Vec<(String, Vec<Segment>)>),
}

#[derive(Debug, thiserror::Error)]
pub enum SelectError {
  #[error("invalid select JSON: {0}")]
  Json(String),
}

impl CompiledSelect {
  pub fn parse(json: &str) -> Result<Self, SelectError> {
    serde_json::from_str(json).map_err(|e| SelectError::Json(e.to_string()))
  }
}
