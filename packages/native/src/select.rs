//! Projection IR for `iter`'s `select`: extract a single sub-value, or a map
//! of named sub-values, from each child before it crosses - so the
//! non-selected parts of the child never materialize. Mirrors the TS
//! `serializeSelect` output (`{"one": [...path]}` or
//! `{"map": [[key, [...path]], ...]}`), where each sub-path is an array of
//! `string | number` segments.

use serde::Deserialize;

use crate::path::Segment;

/// Wire form, externally tagged so `{"one": [...]}` / `{"map": [...]}` decode
/// directly. Lowercase to match the JSON the facade emits.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SelectIr {
  One(Vec<WireSegment>),
  Map(Vec<(String, Vec<WireSegment>)>),
}

/// Wire segment: untagged so a JSON `string` decodes to `Member` and a
/// JSON `number` decodes to `Element`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireSegment {
  Member(String),
  Element(u64),
}

impl From<WireSegment> for Segment {
  fn from(w: WireSegment) -> Self {
    match w {
      WireSegment::Member(s) => Segment::Member(s),
      WireSegment::Element(n) => Segment::Element(n as usize),
    }
  }
}

/// Compiled projection: sub-paths typed once.
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
    let ir: SelectIr = serde_json::from_str(json).map_err(|e| SelectError::Json(e.to_string()))?;
    Ok(match ir {
      SelectIr::One(p) => CompiledSelect::One(p.into_iter().map(Into::into).collect()),
      SelectIr::Map(fields) => CompiledSelect::Map(
        fields
          .into_iter()
          .map(|(k, p)| (k, p.into_iter().map(Into::into).collect()))
          .collect(),
      ),
    })
  }
}
