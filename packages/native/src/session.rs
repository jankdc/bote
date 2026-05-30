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
use crate::path::Segment;
use crate::resolve::{self, ChildEntry, Children, ResolveState, ValueLocation};
use crate::select::SelectError;
use crate::source::Source;
use crate::walker::{self, ChunkBytes, ChunkMiss, SkipState, TraverseError, Walker};

#[derive(Debug, Error)]
pub enum SessionError {
  #[error("traversal error: {0}")]
  Traverse(#[from] TraverseError),
  #[error(transparent)]
  Cache(#[from] CacheError),
  #[error("failed to parse JSON value: {0}")]
  Json(#[from] serde_json::Error),
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
pub(crate) const MAX_BURST: u64 = 256;

/// Adaptive doubling burst schedule used by every resolver that doesn't know
/// the value's extent up front (`run_locate`, `skip_value_at`, `count::children`).
/// Each call yields the current burst (in chunks) and doubles it for the next
/// call, capped at [`MAX_BURST`]. The returned closure is move-bound and
/// stateful; use one per `Session::drive` invocation.
pub(crate) fn doubling_burst() -> impl FnMut(u64) -> u64 {
  let mut n = 1u64;
  move |_off| {
    let cur = n;
    n = n.saturating_mul(2).min(MAX_BURST);
    cur
  }
}

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

  pub async fn locate_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
  ) -> Result<Option<u64>, SessionError> {
    let mut q = Query::new(self);
    self.run_locate(path, anchor_start, &mut q.pinned).await
  }

  pub async fn has_at(&self, path: &[Segment], anchor_start: u64) -> Result<bool, SessionError> {
    let mut q = Query::new(self);
    Ok(
      self
        .run_resolve(path, anchor_start, &mut q.pinned)
        .await?
        .is_some(),
    )
  }

  pub async fn get_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
  ) -> Result<Option<serde_json::Value>, SessionError> {
    let mut q = Query::new(self);
    let Some(loc) = self.run_resolve(path, anchor_start, &mut q.pinned).await? else {
      return Ok(None);
    };
    let bytes = self.read_range(loc.start, loc.end, &mut q.pinned).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
  }

  /// Drop bitmaps for chunks the cache has evicted since the last drain.
  /// Without this the bitmap store grows unbounded - for a 100 MB
  /// document with a 1 MB chunk budget, bitmaps alone would retain
  /// ~90 MB (one set per chunk x 8 bitmap kinds x ~8 KB).
  ///
  /// Called from every place that releases per-query pins
  /// (`get_at` / `locate_at` end-of-query, iter/walk end-of-yield)
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

  /// Drop every pin except the one covering `next_offset`. Bounds the
  /// resident-pin count to 1 between iterator yields so the cache's own
  /// eviction loop is free to maintain its resident-chunk slot cap. When
  /// `next_offset >= source_size` (iteration walked off the end) clears
  /// everything.
  pub(crate) fn prune_pins(&self, pinned: &mut HashMap<u64, ChunkRef>, next_offset: u64) {
    if next_offset >= self.source_size {
      pinned.clear();
      return;
    }
    let keep = (next_offset / self.chunk_size) * self.chunk_size;
    pinned.retain(|&off, _| off == keep);
  }

  /// Frontier-prune followed by a bitmap drain. The order is load-bearing:
  /// pin releases populate the cache's eviction queue (`unpin` ->
  /// `evict_to_caps`), then [`sync_bitmap_evictions`](Self::sync_bitmap_evictions)
  /// reads that queue and applies it to the bitmap store. Doing it the
  /// other way around drains an empty queue and the bitmaps for chunks
  /// about to be evicted persist until the next sync - the leak path
  /// the `bitmap_evict_drains_only_after_unpin` test guards against.
  ///
  /// Use at every iterator step where the walker has advanced.
  pub(crate) fn prune_frontier_and_sync(
    &self,
    pinned: &mut HashMap<u64, ChunkRef>,
    next_offset: u64,
  ) {
    self.prune_pins(pinned, next_offset);
    self.sync_bitmap_evictions();
  }

  /// Open a child iterator over the container starting at `value_start`.
  /// Returns `Ok(None)` if the value isn't an object or array.
  pub async fn enter_container(
    &self,
    value_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Option<Children>, SessionError> {
    self
      .drive(
        pinned,
        |_| 1,
        |walker| resolve::enter_container(walker, value_start),
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

  /// Resolve `path` starting at `anchor_start`, returning only the
  /// resolved value's **start offset** (no extent walk).
  ///
  /// Used by the entry points that don't need the value's bytes
  /// (`locate_at`, container-walking iterators, `count`). Pairs with
  /// [`Session::skip_value_at`] in `run_resolve` to build the full
  /// `ValueLocation` only when a caller actually needs it.
  pub(crate) async fn run_locate(
    &self,
    path: &[Segment],
    anchor_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Option<u64>, SessionError> {
    // ResolveState persists across `ChunkMiss` retries. The inner
    // `resolve_step` updates state only at iteration boundaries, so a
    // chunk fault during a long array walk redoes at most one element on
    // resumption - not the whole walk from the anchor.
    let mut state = ResolveState::new(anchor_start);
    self
      .drive(pinned, doubling_burst(), |walker| {
        resolve::resolve_step(walker, path, &mut state)
      })
      .await
  }

  /// Resolve `path` to a full `[start, end)` byte range. Equivalent to
  /// `run_locate` followed by `skip_value_at` on the start.
  pub(crate) async fn run_resolve(
    &self,
    path: &[Segment],
    anchor_start: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<Option<ValueLocation>, SessionError> {
    let Some(start) = self.run_locate(path, anchor_start, pinned).await? else {
      return Ok(None);
    };
    let end = self.skip_value_at(start, pinned).await?;
    Ok(Some(ValueLocation { start, end }))
  }

  pub(crate) async fn skip_value_at(
    &self,
    from: u64,
    pinned: &mut HashMap<u64, ChunkRef>,
  ) -> Result<u64, SessionError> {
    // Adaptive burst: the value's extent is unknown. The `doubling_burst`
    // schedule (1, 2, 4, ..., MAX_BURST) means short values pay no over-fetch
    // and long ones converge to near-single-pass once the burst caps out.
    let mut state = SkipState::start(from);
    self
      .drive(pinned, doubling_burst(), |walker| {
        walker::skip_value_step(walker, &mut state)
      })
      .await
  }

  pub(crate) async fn read_range(
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
  pub(crate) async fn drive<T, S, B>(
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
    for chunk in self.cache.fetch(start, n).await? {
      pinned.entry(chunk.offset).or_insert(chunk);
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

/// RAII scope for a one-shot query (`locate_at` / `get_at` / `count_at`):
/// owns the pinned-chunk map and, on drop, releases its pins and drains
/// bitmap evictions so both native pools return under cap - including on the
/// early-`?` and early-`return` paths. Encodes "pins released and bitmaps
/// synced at end of query" in one place.
///
/// The iterators in `cursor.rs` deliberately don't use this: they keep a
/// long-lived pin map across yields and prune the frontier explicitly.
pub(crate) struct Query<'a> {
  session: &'a Session,
  pub(crate) pinned: HashMap<u64, ChunkRef>,
}

impl<'a> Query<'a> {
  pub(crate) fn new(session: &'a Session) -> Self {
    Self {
      session,
      pinned: HashMap::new(),
    }
  }
}

impl Drop for Query<'_> {
  fn drop(&mut self) {
    // Clearing drops the ChunkRefs (unpin -> eviction), then we drain the
    // bitmaps the eviction freed - same order as the chunk engine elsewhere.
    self.pinned.clear();
    self.session.sync_bitmap_evictions();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::bitmap::ChunkBitmaps;
  use crate::cache::CacheOptions;
  use crate::simd::ScanCarry;
  use crate::source::InMemorySource;

  #[test]
  fn burst_doubling_schedule_caps_at_max_burst() {
    // The MAX_BURST policy doc (see above) lives in one comment but is
    // enforced by convention in three drivers (`run_locate`, `skip_value_at`,
    // `count::children`). Centralising the schedule in `doubling_burst` makes
    // it directly testable: yields are 1, 2, 4, 8, ..., MAX_BURST, MAX_BURST.
    let mut b = doubling_burst();
    let mut expected = 1u64;
    // The schedule reaches MAX_BURST in log2(MAX_BURST) + 1 = 9 iterations
    // (1, 2, 4, 8, 16, 32, 64, 128, 256). Iterate well past that to confirm
    // the cap is sticky.
    for _ in 0..16 {
      assert_eq!(b(0), expected, "expected {expected} at this step");
      expected = expected.saturating_mul(2).min(MAX_BURST);
    }
    // Sticky cap: further calls keep yielding MAX_BURST.
    assert_eq!(b(0), MAX_BURST);
    assert_eq!(b(0), MAX_BURST);
  }

  #[tokio::test]
  async fn bitmap_evict_drains_only_after_unpin() {
    // The bitmap-bytes accounting depends on a specific ordering: when a query
    // ends, pins release FIRST (unpin -> evict_to_caps queues offsets in
    // `evicted_since_drain`), THEN `sync_bitmap_evictions` drains the queue
    // into the bitmap store. Calling sync the other way around - before any
    // pin releases - is a no-op, and the bitmaps for chunks that are about to
    // be evicted persist until the next sync, breaking the bounded-bitmap
    // contract under churn. This test pins below the cap, syncs (must be a
    // no-op), unpins, syncs again (must reclaim).
    let src: Arc<dyn Source> = Arc::new(InMemorySource::new(vec![b' '; 256]));
    let session = Session::new(
      src,
      CacheOptions {
        chunk_size: 64,
        max_resident_bytes: 64,
      },
    )
    .unwrap();

    // Pin chunks 0 and 64. Cap is 1; eviction can't fire while both are pinned
    // (evict_to_caps finds no unpinned victim).
    let pin0 = session.cache.fetch(0, 1).await.unwrap().pop().unwrap();
    let pin64 = session.cache.fetch(64, 1).await.unwrap().pop().unwrap();

    // Build bitmaps for both, chaining carries.
    {
      let mut store = session.bitmaps.lock().unwrap();
      let bm0 = ChunkBitmaps::build_basic(&pin0.data, ScanCarry::default());
      let entry_after_0 = bm0.exit_carry();
      store.insert(0, bm0);
      let bm64 = ChunkBitmaps::build_basic(&pin64.data, entry_after_0);
      store.insert(64, bm64);
    }
    let bytes_with_both = session.cache.bitmap_bytes();
    assert!(bytes_with_both > 0, "bitmaps must contribute bytes");

    // Sync before any pin release: cache's eviction queue is empty, drain is
    // a no-op.
    session.sync_bitmap_evictions();
    assert_eq!(
      session.cache.bitmap_bytes(),
      bytes_with_both,
      "sync before pin release must be a no-op",
    );

    // Release pin 0. unpin -> evict_to_caps sees 1 unpinned chunk + 1 pinned >
    // cap=1, evicts the unpinned one, pushes its offset onto evicted_since_drain.
    drop(pin0);

    // The cache has dropped chunk 0's bytes; the bitmap store still has its
    // bitmaps - exactly the leak window that a wrong drain ordering would
    // make permanent.
    assert_eq!(
      session.cache.bitmap_bytes(),
      bytes_with_both,
      "before sync, bitmap bytes for the evicted chunk are not yet reclaimed",
    );

    // Sync now. Drain pulls chunk 0 off the queue and drops its bitmaps.
    session.sync_bitmap_evictions();
    assert!(
      session.cache.bitmap_bytes() < bytes_with_both,
      "sync after unpin must reclaim evicted chunk's bitmap bytes (was {bytes_with_both}, now {})",
      session.cache.bitmap_bytes(),
    );

    drop(pin64);
  }
}
