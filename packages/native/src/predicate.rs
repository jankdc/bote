//! Predicate IR

use serde::Deserialize;

use crate::pointer::{JsonPointer, PointerParseError};

/// Wire form of a predicate, deserialized from the JSON the TS facade emits.
/// `#[serde(tag = "t")]` keys each node by its constructor name.
#[derive(Debug, Deserialize)]
#[serde(tag = "t")]
enum PredicateIr {
  #[serde(rename = "eq")]
  Eq { p: String, v: serde_json::Value },
  #[serde(rename = "lt")]
  Lt { p: String, v: serde_json::Value },
  #[serde(rename = "lte")]
  Lte { p: String, v: serde_json::Value },
  #[serde(rename = "gt")]
  Gt { p: String, v: serde_json::Value },
  #[serde(rename = "gte")]
  Gte { p: String, v: serde_json::Value },
  #[serde(rename = "exists")]
  Exists { p: String },
  #[serde(rename = "and")]
  And { c: Vec<PredicateIr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareOp {
  Eq,
  Lt,
  Lte,
  Gt,
  Gte,
}

/// A scalar literal to compare a resolved value against.
#[derive(Debug, Clone)]
enum Literal {
  Str(String),
  Num(f64),
  Bool(bool),
  Null,
}

impl Literal {
  fn from_json(v: serde_json::Value) -> Self {
    match v {
      serde_json::Value::String(s) => Literal::Str(s),
      // `as_f64` is lossy past 2^53, matching JS `number` semantics (the
      // literal is already a JS number on the other side of the boundary).
      serde_json::Value::Number(n) => Literal::Num(n.as_f64().unwrap_or(f64::NAN)),
      serde_json::Value::Bool(b) => Literal::Bool(b),
      // Null - and (unreachable; TS forbids them) array/object literals -
      // compare equal only to a literal `null`.
      _ => Literal::Null,
    }
  }
}

/// One flattened predicate leaf: a sub-pointer plus how to test the value it
/// resolves to.
#[derive(Debug, Clone)]
pub struct Leaf {
  pointer: JsonPointer,
  test: LeafTest,
}

#[derive(Debug, Clone)]
enum LeafTest {
  /// Pointer must resolve to *some* value (the value itself is never read).
  Exists,
  /// Pointer must resolve and the value must satisfy `op` against `literal`.
  Compare { op: CompareOp, literal: Literal },
}

impl Leaf {
  /// The sub-pointer this leaf resolves, relative to each candidate child.
  pub fn pointer(&self) -> &JsonPointer {
    &self.pointer
  }

  /// Whether evaluating this leaf needs the resolved value's bytes. `false`
  /// for `exists` (presence is enough), so the caller can skip the read.
  pub fn needs_value(&self) -> bool {
    matches!(self.test, LeafTest::Compare { .. })
  }

  /// Test a `Compare` leaf against the resolved value's exact `[start, end)`
  /// bytes (including the surrounding quotes for strings). Only meaningful
  /// when [`Leaf::needs_value`] is true; returns `true` for `exists`.
  pub fn satisfied_by(&self, value_raw: &[u8]) -> bool {
    match &self.test {
      LeafTest::Exists => true,
      LeafTest::Compare { op, literal } => compare(*op, literal, value_raw),
    }
  }
}

/// A compiled predicate: an AND of leaves (this revision has no `or`/`not`).
#[derive(Debug, Clone)]
pub struct CompiledPredicate {
  leaves: Vec<Leaf>,
}

#[derive(Debug, thiserror::Error)]
pub enum PredicateError {
  #[error("invalid predicate JSON: {0}")]
  Json(String),
  #[error("invalid predicate pointer: {0}")]
  Pointer(#[from] PointerParseError),
}

impl CompiledPredicate {
  /// Parse and compile the JSON IR the TS facade emits. Pointer parse errors
  /// surface here - on the first `next()` of a `scan`/`walk`, or as the
  /// `count` promise rejection - keeping the iterator constructors infallible.
  pub fn parse(json: &str) -> Result<Self, PredicateError> {
    let ir: PredicateIr =
      serde_json::from_str(json).map_err(|e| PredicateError::Json(e.to_string()))?;
    let mut leaves = Vec::new();
    flatten(ir, &mut leaves)?;
    Ok(Self { leaves })
  }

  pub fn leaves(&self) -> &[Leaf] {
    &self.leaves
  }
}

/// Flatten the IR tree into a leaf list. `and` nodes recurse and append; an
/// empty `and` flattens to zero leaves (a vacuously-true predicate).
fn flatten(ir: PredicateIr, out: &mut Vec<Leaf>) -> Result<(), PredicateError> {
  let (pointer, test) = match ir {
    PredicateIr::And { c } => {
      for child in c {
        flatten(child, out)?;
      }
      return Ok(());
    }
    PredicateIr::Exists { p } => (p, LeafTest::Exists),
    PredicateIr::Eq { p, v } => (p, cmp_test(CompareOp::Eq, v)),
    PredicateIr::Lt { p, v } => (p, cmp_test(CompareOp::Lt, v)),
    PredicateIr::Lte { p, v } => (p, cmp_test(CompareOp::Lte, v)),
    PredicateIr::Gt { p, v } => (p, cmp_test(CompareOp::Gt, v)),
    PredicateIr::Gte { p, v } => (p, cmp_test(CompareOp::Gte, v)),
  };
  out.push(Leaf {
    pointer: JsonPointer::parse(&pointer)?,
    test,
  });
  Ok(())
}

fn cmp_test(op: CompareOp, v: serde_json::Value) -> LeafTest {
  LeafTest::Compare {
    op,
    literal: Literal::from_json(v),
  }
}

/// Compare a resolved value's raw bytes against a literal. Total and
/// non-throwing: any type mismatch is `false`.
fn compare(op: CompareOp, literal: &Literal, value_raw: &[u8]) -> bool {
  match literal {
    Literal::Num(lit) => match parse_number(value_raw) {
      Some(val) => match op {
        CompareOp::Eq => val == *lit,
        CompareOp::Lt => val < *lit,
        CompareOp::Lte => val <= *lit,
        CompareOp::Gt => val > *lit,
        CompareOp::Gte => val >= *lit,
      },
      None => false,
    },
    Literal::Str(lit) => match op {
      CompareOp::Eq => string_eq(value_raw, lit),
      _ => match decode_string(value_raw) {
        Some(val) => {
          let ord = val.as_str().cmp(lit.as_str());
          match op {
            CompareOp::Lt => ord.is_lt(),
            CompareOp::Lte => ord.is_le(),
            CompareOp::Gt => ord.is_gt(),
            CompareOp::Gte => ord.is_ge(),
            CompareOp::Eq => unreachable!("eq handled above"),
          }
        }
        None => false,
      },
    },
    // Ordering against bool/null is a type error (TS restricts lt/gt to
    // `number | string`); only `eq` is meaningful here.
    Literal::Bool(lit) => matches!(op, CompareOp::Eq) && value_raw == bool_bytes(*lit),
    Literal::Null => matches!(op, CompareOp::Eq) && value_raw == b"null",
  }
}

fn bool_bytes(b: bool) -> &'static [u8] {
  if b {
    b"true"
  } else {
    b"false"
  }
}

/// Parse a JSON number from the value's exact bytes, or `None` if the value
/// isn't a number (string/bool/null/container all fail `from_slice::<f64>`).
fn parse_number(value_raw: &[u8]) -> Option<f64> {
  serde_json::from_slice::<f64>(value_raw).ok()
}

/// Byte-compare a JSON string value to a literal, decoding only when escapes
/// are present (mirrors `resolve::step_object`'s key compare). `value_raw`
/// includes the surrounding quotes; any non-string value is never equal.
fn string_eq(value_raw: &[u8], lit: &str) -> bool {
  if value_raw.len() < 2 || value_raw[0] != b'"' || value_raw[value_raw.len() - 1] != b'"' {
    return false;
  }
  let interior = &value_raw[1..value_raw.len() - 1];
  if !interior.contains(&b'\\') {
    interior == lit.as_bytes()
  } else {
    decode_string(value_raw).map(|s| s == lit).unwrap_or(false)
  }
}

/// Decode a JSON string value (handling escapes) to an owned `String`, or
/// `None` if the value isn't a JSON string.
fn decode_string(value_raw: &[u8]) -> Option<String> {
  if value_raw.first() != Some(&b'"') {
    return None;
  }
  serde_json::from_slice::<String>(value_raw).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn compiled(json: &str) -> CompiledPredicate {
    CompiledPredicate::parse(json).unwrap()
  }

  /// Test the comparison of a single-leaf predicate directly against a value's
  /// raw bytes (pointer resolution is exercised in the session/core tests).
  fn holds(json: &str, value_raw: &[u8]) -> bool {
    compiled(json).leaves()[0].satisfied_by(value_raw)
  }

  #[test]
  fn eq_number_matches_value_bytes() {
    assert!(holds(r#"{"t":"eq","p":"/n","v":100}"#, b"100"));
    assert!(!holds(r#"{"t":"eq","p":"/n","v":100}"#, b"101"));
    // type-aware: number 1 != string "1"
    assert!(!holds(r#"{"t":"eq","p":"/n","v":1}"#, b"\"1\""));
  }

  #[test]
  fn eq_string_matches_and_is_type_aware() {
    assert!(holds(r#"{"t":"eq","p":"/s","v":"paid"}"#, b"\"paid\""));
    assert!(!holds(r#"{"t":"eq","p":"/s","v":"paid"}"#, b"\"refunded\""));
    // string literal never equals a number value
    assert!(!holds(r#"{"t":"eq","p":"/s","v":"1"}"#, b"1"));
  }

  #[test]
  fn eq_string_decodes_escapes() {
    // literal `a"b` vs JSON value "a\"b"
    assert!(holds(r#"{"t":"eq","p":"/s","v":"a\"b"}"#, b"\"a\\\"b\""));
  }

  #[test]
  fn ordering_on_numbers() {
    assert!(holds(r#"{"t":"gte","p":"/n","v":100}"#, b"100"));
    assert!(holds(r#"{"t":"gte","p":"/n","v":100}"#, b"250"));
    assert!(!holds(r#"{"t":"gte","p":"/n","v":100}"#, b"99"));
    assert!(holds(r#"{"t":"lt","p":"/n","v":100}"#, b"99"));
    // ordering on a string value is a type mismatch -> false
    assert!(!holds(r#"{"t":"gte","p":"/n","v":100}"#, b"\"x\""));
  }

  #[test]
  fn ordering_on_strings() {
    assert!(holds(r#"{"t":"gt","p":"/s","v":"m"}"#, b"\"z\""));
    assert!(!holds(r#"{"t":"gt","p":"/s","v":"m"}"#, b"\"a\""));
  }

  #[test]
  fn bool_and_null_eq() {
    assert!(holds(r#"{"t":"eq","p":"/b","v":true}"#, b"true"));
    assert!(!holds(r#"{"t":"eq","p":"/b","v":true}"#, b"false"));
    assert!(holds(r#"{"t":"eq","p":"/x","v":null}"#, b"null"));
    assert!(!holds(r#"{"t":"eq","p":"/x","v":null}"#, b"0"));
  }

  #[test]
  fn exists_needs_no_value() {
    assert!(!compiled(r#"{"t":"exists","p":"/a"}"#).leaves()[0].needs_value());
  }

  #[test]
  fn and_flattens_nested() {
    let p = compiled(
      r#"{"t":"and","c":[{"t":"eq","p":"/a","v":1},{"t":"and","c":[{"t":"gt","p":"/b","v":2},{"t":"exists","p":"/c"}]}]}"#,
    );
    assert_eq!(p.leaves().len(), 3);
  }

  #[test]
  fn empty_and_is_zero_leaves() {
    assert_eq!(compiled(r#"{"t":"and","c":[]}"#).leaves().len(), 0);
  }

  #[test]
  fn invalid_pointer_errors() {
    assert!(CompiledPredicate::parse(r#"{"t":"eq","p":"bad","v":1}"#).is_err());
  }

  #[test]
  fn invalid_json_errors() {
    assert!(CompiledPredicate::parse("not json").is_err());
  }
}
