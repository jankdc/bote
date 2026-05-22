//! JSON Pointer evaluator.
//!
//! Walks a parsed [`JsonPointer`] over a bitmap-driven [`Walker`], descending
//! one reference token at a time into objects (by key) and arrays (by
//! index). Returns the byte range covering the resolved value or `None`,
//! when any token along the path doesn't address an existing member.

use crate::pointer::{token_as_array_index, JsonPointer};
use crate::walker::{AdvanceCommas, ChunkBytes, TraverseError, Walker};

/// Byte range `[start, end)` covering a JSON value in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueLocation {
  pub start: u64,
  pub end: u64,
}

/// Per-query resolver state. Persisted across `ChunkMiss` retries so a long
/// array walk doesn't restart from the anchor every time a new chunk is
/// faulted in - see [`resolve_step`].
#[derive(Debug, Clone)]
pub struct ResolveState {
  /// 0-based index of the pointer token we're currently processing.
  /// Reaches `pointer.tokens().len()` once all tokens are resolved.
  token_idx: usize,
  /// Byte offset where the current token's value starts. Before descending
  /// this holds the offset of `{` or `[`; after descending into a member
  /// it points at the member value's first byte.
  start: u64,
  /// Per-token scan state. `None` before descending into a container or
  /// after a token has been fully resolved.
  loop_state: Option<LoopState>,
}

#[derive(Debug, Clone)]
enum LoopState {
  Object(ObjectLoopState),
  Array(ArrayLoopState),
}

#[derive(Debug, Clone)]
struct ObjectLoopState {
  /// Byte offset where the next iteration's key scan begins.
  offset: u64,
}

#[derive(Debug, Clone)]
struct ArrayLoopState {
  /// Byte offset where the next element scan begins.
  offset: u64,
  /// Index of the next element to be considered.
  index: usize,
  /// Container-nesting depth at `offset`, relative to the array we entered.
  /// Used by the comma-bitmap fast path so a `ChunkMiss` mid-scan can
  /// resume without losing depth state. Always 0 at iteration boundaries
  /// the slow path sets up; the fast path may commit mid-nesting at a
  /// chunk boundary.
  depth: u32,
}

impl ResolveState {
  pub fn new(start: u64) -> Self {
    Self {
      token_idx: 0,
      start,
      loop_state: None,
    }
  }
}

/// Drive the resolver forward against the current `state`.
///
/// Each call tries to make as much progress as possible. On success, the
/// final `ValueLocation` is returned and `state` is left at the terminal
/// position. On a `ChunkMiss` chunk fault, `state` is updated to the last
/// **iteration boundary** (start of the key/value being scanned) and the
/// error is propagated - re-calling with the same `state` after fetching
/// the missing chunk resumes from that point, redoing at most one
/// element's worth of work instead of the whole traversal.
pub fn resolve_step<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  pointer: &JsonPointer,
  state: &mut ResolveState,
) -> Result<Option<ValueLocation>, TraverseError> {
  let tokens = pointer.tokens();
  while state.token_idx < tokens.len() {
    if state.loop_state.is_none() {
      // First entry into this token - figure out the container kind. Commit
      // `state.start` to the skipped-whitespace position before the byte
      // fetch so a `ChunkMiss` from `byte_at` doesn't re-skip on retry.
      let s = walker.skip_whitespace(state.start)?;
      state.start = s;
      let b = walker.byte_at(s)?.ok_or(TraverseError::UnexpectedEof(s))?;
      match b {
        b'{' => {
          state.loop_state = Some(LoopState::Object(ObjectLoopState { offset: s + 1 }));
        }
        b'[' => {
          state.loop_state = Some(LoopState::Array(ArrayLoopState {
            offset: s + 1,
            index: 0,
            depth: 0,
          }));
        }
        _ => return Ok(None),
      }
    }
    let token = &tokens[state.token_idx];
    let descend = match state.loop_state.as_mut().expect("set just above") {
      LoopState::Object(o) => step_object(walker, token, o)?,
      LoopState::Array(a) => step_array(walker, token, a)?,
    };
    match descend {
      Some(value_start) => {
        state.start = value_start;
        state.token_idx += 1;
        state.loop_state = None;
      }
      None => return Ok(None),
    }
  }
  let end = walker.skip_value(state.start)?;
  Ok(Some(ValueLocation {
    start: state.start,
    end,
  }))
}

