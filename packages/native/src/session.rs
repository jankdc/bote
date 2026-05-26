//! Async session: glues the sync evaluator to the chunk cache.
//!
//! A [`Session`] owns:
//!   - the [`ChunkCache`] (async byte fetch + LRU eviction)
//!   - a `Mutex<BitmapStore>` (cached bitmaps)
//!   - immutable source metadata (size, chunk size)
//!
//! Per-query (`resolve`, `get`, `has`) it runs the [`Session::drive`] retry
//! loop:
//!
//!   1. Build a [`Walker`] over an in-memory `HashMap<chunk_offset, ChunkRef>`
//!      of currently-pinned chunks, run the caller-supplied sync step.
//!   2. On `ChunkMiss(off)`, fetch a burst of chunks async, pin them in the
//!      map, retry.
//!   3. On success, drop pins and return.
//!
//! The pinned chunks accumulate for the duration of one query and are
//! released as soon as it returns. This lets the chunk cache evict freely
//! between queries while still guaranteeing in-flight requests aren't
//! pulled out from under them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::bitmap::BitmapStore;
use crate::cache::{CacheError, CacheOptions, ChunkCache, ChunkRef};
use crate::pointer::{JsonPointer, PointerParseError};
use crate::predicate::{CompiledPredicate, PredicateError};
use crate::resolve::{self, ChildEntry, Children, ResolveState, ValueLocation};
use crate::select::{CompiledSelect, SelectError};
use crate::source::Source;
use crate::walker::{AdvanceCommas, ChunkBytes, ChunkMiss, TraverseError, Walker};

