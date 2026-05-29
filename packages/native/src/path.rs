use napi::bindgen_prelude::Either;

#[derive(Debug, Clone, PartialEq, Eq)]
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