/// Advance an object scan, updating `state.offset` only after each fully
/// successful iteration. A `ChunkMiss` mid-iteration leaves `state` at the
/// previous iteration's boundary, so resumption redoes at most one key.
fn step_object<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  target: &str,
  state: &mut ObjectLoopState,
) -> Result<Option<u64>, TraverseError> {
  loop {
    let iter_offset = state.offset;
    let offset = walker.skip_whitespace(iter_offset)?;
    match walker.byte_at(offset)? {
      None => return Err(TraverseError::UnexpectedEof(offset)),
      Some(b'}') => return Ok(None),
      Some(b'"') => {}
      Some(_) => return Err(TraverseError::Malformed(offset)),
    }
    let key_close = walker
      .next_string_close(offset + 1)?
      .ok_or(TraverseError::UnexpectedEof(offset))?;

    // fast path: JSON escapes (`\n`, `\"`, `\uXXXX`, …) only
    // ever *shrink* a string's byte count, so if the raw byte span between
    // the quotes is shorter than the pointer target, no decoding can make
    // them equal - skip the `read_range` allocation entirely. When lengths
    // could match, peek for a backslash: with none, the raw bytes equal the
    // decoded key and we byte-compare; otherwise fall through to a real
    // serde decode.
    let raw_len = (key_close - offset).saturating_sub(1) as usize;
    let target_bytes = target.as_bytes();
    let matches = if raw_len < target_bytes.len() {
      false
    } else {
      let raw = walker.read_range(offset, key_close + 1)?;
      let inner = &raw[1..raw.len() - 1];
      if !inner.contains(&b'\\') {
        inner == target_bytes
      } else {
        let decoded: String =
          serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(offset))?;
        decoded == target
      }
    };
    let post_key = walker.skip_whitespace(key_close + 1)?;
    if walker.byte_at(post_key)? != Some(b':') {
      return Err(TraverseError::Malformed(post_key));
    }
    let value_start = walker.skip_whitespace(post_key + 1)?;
    if matches {
      return Ok(Some(value_start));
    }
    let value_end = walker.skip_value(value_start)?;
    let after = walker.skip_whitespace(value_end)?;
    match walker.byte_at(after)? {
      Some(b',') => state.offset = after + 1,
      Some(b'}') => return Ok(None),
      _ => return Err(TraverseError::Malformed(after)),
    }
  }
}

/// Advance an array scan, updating `state.offset` / `state.index` only
/// after each fully successful iteration.
fn step_array<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  target: &str,
  state: &mut ArrayLoopState,
) -> Result<Option<u64>, TraverseError> {
  let Some(target_index) = token_as_array_index(target) else {
    return Ok(None);
  };

  // Fast path: jump to the target element by counting depth-0 commas in
  // the comma bitmap, skipping per-element skip_value calls.
  while state.index < target_index {
    let needed = target_index - state.index;
    match walker.advance_top_level_commas(state.offset, state.depth, needed)? {
      AdvanceCommas::Found {
        offset_after_comma,
        consumed,
      } => {
        state.offset = offset_after_comma;
        state.index += consumed;
        state.depth = 0;
      }
      AdvanceCommas::ArrayClosed { consumed: _ } => {
        // The array ended before the target index existed.
        return Ok(None);
      }
      AdvanceCommas::Partial {
        offset,
        depth,
        consumed,
      } => {
        state.offset = offset;
        state.index += consumed;
        state.depth = depth;
        // Loop: next iteration will re-enter the fast path. If the next
        // chunk is now unresident the call itself surfaces ChunkMiss
        // via `ensure`, which propagates to the session retry loop.
      }
    }
  }
  loop {
    let iter_offset = state.offset;
    let iter_index = state.index;
    let offset = walker.skip_whitespace(iter_offset)?;
    match walker.byte_at(offset)? {
      None => return Err(TraverseError::UnexpectedEof(offset)),
      Some(b']') => return Ok(None),
      _ => {}
    }
    if iter_index == target_index {
      return Ok(Some(offset));
    }
    let value_end = walker.skip_value(offset)?;
    let after = walker.skip_whitespace(value_end)?;
    match walker.byte_at(after)? {
      Some(b',') => {
        state.offset = after + 1;
        state.index = iter_index + 1;
      }
      Some(b']') => return Ok(None),
      _ => return Err(TraverseError::Malformed(after)),
    }
  }
}

