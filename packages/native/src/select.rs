//! Projection IR for `iter`'s `select`: extract a single sub-value, or a map
//! of named sub-values, from each child before it crosses - so the
//! non-selected parts of the child never materialize. Mirrors the TS
//! `serializeSelect` output (`{"one": [...path]}` or
//! `{"map": [[key, [...path]], ...]}`), where each sub-path is an array of
//! `string | number` segments.

use serde::Deserialize;

use crate::path::Segment;

/// Compiled projection. Externally tagged so the facade's `{"one": [...]}` /
/// `{"map": [...]}` JSON decodes straight into it; each sub-path's segments
/// decode via [`Segment`]'s untagged `Deserialize`. Lowercase to match the
/// JSON the facade emits.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompiledSelect {
  /// A single sub-path - yield the bare sub-value.
  One(Vec<Segment>),
  /// Output-key -> sub-path pairs - yield an object of sub-values in the
  /// declared key order (relies on serde_json `preserve_order`).
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
