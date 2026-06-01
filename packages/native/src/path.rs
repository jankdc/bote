use napi::bindgen_prelude::Either;
use serde::Deserialize;

/// A single path step. `Deserialize` is `untagged` so a JSON `string` decodes
/// to `Member` and a JSON `number` to `Element` - the wire form `select` IR
/// uses (see [`crate::select`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(untagged)]
pub enum Segment {
  Member(String),
  Element(usize),
}

impl From<Either<String, u32>> for Segment {
  fn from(raw: Either<String, u32>) -> Self {
    match raw {
      Either::A(s) => Segment::Member(s),
      Either::B(n) => Segment::Element(n as usize),
    }
  }
}

pub fn from_napi(raw: Vec<Either<String, u32>>) -> Vec<Segment> {
  raw.into_iter().map(Segment::from).collect()
}