/// Kind of JSON container being iterated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
  Object,
  Array,
}

/// Cursor over the children of an object or array value. Created from
/// [`enter_container`] and advanced one entry at a time by [`next_child`].
#[derive(Debug, Clone)]
pub struct Children {
  pub kind: ContainerKind,
  pub next_offset: u64,
  pub index: usize,
}

/// One yielded child of a container.
#[derive(Debug, Clone)]
pub enum ChildEntry {
  /// Object member: decoded key plus the byte range of the value.
  Member {
    key: String,
    location: ValueLocation,
  },
  /// Array element: zero-based index plus the byte range of the value.
  Element {
    index: usize,
    location: ValueLocation,
  },
}

impl ChildEntry {
  pub fn location(&self) -> ValueLocation {
    match self {
      Self::Member { location, .. } | Self::Element { location, .. } => *location,
    }
  }
}

/// Position a [`Children`] at the first child of the container at
/// `value_loc`. Returns `Ok(None)` if the value isn't a container.
pub fn enter_container<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  value_loc: ValueLocation,
) -> Result<Option<Children>, TraverseError> {
  let open = walker.skip_whitespace(value_loc.start)?;
  let byte = walker
    .byte_at(open)?
    .ok_or(TraverseError::UnexpectedEof(open))?;
  let kind = match byte {
    b'{' => ContainerKind::Object,
    b'[' => ContainerKind::Array,
    _ => return Ok(None),
  };
  Ok(Some(Children {
    kind,
    next_offset: open + 1,
    index: 0,
  }))
}

/// Advance `cw` to the next child entry. Returns `Ok(None)` when the
/// container is exhausted (closing `}` or `]` reached).
pub fn next_child<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  match cw.kind {
    ContainerKind::Object => next_object_member(walker, cw),
    ContainerKind::Array => next_array_element(walker, cw),
  }
}

fn next_object_member<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cw.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b'}') => {
      cw.next_offset = offset + 1;
      return Ok(None);
    }
    Some(b'"') => {}
    Some(_) => return Err(TraverseError::Malformed(offset)),
  }
  let key_close = walker
    .next_string_close(offset + 1)?
    .ok_or(TraverseError::UnexpectedEof(offset))?;
  let raw = walker.read_range(offset, key_close + 1)?;
  let key: String = serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(offset))?;
  let post_key = walker.skip_whitespace(key_close + 1)?;
  if walker.byte_at(post_key)? != Some(b':') {
    return Err(TraverseError::Malformed(post_key));
  }
  let value_start = walker.skip_whitespace(post_key + 1)?;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cw.next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b'}') => after, // next call lands on `}` and terminates
    Some(_) | None => return Err(TraverseError::Malformed(after)),
  };
  Ok(Some(ChildEntry::Member {
    key,
    location: ValueLocation {
      start: value_start,
      end: value_end,
    },
  }))
}

