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
/// resolves to. `op == None` is the `exists` test (the value's bytes are
/// never read); `op == Some(_)` is a comparison against `literal`.
#[derive(Debug, Clone)]
pub struct Leaf {
  pointer: JsonPointer,
  op: Option<CompareOp>,
  /// Comparison literal. A sentinel [`Literal::Null`] is stored when `op`
  /// is `None`, keeping the struct uniform; it's ignored in that case.
  literal: Literal,
}

impl Leaf {
  /// The sub-pointer this leaf resolves, relative to each candidate child.
  pub fn pointer(&self) -> &JsonPointer {
    &self.pointer
  }

  /// Whether evaluating this leaf needs the resolved value's bytes. `false`
  /// for `exists` (presence is enough), so the caller can skip the read.
  pub fn needs_value(&self) -> bool {
    self.op.is_some()
  }

  /// Test the resolved value's exact `[start, end)` bytes (including the
  /// surrounding quotes for strings) against this leaf's comparison. Only
  /// meaningful when [`Leaf::needs_value`] is true; returns `true` for
  /// `exists`.
  pub fn satisfied_by(&self, value_raw: &[u8]) -> bool {
    match self.op {
      None => true,
      Some(op) => compare(op, &self.literal, value_raw),
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
  let (pointer, op, literal) = match ir {
    PredicateIr::And { c } => {
      for child in c {
        flatten(child, out)?;
      }
      return Ok(());
    }
    PredicateIr::Exists { p } => (p, None, Literal::Null),
    PredicateIr::Eq { p, v } => (p, Some(CompareOp::Eq), Literal::from_json(v)),
    PredicateIr::Lt { p, v } => (p, Some(CompareOp::Lt), Literal::from_json(v)),
    PredicateIr::Lte { p, v } => (p, Some(CompareOp::Lte), Literal::from_json(v)),
    PredicateIr::Gt { p, v } => (p, Some(CompareOp::Gt), Literal::from_json(v)),
    PredicateIr::Gte { p, v } => (p, Some(CompareOp::Gte), Literal::from_json(v)),
  };
  out.push(Leaf {
    pointer: JsonPointer::parse(&pointer)?,
    op,
    literal,
  });
  Ok(())
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

/// Compare the raw bytes of a JSON-encoded string value (including the
/// surrounding quotes) to a Rust `&str`. The hot path - interior contains
/// no backslash - byte-compares directly; escaped strings invoke
/// `serde_json::from_slice` to decode.
///
/// Returns `Err(())` only when an escaped interior fails to decode (i.e.
/// the JSON is malformed). Callers in resolve context map this to
/// [`TraverseError::Malformed`](crate::walker::TraverseError::Malformed)
/// to surface bad data; predicate context treats it as a non-match (via
/// [`string_eq`] below).
///
/// Shared with [`crate::resolve::step_object`]'s key comparison so the two
/// sites stay in sync on escape-decoding semantics.
pub(crate) fn quoted_string_eq(value_raw: &[u8], target: &str) -> Result<bool, ()> {
  if value_raw.len() < 2 || value_raw[0] != b'"' || value_raw[value_raw.len() - 1] != b'"' {
    return Ok(false);
  }
  let interior = &value_raw[1..value_raw.len() - 1];
  if !interior.contains(&b'\\') {
    return Ok(interior == target.as_bytes());
  }
  serde_json::from_slice::<String>(value_raw)
    .map(|s| s == target)
    .map_err(|_| ())
}

/// Byte-compare a JSON string value to a literal. Wraps [`quoted_string_eq`]
/// and folds malformed escapes into `false` (predicates are total).
fn string_eq(value_raw: &[u8], lit: &str) -> bool {
  quoted_string_eq(value_raw, lit).unwrap_or(false)
}

/// Decode a JSON string value (handling escapes) to an owned `String`, or
/// `None` if the value isn't a JSON string. Used by the ordering compares
/// (`lt`/`gt`/...) which need the decoded `String` to call `str::cmp`.
fn decode_string(value_raw: &[u8]) -> Option<String> {
  if value_raw.first() != Some(&b'"') {
    return None;
  }
  serde_json::from_slice::<String>(value_raw).ok()
}