#[derive(Debug, Error)]
pub enum SessionError {
  #[error(transparent)]
  Pointer(#[from] PointerParseError),
  #[error("traversal error: {0}")]
  Traverse(#[from] TraverseError),
  #[error(transparent)]
  Cache(#[from] CacheError),
  #[error("failed to parse JSON value: {0}")]
  Json(#[from] serde_json::Error),
  #[error("pointer did not resolve to a value")]
  NotFound,
  #[error(transparent)]
  Predicate(#[from] PredicateError),
  #[error(transparent)]
  Select(#[from] SelectError),
}

pub struct Session {
  pub source_size: u64,
  pub chunk_size: u64,
  pub cache: Arc<ChunkCache>,
  pub bitmaps: Mutex<BitmapStore>,
}

/// Cap on adaptive burst size for resolver retries. The resolver restarts
/// from the anchor on every chunk fault, so unbounded restarts give O(N²)
/// traversal work for an N-chunk query. Each `ChunkMiss` doubles the burst
/// (1, 2, 4, 8, ..., capped here) - short queries pay no over-fetch cost,
/// long queries converge to a near-single-pass traversal because the burst
/// quickly reaches the cap and the remaining chunks load in one or two hits.
const MAX_BURST: u64 = 256;

impl Session {
  pub fn new(source: Arc<dyn Source>, options: CacheOptions) -> Result<Arc<Self>, SessionError> {
    let source_size = source.size();
    let chunk_size = options.chunk_size as u64;
    let cache = ChunkCache::new(source, options)?;
    // Share a single byte counter between the bitmap store and the cache so
    // the cache's eviction loop can see lazy structural-bitmap growth.
    let bitmap_bytes = cache.bitmap_bytes_handle();
    Ok(Arc::new(Self {
      source_size,
      chunk_size,
      cache,
      bitmaps: Mutex::new(BitmapStore::with_bytes_counter(bitmap_bytes)),
    }))
  }

  pub async fn resolve_at(
    &self,
    pointer_str: &str,
    anchor_start: u64,
  ) -> Result<Option<ValueLocation>, SessionError> {
    let pointer = JsonPointer::parse(pointer_str)?;
    let mut pinned: HashMap<u64, ChunkRef> = HashMap::new();
    let result = self.run_resolve(&pointer, anchor_start, &mut pinned).await;
    drop(pinned);
    self.sync_bitmap_evictions();
    result
  }

  pub async fn has_at(&self, pointer_str: &str, anchor_start: u64) -> Result<bool, SessionError> {
    Ok(self.resolve_at(pointer_str, anchor_start).await?.is_some())
  }

  pub async fn get_at(
    &self,
    pointer_str: &str,
    anchor_start: u64,
  ) -> Result<serde_json::Value, SessionError> {
    let pointer = JsonPointer::parse(pointer_str)?;
    let mut pinned: HashMap<u64, ChunkRef> = HashMap::new();
    let result = async {
      let loc = self
        .run_resolve(&pointer, anchor_start, &mut pinned)
        .await?
        .ok_or(SessionError::NotFound)?;
      let bytes = self.read_range(loc.start, loc.end, &mut pinned).await?;
      Ok::<_, SessionError>(serde_json::from_slice(&bytes)?)
    }
    .await;
    drop(pinned);
    self.sync_bitmap_evictions();
    result
  }

  /// Count the children of the container `pointer_str` resolves to, with no
  /// materialization. A missing pointer or a non-container value is `0`
  /// (total and non-throwing, like `has_at`). The element/member count is the
  /// number of depth-0 commas plus one for a non-empty container - the same
  /// comma-bitmap popcount `step_array` uses - so cost is O(children) and
  /// resident memory stays constant in document size.
  pub async fn count_at(
    &self,
    pointer_str: &str,
    anchor_start: u64,
    pred: Option<&CompiledPredicate>,
  ) -> Result<u64, SessionError> {
    let pointer = JsonPointer::parse(pointer_str)?;
    let mut pinned: HashMap<u64, ChunkRef> = HashMap::new();
    let result = async {
      let Some(loc) = self
        .run_resolve(&pointer, anchor_start, &mut pinned)
        .await?
      else {
        return Ok(0); // missing pointer
      };
      let Some(children) = self.enter_container(loc, &mut pinned).await? else {
        return Ok(0); // not an object or array
      };
      match pred {
        None => self.count_children(children.next_offset, &mut pinned).await,
        Some(p) => self.count_matching(children, p, &mut pinned).await,
      }
    }
    .await;
    drop(pinned);
    self.sync_bitmap_evictions();
    result
  }

  /// Drive the comma-popcount counter from `start` (the byte just past the
  /// container's opening `{`/`[`) to its matching close. `CountState` persists
  /// across chunk faults so a long count resumes at the last chunk boundary.
  async fn count_children(
    &self,
    start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<u64, SessionError> {
    let mut state = CountState::new(start);
    let mut burst = 1u64;
    self
      .drive(
        pinned,
        |_| {
          let n = burst;
          burst = burst.saturating_mul(2).min(MAX_BURST);
          n
        },
        |walker| count_step(walker, &mut state),
      )
      .await
  }

  /// Count the children matching `pred`. Unlike the un-filtered count there is
  /// no comma-popcount shortcut - each child is resolved against the predicate.
  /// Memory stays bounded because `next_child` advances one chunk at a time,
  /// pruning pins behind the frontier as it goes.
  async fn count_matching(
    &self,
    mut cw: Children,
    pred: &CompiledPredicate,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<u64, SessionError> {
    let mut n = 0u64;
    loop {
      let Some(child) = self.next_child(&mut cw, pinned).await? else {
        return Ok(n);
      };
      if self
        .matches_predicate(pred, child.location().start, pinned)
        .await?
      {
        n += 1;
      }
    }
  }

  /// Evaluate `pred` against the child whose value starts at `child_start`.
  /// Each leaf resolves its sub-pointer relative to `child_start` (its own
  /// resumable drive) and compares off the resolved value's raw bytes; `and`
  /// short-circuits on the first unsatisfied leaf. Total and non-throwing: a
  /// missing sub-pointer fails the leaf, never errors.
  ///
  /// Leaves resolve *back* to `child_start` (behind the container scan
  /// frontier), so a child larger than the burst window can re-fault between
  /// leaves; the doubling burst converges once it spans the child. In practice
  /// predicate sub-pointers are shallow and children are small.
  pub async fn matches_predicate(
    &self,
    pred: &CompiledPredicate,
    child_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<bool, SessionError> {
    for leaf in pred.leaves() {
      let Some(loc) = self
        .run_resolve(leaf.pointer(), child_start, pinned)
        .await?
      else {
        return Ok(false);
      };
      if leaf.needs_value() {
        let raw = self.read_range(loc.start, loc.end, pinned).await?;
        if !leaf.satisfied_by(&raw) {
          return Ok(false);
        }
      }
    }
    Ok(true)
  }

  /// Project a matched child into its yielded value per `select`: a single
  /// sub-pointer yields the bare sub-value; a map yields an object of named
  /// sub-values in declared order. A missing sub-pointer yields `null`
  /// (projection is lossy, not a filter). Only the projected `[start, end)`
  /// bytes materialize - the rest of the child never does.
  pub async fn project(
    &self,
    select: &CompiledSelect,
    child_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<serde_json::Value, SessionError> {
    match select {
      CompiledSelect::One(ptr) => self.project_one(ptr, child_start, pinned).await,
      CompiledSelect::Map(fields) => {
        let mut obj = serde_json::Map::new();
        for (key, ptr) in fields {
          let value = self.project_one(ptr, child_start, pinned).await?;
          obj.insert(key.clone(), value);
        }
        Ok(serde_json::Value::Object(obj))
      }
    }
  }

  async fn project_one(
    &self,
    ptr: &JsonPointer,
    child_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<serde_json::Value, SessionError> {
    match self.run_resolve(ptr, child_start, pinned).await? {
      None => Ok(serde_json::Value::Null),
      Some(loc) => self.materialize(loc, pinned).await,
    }
  }

  /// Drop bitmaps for chunks the cache has evicted since the last drain.
  /// Without this the bitmap store grows unbounded - for a 100 MB
  /// document with a 1 MB chunk budget, bitmaps alone would retain
  /// ~90 MB (one set per chunk x 8 bitmap kinds x ~8 KB).
  ///
  /// Called from every place that releases per-query pins
  /// (`get_at` / `resolve_at` end-of-query, iter/walk end-of-yield)
  /// so the bitmap store stays in lockstep with the chunk cache.
  pub(crate) fn sync_bitmap_evictions(&self) {
    let evicted = self.cache.drain_evicted();
    if evicted.is_empty() {
      return;
    }
    let mut store = self.bitmaps.lock().unwrap();
    for off in evicted {
      store.evict(off);
    }
  }

  /// Open a child iterator over the container at `value_loc`. Returns
  /// `Ok(None)` if the value isn't an object or array.
  pub async fn enter_container(
    &self,
    value_loc: ValueLocation,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Option<Children>, SessionError> {
    self
      .drive(
        pinned,
        |_| 1,
        |walker| resolve::enter_container(walker, value_loc),
      )
      .await
  }

  /// Advance `cw` to the next child entry.
  pub async fn next_child(
    &self,
    cw: &mut Children,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Option<ChildEntry>, SessionError> {
    // walk / iter's `next_child` advances one element at a time - each call
    // typically stays inside one chunk. Burst=1 is fine for the common case,
    // and the walker's per-call invocation means there's no quadratic-restart
    // risk to amortize against.
    self
      .drive(pinned, |_| 1, |walker| resolve::next_child(walker, cw))
      .await
  }

  /// Materialize the JSON value at `loc` by reading and parsing its bytes.
  pub async fn materialize(
    &self,
    loc: ValueLocation,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<serde_json::Value, SessionError> {
    let bytes = self.read_range(loc.start, loc.end, pinned).await?;
    Ok(serde_json::from_slice(&bytes)?)
  }

  async fn run_resolve(
    &self,
    pointer: &JsonPointer,
    anchor_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Option<ValueLocation>, SessionError> {
    // ResolveState persists across `ChunkMiss` retries. The inner
    // `resolve_step` updates state only at iteration boundaries, so a
    // chunk fault during a long array walk redoes at most one element on
    // resumption - not the whole walk from the anchor.
    let mut state = ResolveState::new(anchor_start);
    let mut burst = 1u64;
    self
      .drive(
        pinned,
        |_| {
          let n = burst;
          burst = burst.saturating_mul(2).min(MAX_BURST);
          n
        },
        |walker| resolve::resolve_step(walker, pointer, &mut state),
      )
      .await
  }

  async fn read_range(
    &self,
    from: u64,
    to: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Vec<u8>, SessionError> {
    let chunk_size = self.chunk_size;
    self
      .drive(
        pinned,
        // We know the full byte range up front, so on each ChunkMiss fetch
        // the rest of it in one shot rather than restarting per chunk.
        move |off| to.saturating_sub(off).div_ceil(chunk_size).max(1),
        |walker| walker.read_range(from, to).map_err(TraverseError::from),
      )
      .await
  }

  /// The shared retry loop: build a fresh `Walker` over currently-pinned
  /// chunks, run a sync `step`, and on `ChunkMiss(off)` fetch a burst of
  /// chunks (sized by `burst_for`) and retry. Releases the bitmap-store
  /// lock before any `.await` so it never crosses an await point.
  async fn drive<T, S, B>(
    &self,
    pinned: &mut HashMap<u64, ChunkRef>,
    mut burst_for: B,
    mut step: S,
  ) -> Result<T, SessionError>
  where
    S: FnMut(&mut Walker<'_, PinnedChunks<'_>>) -> Result<T, TraverseError>,
    B: FnMut(u64) -> u64,
  {
    loop {
      let outcome = {
        let mut store = self.bitmaps.lock().unwrap();
        let provider = PinnedChunks { pinned };
        let mut walker = Walker::new(self.source_size, self.chunk_size, &mut store, &provider);
        step(&mut walker)
      };
      match outcome {
        Ok(v) => return Ok(v),
        Err(TraverseError::Pending(ChunkMiss(off))) => {
          let n = burst_for(off);
          self.burst_fetch(off, n, pinned).await?;
        }
        Err(e) => return Err(e.into()),
      }
    }
  }

  /// Fetch up to `n` consecutive chunks starting at `start`, skipping any
  /// already pinned.
  ///
  /// Before fetching, drops any entries in `pinned` whose offset is
  /// strictly less than `start`. All current callers
  /// (`resolve_step`, `next_child`, `read_range`) advance monotonically
  /// forward within a single query, so anything behind the next
  /// chunk-miss offset is past the scan frontier and cannot be needed
  /// again. Dropping those entries releases their pins so the cache can
  /// evict them under cap - without this the pin set grows linearly with
  /// the document for far-positional access (e.g. `get('/N-1')` on a
  /// large array), defeating the cache's memory invariant.
  ///
  /// NOTE: this couples burst-fetch correctness to forward-only access.
  /// If a future caller within one query needs to revisit an offset
  /// behind a previous chunk-miss, this prune must be guarded or moved
  /// to a resolver-driven mechanism that knows the walker's true
  /// retention window.
  async fn burst_fetch(
    &self,
    start: u64,
    n: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<(), CacheError> {
    pinned.retain(|&off, _| off >= start);
    let burst_end = start
      .saturating_add(n.saturating_mul(self.chunk_size))
      .min(self.source_size);
    let mut cur = start;
    while cur < burst_end {
      if let std::collections::hash_map::Entry::Vacant(slot) = pinned.entry(cur) {
        let chunk = self.cache.fetch(cur).await?;
        slot.insert(chunk);
      }
      cur = match cur.checked_add(self.chunk_size) {
        Some(v) => v,
        None => break,
      };
    }

    // Pruning above released pins and the fetches may have triggered
    // additional evictions to stay under cap. Drain bitmap evictions
    // now so the bitmap store stays in lockstep with the chunk cache.
    // otherwise bitmaps for long-evicted chunks accumulate until
    // end-of-query (`sync_bitmap_evictions` is also called there).
    self.sync_bitmap_evictions();
    Ok(())
  }
}

pub struct PinnedChunks<'a> {
  pinned: &'a HashMap<u64, ChunkRef>,
}

impl ChunkBytes for PinnedChunks<'_> {
  fn get_chunk(&self, chunk_offset: u64) -> Option<&[u8]> {
    self.pinned.get(&chunk_offset).map(|c| &c.data[..])
  }
}

/// Persisted across `ChunkMiss` retries while counting a container's
/// children, so a chunk fault mid-count resumes at the last committed chunk
/// boundary instead of recounting from the container's start.
struct CountState {
  /// Next byte to scan. Before the peek, the byte just past the opening
  /// `{`/`[`; after, a chunk-boundary commit point from `Partial`.
  offset: u64,
  /// Nesting depth at `offset`, relative to the container being counted.
  depth: u32,
  /// Depth-0 commas counted so far across resumes.
  consumed: u64,
  /// Set once the container is confirmed non-empty (peeked past the opener
  /// and did not immediately hit the matching close).
  peeked: bool,
}

impl CountState {
  fn new(start: u64) -> Self {
    Self {
      offset: start,
      depth: 0,
      consumed: 0,
      peeked: false,
    }
  }
}

/// Sync step for [`Session::count_children`]: returns the final child count
/// once the container's close is reached, or surfaces `ChunkMiss` (via `?`)
/// to fault the next chunk. `state` carries progress across faults.
fn count_step<P: ChunkBytes + ?Sized>(
  walker: &mut Walker<P>,
  state: &mut CountState,
) -> Result<u64, TraverseError> {
  if !state.peeked {
    // Empty-container short-circuit: a `}`/`]` immediately after the opener
    // means zero children (no value starts with a close). Commit the
    // whitespace-skipped offset before `byte_at` so a fault doesn't re-skip.
    let off = walker.skip_whitespace(state.offset)?;
    state.offset = off;
    match walker.byte_at(off)? {
      None => return Err(TraverseError::UnexpectedEof(off)),
      Some(b']' | b'}') => return Ok(0),
      Some(_) => {}
    }
    state.peeked = true;
  }
  loop {
    match walker.advance_top_level_commas(state.offset, state.depth, usize::MAX)? {
      // Non-empty container: child count is depth-0 commas + 1.
      AdvanceCommas::ArrayClosed { consumed } => return Ok(state.consumed + consumed as u64 + 1),
      // Commit progress; the next call faults the unloaded chunk via `ensure`,
      // surfacing ChunkMiss to the drive loop so it resumes from here.
      AdvanceCommas::Partial {
        offset,
        depth,
        consumed,
      } => {
        state.consumed += consumed as u64;
        state.offset = offset;
        state.depth = depth;
      }
      // Unreachable with `needed == usize::MAX` (the count never bottoms out),
      // but stay total: keep scanning from past the comma.
      AdvanceCommas::Found {
        offset_after_comma,
        consumed,
      } => {
        state.consumed += consumed as u64;
        state.offset = offset_after_comma;
        state.depth = 0;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::predicate::CompiledPredicate;
  use crate::source::InMemorySource;

  fn in_memory_session(data: Vec<u8>, chunk_size: usize, max_chunks: u32) -> Arc<Session> {
    let source: Arc<dyn Source> = Arc::new(InMemorySource::new(data));
    Session::new(
      source,
      CacheOptions {
        chunk_size,
        max_resident_chunks: max_chunks,
      },
    )
    .unwrap()
  }

  #[tokio::test]
  async fn get_simple_value() {
    let s = in_memory_session(br#"{"a":1,"b":2}"#.to_vec(), 64, 16);
    assert_eq!(s.get_at("/a", 0).await.unwrap(), serde_json::json!(1));
    assert_eq!(s.get_at("/b", 0).await.unwrap(), serde_json::json!(2));
  }

  #[tokio::test]
  async fn get_nested_value() {
    let s = in_memory_session(br#"{"u":{"n":"Alice"}}"#.to_vec(), 64, 16);
    assert_eq!(
      s.get_at("/u/n", 0).await.unwrap(),
      serde_json::json!("Alice")
    );
  }

  #[tokio::test]
  async fn get_missing_returns_not_found() {
    let s = in_memory_session(br#"{"a":1}"#.to_vec(), 64, 16);
    assert!(matches!(
      s.get_at("/missing", 0).await,
      Err(SessionError::NotFound)
    ));
  }

  #[tokio::test]
  async fn has_distinguishes_present_and_missing() {
    let s = in_memory_session(br#"{"a":1}"#.to_vec(), 64, 16);
    assert!(s.has_at("/a", 0).await.unwrap());
    assert!(!s.has_at("/b", 0).await.unwrap());
  }

  #[tokio::test]
  async fn get_across_chunks() {
    // Document spans many 64-byte chunks; chunk-driven bitmap chain must
    // resolve through them all.
    let mut doc = String::from("{\"skip\":[");
    for i in 0..100 {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&i.to_string());
    }
    doc.push_str("],\"target\":\"found\"}");
    let s = in_memory_session(doc.into_bytes(), 64, 256);
    assert_eq!(
      s.get_at("/target", 0).await.unwrap(),
      serde_json::json!("found")
    );
    assert_eq!(
      s.get_at("/skip/50", 0).await.unwrap(),
      serde_json::json!(50)
    );
  }

  #[tokio::test]
  async fn get_succeeds_when_document_exceeds_cap() {
    // ~30 KiB document; cap = 16 slots x 256 bytes = ~4 KiB worth of
    // chunks. A single query may temporarily pin more than the cap (chunks
    // in flight are pinned and can't be evicted), but it must still
    // succeed AND return to cap compliance once pins are released -
    // eviction runs on both fetch and unpin.
    let mut doc = String::from("{");
    for i in 0..2000 {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&format!("\"k{i:04}\":{i}"));
    }
    doc.push('}');
    let s = in_memory_session(doc.into_bytes(), 256, 16);
    assert_eq!(
      s.get_at("/k1500", 0).await.unwrap(),
      serde_json::json!(1500)
    );
    assert!(
      s.cache.resident_chunks() <= 16,
      "resident chunks {} exceeded cap after query completed",
      s.cache.resident_chunks()
    );
  }

  #[tokio::test]
  async fn get_root_pointer_returns_whole_document() {
    let s = in_memory_session(br#"[1,2,3]"#.to_vec(), 64, 16);
    assert_eq!(s.get_at("", 0).await.unwrap(), serde_json::json!([1, 2, 3]));
  }

  #[tokio::test]
  async fn count_array_elements() {
    let s = in_memory_session(br#"{"items":[10,20,30]}"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("/items", 0, None).await.unwrap(), 3);
  }

  #[tokio::test]
  async fn count_empty_array_is_zero() {
    let s = in_memory_session(br#"{"items":[]}"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("/items", 0, None).await.unwrap(), 0);
  }

  #[tokio::test]
  async fn count_single_element_array_is_one() {
    let s = in_memory_session(br#"[5]"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 1);
  }

  #[tokio::test]
  async fn count_object_members() {
    let s = in_memory_session(br#"{"a":1,"b":2,"c":3}"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 3);
  }

  #[tokio::test]
  async fn count_empty_object_is_zero() {
    let s = in_memory_session(br#"{}"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 0);
  }

  #[tokio::test]
  async fn count_ignores_nested_commas() {
    // Only depth-0 commas are element boundaries; nested array/object commas
    // must not inflate the count.
    let s = in_memory_session(br#"[{"x":[1,2,3]},{"y":3},[4,5]]"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 3);
  }

  #[tokio::test]
  async fn count_ignores_in_string_commas() {
    // Commas/braces/brackets inside strings are masked by the bitmaps.
    let s = in_memory_session(br#"["a,b","c}d","]e["]"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 3);
  }

  #[tokio::test]
  async fn count_non_container_is_zero() {
    let s = in_memory_session(br#"{"a":1}"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("/a", 0, None).await.unwrap(), 0);
  }

  #[tokio::test]
  async fn count_missing_is_zero() {
    let s = in_memory_session(br#"{"a":1}"#.to_vec(), 64, 16);
    assert_eq!(s.count_at("/missing", 0, None).await.unwrap(), 0);
  }

  #[tokio::test]
  async fn count_large_array_under_tight_cap() {
    // 2000 elements, chunk 256, cap 16: the count must be exact AND resident
    // chunks must return to the cap once the query releases its pins.
    let mut doc = String::from("[");
    for i in 0..2000 {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&format!("{{\"id\":{i}}}"));
    }
    doc.push(']');
    let s = in_memory_session(doc.into_bytes(), 256, 16);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 2000);
    assert!(
      s.cache.resident_chunks() <= 16,
      "resident chunks {} exceeded cap after count",
      s.cache.resident_chunks()
    );
  }

  #[tokio::test]
  async fn count_array_resumes_across_chunks() {
    // 100 elements across many 64-byte chunks: the comma popcount must resume
    // correctly through chunk faults.
    let mut doc = String::from("[");
    for i in 0..100 {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&i.to_string());
    }
    doc.push(']');
    let s = in_memory_session(doc.into_bytes(), 64, 256);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 100);
  }

  #[tokio::test]
  async fn count_with_where_filters_array() {
    let s = in_memory_session(
      br#"[{"s":"paid"},{"s":"x"},{"s":"paid"},{"s":"paid"}]"#.to_vec(),
      64,
      16,
    );
    let paid = CompiledPredicate::parse(r#"{"t":"eq","p":"/s","v":"paid"}"#).unwrap();
    assert_eq!(s.count_at("", 0, Some(&paid)).await.unwrap(), 3);
    assert_eq!(s.count_at("", 0, None).await.unwrap(), 4);
  }

  #[tokio::test]
  async fn count_with_where_and_combines() {
    let s = in_memory_session(
      br#"[{"s":"paid","t":50},{"s":"paid","t":150},{"s":"x","t":200}]"#.to_vec(),
      64,
      16,
    );
    let pred = CompiledPredicate::parse(
      r#"{"t":"and","c":[{"t":"eq","p":"/s","v":"paid"},{"t":"gte","p":"/t","v":100}]}"#,
    )
    .unwrap();
    assert_eq!(s.count_at("", 0, Some(&pred)).await.unwrap(), 1);
  }

  #[tokio::test]
  async fn count_with_where_under_tight_cap() {
    // Filtered count over a large array must stay bounded in memory.
    let mut doc = String::from("[");
    for i in 0..2000 {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&format!("{{\"id\":{i},\"keep\":{}}}", i % 2 == 0));
    }
    doc.push(']');
    let s = in_memory_session(doc.into_bytes(), 256, 16);
    let pred = CompiledPredicate::parse(r#"{"t":"eq","p":"/keep","v":true}"#).unwrap();
    assert_eq!(s.count_at("", 0, Some(&pred)).await.unwrap(), 1000);
    assert!(
      s.cache.resident_chunks() <= 16,
      "resident chunks {} exceeded cap after filtered count",
      s.cache.resident_chunks()
    );
  }
}