fn next_array_element<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  cw: &mut Children,
) -> Result<Option<ChildEntry>, TraverseError> {
  let offset = walker.skip_whitespace(cw.next_offset)?;
  match walker.byte_at(offset)? {
    None => return Err(TraverseError::UnexpectedEof(offset)),
    Some(b']') => {
      cw.next_offset = offset + 1;
      return Ok(None);
    }
    _ => {}
  }
  let value_start = offset;
  let value_end = walker.skip_value(value_start)?;
  let after = walker.skip_whitespace(value_end)?;
  cw.next_offset = match walker.byte_at(after)? {
    Some(b',') => after + 1,
    Some(b']') => after,
    Some(_) | None => return Err(TraverseError::Malformed(after)),
  };
  let index = cw.index;
  cw.index += 1;
  Ok(Some(ChildEntry::Element {
    index,
    location: ValueLocation {
      start: value_start,
      end: value_end,
    },
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::bitmap::BitmapStore;
  use crate::walker::{ChunkBytes, Walker};
  use std::collections::HashMap;

  struct MemoryProvider {
    chunks: HashMap<u64, Vec<u8>>,
  }

  impl ChunkBytes for MemoryProvider {
    fn get_chunk(&self, chunk_offset: u64) -> Option<&[u8]> {
      self.chunks.get(&chunk_offset).map(Vec::as_slice)
    }
  }

  fn chunked(source: &[u8], chunk_size: usize) -> MemoryProvider {
    let mut chunks = HashMap::new();
    let mut offset = 0u64;
    while (offset as usize) < source.len() {
      let end = (offset as usize + chunk_size).min(source.len());
      chunks.insert(offset, source[offset as usize..end].to_vec());
      offset += chunk_size as u64;
    }
    MemoryProvider { chunks }
  }

  fn resolve_value(source: &[u8], pointer: &str, chunk_size: u64) -> Option<serde_json::Value> {
    let provider = chunked(source, chunk_size as usize);
    let mut store = BitmapStore::new();
    let mut walker = Walker::new(source.len() as u64, chunk_size, &mut store, &provider);
    let parsed = JsonPointer::parse(pointer).unwrap();
    let mut state = ResolveState::new(0);
    let loc = resolve_step(&mut walker, &parsed, &mut state).unwrap()?;
    let bytes = walker.read_range(loc.start, loc.end).unwrap();
    serde_json::from_slice(&bytes).ok()
  }

  fn assert_get(source: &[u8], pointer: &str, expected: serde_json::Value) {
    for &chunk_size in &[64u64, 128, 4096] {
      let got = resolve_value(source, pointer, chunk_size)
        .unwrap_or_else(|| panic!("not found: {pointer} (chunk {chunk_size})"));
      assert_eq!(got, expected, "pointer={pointer} chunk_size={chunk_size}");
    }
  }

  fn assert_not_found(source: &[u8], pointer: &str) {
    for &chunk_size in &[64u64, 128, 4096] {
      assert!(
        resolve_value(source, pointer, chunk_size).is_none(),
        "expected None for {pointer} (chunk {chunk_size})"
      );
    }
  }

  #[test]
  fn pointer_root_returns_whole_document() {
    let doc = br#"{"a":1,"b":2}"#;
    assert_get(doc, "", serde_json::json!({"a": 1, "b": 2}));
  }

  #[test]
  fn object_key_lookup() {
    let doc = br#"{"a":1,"b":2}"#;
    assert_get(doc, "/a", serde_json::json!(1));
    assert_get(doc, "/b", serde_json::json!(2));
  }

  #[test]
  fn object_key_missing_returns_none() {
    let doc = br#"{"a":1}"#;
    assert_not_found(doc, "/missing");
  }

  #[test]
  fn object_nested_traversal() {
    let doc = br#"{"user":{"name":{"first":"Alice","last":"Smith"},"age":30}}"#;
    assert_get(doc, "/user/name/first", serde_json::json!("Alice"));
    assert_get(doc, "/user/name/last", serde_json::json!("Smith"));
    assert_get(doc, "/user/age", serde_json::json!(30));
  }

  #[test]
  fn array_index_access() {
    let doc = br#"[10,20,30,40,50]"#;
    assert_get(doc, "/0", serde_json::json!(10));
    assert_get(doc, "/2", serde_json::json!(30));
    assert_get(doc, "/4", serde_json::json!(50));
    assert_not_found(doc, "/5");
  }

  #[test]
  fn array_index_dash_is_never_found() {
    let doc = br#"[1,2,3]"#;
    assert_not_found(doc, "/-");
  }

  #[test]
  fn mixed_object_and_array_traversal() {
    let doc = br#"{"orders":[{"id":1,"qty":5},{"id":2,"qty":7}]}"#;
    assert_get(doc, "/orders/0/id", serde_json::json!(1));
    assert_get(doc, "/orders/1/qty", serde_json::json!(7));
  }

  #[test]
  fn primitive_values_resolve() {
    let doc = br#"{"t":true,"f":false,"n":null,"i":-42,"x":1.5e3}"#;
    assert_get(doc, "/t", serde_json::json!(true));
    assert_get(doc, "/f", serde_json::json!(false));
    assert_get(doc, "/n", serde_json::json!(null));
    assert_get(doc, "/i", serde_json::json!(-42));
    assert_get(doc, "/x", serde_json::json!(1500.0));
  }

  #[test]
  fn key_pointer_escapes() {
    let doc = br#"{"a/b":1,"c~d":2}"#;
    // Pointer escapes: `/` -> `~1`, `~` -> `~0`.
    assert_get(doc, "/a~1b", serde_json::json!(1));
    assert_get(doc, "/c~0d", serde_json::json!(2));
  }

  #[test]
  fn key_json_escapes() {
    let doc = br#"{"with\"quote":"v","with\\backslash":"w","newline\nkey":"x"}"#;
    assert_get(doc, "/with\"quote", serde_json::json!("v"));
    assert_get(doc, "/with\\backslash", serde_json::json!("w"));
    assert_get(doc, "/newline\nkey", serde_json::json!("x"));
  }

  #[test]
  fn whitespace_between_tokens() {
    let doc = br#"  {  "a"  :  [  1  ,  2  ,  3  ]  }  "#;
    assert_get(doc, "/a/1", serde_json::json!(2));
  }

  #[test]
  fn array_nested() {
    let doc = br#"[[1,2],[3,4],[5,6]]"#;
    assert_get(doc, "/0/0", serde_json::json!(1));
    assert_get(doc, "/1/1", serde_json::json!(4));
    assert_get(doc, "/2/0", serde_json::json!(5));
  }

  #[test]
  fn pointer_rfc6901_section_5_examples() {
    let doc = br#"{
        "foo": ["bar", "baz"],
        "": 0,
        "a/b": 1,
        "c%d": 2,
        "e^f": 3,
        "g|h": 4,
        "i\\j": 5,
        "k\"l": 6,
        " ": 7,
        "m~n": 8
    }"#;
    let cases: &[(&str, serde_json::Value)] = &[
      ("", serde_json::from_slice(doc).unwrap()),
      ("/foo", serde_json::json!(["bar", "baz"])),
      ("/foo/0", serde_json::json!("bar")),
      ("/", serde_json::json!(0)),
      ("/a~1b", serde_json::json!(1)),
      ("/c%d", serde_json::json!(2)),
      ("/e^f", serde_json::json!(3)),
      ("/g|h", serde_json::json!(4)),
      ("/i\\j", serde_json::json!(5)),
      ("/k\"l", serde_json::json!(6)),
      ("/ ", serde_json::json!(7)),
      ("/m~0n", serde_json::json!(8)),
    ];
    for (ptr, expected) in cases {
      assert_get(doc, ptr, expected.clone());
    }
  }

  #[test]
  fn skip_container_walks_nested_braces() {
    // The target key comes after a value with deeply nested objects/arrays.
    // Tests that skip_value correctly walks past `{`/`}` and `[`/`]` pairs.
    let doc = br#"{"first":{"a":{"b":{"c":[1,[2,[3,{"d":4}]]]}}},"target":99}"#;
    assert_get(doc, "/target", serde_json::json!(99));
    assert_get(doc, "/first/a/b/c/1/1/1/d", serde_json::json!(4));
  }

  #[test]
  fn skip_value_handles_strings_with_brackets() {
    let doc = br#"{"x":"this has } and , in it","y":2}"#;
    assert_get(doc, "/y", serde_json::json!(2));
  }

  #[test]
  fn chunk_cross_boundary_pointer_resolution() {
    // Build a document where each nested level lives in a different chunk.
    // chunk_size=64 forces every traversal to cross at least one boundary.
    let mut body = String::from("{\"a\":");
    body.push_str(&" ".repeat(60));
    body.push_str("{\"b\":");
    body.push_str(&" ".repeat(60));
    body.push_str("{\"c\":");
    body.push_str(&" ".repeat(60));
    body.push_str("42}}}");
    let doc = body.as_bytes();
    let val = resolve_value(doc, "/a/b/c", 64).unwrap();
    assert_eq!(val, serde_json::json!(42));
  }

  /// Walk every pointer that resolves inside `value` and return them.
  fn enumerate_pointers(value: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
      out.push((prefix.to_owned(), v.clone()));
      match v {
        serde_json::Value::Object(map) => {
          for (k, child) in map {
            let encoded = k.replace('~', "~0").replace('/', "~1");
            walk(&format!("{prefix}/{encoded}"), child, out);
          }
        }
        serde_json::Value::Array(items) => {
          for (i, child) in items.iter().enumerate() {
            walk(&format!("{prefix}/{i}"), child, out);
          }
        }
        _ => {}
      }
    }
    let mut out = Vec::new();
    walk("", value, &mut out);
    out
  }

  /// Deterministic LCG over a small alphabet - produces structurally varied
  /// JSON documents up to a target depth.
  fn random_json(seed: u64, depth: u32) -> serde_json::Value {
    fn step(state: &mut u64) -> u64 {
      *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
      *state >> 33
    }
    fn build(state: &mut u64, depth: u32) -> serde_json::Value {
      if depth == 0 {
        return match step(state) % 5 {
          0 => serde_json::Value::Null,
          1 => serde_json::Value::Bool((step(state) & 1) == 1),
          2 => serde_json::Value::Number((step(state) as i64).into()),
          3 => serde_json::json!(format!("s{}", step(state) % 1000)),
          _ => serde_json::json!(format!(
            "with \"quote\" and \\backslash {}",
            step(state) % 100
          )),
        };
      }
      match step(state) % 3 {
        0 => {
          let n = (step(state) % 4) as usize + 1;
          let mut map = serde_json::Map::new();
          for i in 0..n {
            map.insert(
              format!("k{i}_{}", step(state) % 100),
              build(state, depth - 1),
            );
          }
          serde_json::Value::Object(map)
        }
        1 => {
          let n = (step(state) % 5) as usize + 1;
          serde_json::Value::Array((0..n).map(|_| build(state, depth - 1)).collect())
        }
        _ => build(state, 0),
      }
    }
    let mut s = seed;
    build(&mut s, depth)
  }

  #[test]
  fn fuzz_matches_serde_oracle() {
    // 32 random documents × every pointer that resolves × 3 chunk sizes.
    for seed in 0..32u64 {
      let value = random_json(seed.wrapping_mul(0x9E37_79B1_7F4A_7C15), 4);
      let serialized = serde_json::to_vec(&value).unwrap();
      let pointers = enumerate_pointers(&value);
      for (pointer, expected) in pointers {
        for &chunk_size in &[64u64, 256, 1024] {
          let got = resolve_value(&serialized, &pointer, chunk_size).unwrap_or_else(|| {
            panic!("seed={seed} pointer={pointer:?} chunk={chunk_size}: not found")
          });
          assert_eq!(
            got, expected,
            "seed={seed} pointer={pointer:?} chunk={chunk_size}"
          );
        }
      }
    }
  }

  #[test]
  fn array_large_index_through_many_chunks() {
    // 1000 elements, each ~20 bytes, total ~20 KB; chunk_size 256 forces
    // ~80 chunks. The traversal must skip 500 elements then materialize.
    let elements: Vec<String> = (0..1000).map(|i| format!("\"item-{i:04}\"")).collect();
    let body = format!("[{}]", elements.join(","));
    let val = resolve_value(body.as_bytes(), "/500", 256).unwrap();
    assert_eq!(val, serde_json::json!("item-0500"));
  }
}
