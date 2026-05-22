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
use crate::resolve::{self, ChildEntry, Children, ResolveState, ValueLocation};
use crate::source::Source;
use crate::walker::{ChunkBytes, ChunkMiss, TraverseError, Walker};

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

  /// Drop bitmaps for chunks the cache has evicted since the last drain.
  /// Without this the bitmap store grows unbounded - for a 100 MB
  /// document with a 1 MB chunk budget, bitmaps alone would retain
  /// ~90 MB (one set per chunk × 8 bitmap kinds × ~8 KB).
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

#[cfg(test)]
mod tests {
  use super::*;
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
    // ~30 KiB document; cap = 16 slots × 256 bytes = ~4 KiB worth of
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
}
