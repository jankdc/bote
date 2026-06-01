//! Async session: glues the sync walker to the chunk reader.
//!
//! A [`Session`] owns the immutable source metadata and a [`ChunkReader`]
//! (coalesced async byte fetch, no residency of its own). Per-query it runs the
//! [`Session::drive`] retry loop over a transient [`ByteWindow`]:
//!
//!   1. Build a [`Walker`] over the window's currently-resident chunks and run
//!      the caller-supplied sync step.
//!   2. On `ChunkMiss(off)`, read a burst of chunks async, insert them into the
//!      window, drop everything below the step's retention floor, and retry.
//!   3. On success, return.
//!
//! Nothing persists across queries: the window is owned by the query (one-shot
//! [`Query`]) or the iterator (`StreamCore`) and is dropped or pruned to the
//! scan frontier as the walk advances, so resident source memory stays bounded
//! by the burst window regardless of document size. Bitmaps aren't stored at
//! all - the walker builds them per block on the fly.

use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::chunks::{ByteWindow, ChunkMiss, ChunkReader, ReaderError};
use crate::index_cache::{Frontier, IndexCache};
use crate::path::Segment;
use crate::resolve::{
  self, ChildEntry, Children, ContainerKind, ResolveState, ResumeHint, ScanHarvest, ValueLocation,
};
use crate::select::SelectError;
use crate::source::Source;
use crate::walker::{self, SkipState, TraverseError, Walker};

use std::sync::{Arc, Mutex};

/// Children-budget default when the facade doesn't pass `indexCacheEntries`.
pub(crate) const DEFAULT_INDEX_CACHE_ENTRIES: usize = 1024;

#[derive(Debug, Error)]
pub enum SessionError {
  #[error("traversal error: {0}")]
  Traverse(#[from] TraverseError),
  #[error(transparent)]
  Reader(#[from] ReaderError),
  #[error("failed to parse JSON value: {0}")]
  Json(#[from] serde_json::Error),
  #[error(transparent)]
  Select(#[from] SelectError),
}

pub struct Session {
  pub source_size: u64,
  pub chunk_size: u64,
  pub reader: Arc<ChunkReader>,
  /// Structural-index cache, shared across every cursor over this source.
  /// `&self` methods mutate it through interior mutability; the lock is held
  /// only for the synchronous lookup/write-back, never across an `.await`.
  cache: Mutex<IndexCache>,
  /// Mirror of `budget > 0`, so the hot gate checks never take the lock.
  cache_enabled: bool,
}

/// Cap on the adaptive doubling burst. The resolver restarts from the anchor on
/// every chunk fault, so unbounded restarts give O(N²) traversal for an
/// N-chunk query. Each `ChunkMiss` doubles the burst (1, 2, 4, ..., capped
/// here): short queries pay no over-fetch, long queries converge to a near
/// single pass. This cap is also the dominant bound on resident source memory:
/// the window holds at most ~one burst of chunks between prunes.
pub(crate) const MAX_BURST: u64 = 256;

/// Adaptive doubling burst used by every driver that doesn't know the value's
/// extent up front (`run_locate`, `skip_value_at`, `count::children`). Each call
/// yields the current burst (in chunks) and doubles it for the next, capped at
/// [`MAX_BURST`]. Move-bound and stateful; use one per `Session::drive`.
pub(crate) fn doubling_burst() -> impl FnMut(u64) -> u64 {
  let mut n = 1u64;
  move |_off| {
    let cur = n;
    n = n.saturating_mul(2).min(MAX_BURST);
    cur
  }
}

impl Session {
  pub fn new(
    source: Arc<dyn Source>,
    chunk_size: usize,
    index_cache_budget: usize,
  ) -> Result<Arc<Self>, SessionError> {
    let source_size = source.size();
    let reader = ChunkReader::new(source, chunk_size)?;
    Ok(Arc::new(Self {
      source_size,
      chunk_size: reader.chunk_size(),
      reader,
      cache: Mutex::new(IndexCache::new(index_cache_budget)),
      cache_enabled: index_cache_budget > 0,
    }))
  }

  /// Cached child count for `(anchor, path)`, when a prior `count`/`iter`/`walk`
  /// learned it - lets a repeat `count` skip the scan entirely.
  pub(crate) fn cached_child_count(&self, anchor: u64, path: &[Segment]) -> Option<u64> {
    if !self.cache_enabled {
      return None;
    }
    self.cache.lock().unwrap().get(anchor, path)?.child_count()
  }

  pub(crate) fn record_child_count(
    &self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    count: u64,
  ) {
    if !self.cache_enabled {
      return;
    }
    self
      .cache
      .lock()
      .unwrap()
      .record_child_count(anchor, path, kind, value_start, count);
  }

  /// Record the close offset (`}`/`]` + 1) of the container at `(anchor, path)`.
  pub(crate) fn record_close(
    &self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    close: u64,
  ) {
    if !self.cache_enabled {
      return;
    }
    self
      .cache
      .lock()
      .unwrap()
      .record_close(anchor, path, kind, value_start, close);
  }

  /// Record an array frontier landmark `(index, offset)` at `(anchor, path)` -
  /// used by `iter`/`walk` early termination so a later random index resumes
  /// near the stop point.
  pub(crate) fn record_array_frontier(
    &self,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    index: usize,
    offset: u64,
  ) {
    if !self.cache_enabled {
      return;
    }
    self
      .cache
      .lock()
      .unwrap()
      .record_array_scan(anchor, path, value_start, Some((index, offset)));
  }

  pub(crate) fn new_window(&self) -> ByteWindow {
    ByteWindow::new(self.chunk_size, self.source_size)
  }

  pub async fn locate_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
  ) -> Result<Option<u64>, SessionError> {
    let mut window = self.new_window();
    self.run_locate(path, anchor_start, &mut window).await
  }

  pub async fn has_at(&self, path: &[Segment], anchor_start: u64) -> Result<bool, SessionError> {
    let mut window = self.new_window();
    Ok(
      self
        .run_resolve(path, anchor_start, &mut window)
        .await?
        .is_some(),
    )
  }

  pub async fn get_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
  ) -> Result<Option<serde_json::Value>, SessionError> {
    let mut window = self.new_window();
    let Some(loc) = self.run_resolve(path, anchor_start, &mut window).await? else {
      return Ok(None);
    };
    let bytes = self.read_range(loc.start, loc.end, &mut window).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
  }

  /// Prune the iterator window to the scan frontier: keep just the chunk
  /// covering `next_offset` so the next yield's first read is hot, dropping
  /// everything behind it. Clears entirely once iteration walks off the end.
  /// Bounds the iterator's resident chunks to ~1 between yields.
  pub(crate) fn prune_window(&self, window: &mut ByteWindow, next_offset: u64) {
    if next_offset >= self.source_size {
      window.clear();
    } else {
      window.drop_below(next_offset);
    }
  }

  /// Open a child iterator over the container starting at `value_start`.
  /// Returns `Ok(None)` if the value isn't an object or array.
  pub async fn enter_container(
    &self,
    value_start: u64,
    window: &mut ByteWindow,
  ) -> Result<Option<Children>, SessionError> {
    let floor = AtomicU64::new(value_start);
    self
      .drive(
        window,
        &floor,
        |_| 1,
        |walker| resolve::enter_container(walker, value_start),
      )
      .await
  }

  /// Advance `cw` to the next child entry.
  pub async fn next_child(
    &self,
    cw: &mut Children,
    window: &mut ByteWindow,
  ) -> Result<Option<ChildEntry>, SessionError> {
    // One element typically stays inside one chunk; burst=1 is fine, and the
    // per-call invocation means no quadratic-restart risk to amortize.
    let floor = AtomicU64::new(cw.next_offset);
    self
      .drive(
        window,
        &floor,
        |_| 1,
        |walker| resolve::next_child(walker, cw),
      )
      .await
  }

  /// Materialize the JSON value at `loc` by reading and parsing its bytes.
  pub async fn materialize(
    &self,
    loc: ValueLocation,
    window: &mut ByteWindow,
  ) -> Result<serde_json::Value, SessionError> {
    let bytes = self.read_range(loc.start, loc.end, window).await?;
    Ok(serde_json::from_slice(&bytes)?)
  }

  /// Resolve `path` from `anchor_start`, returning only the resolved value's
  /// start offset (no extent walk).
  ///
  /// Memoization seam: `run_locate` (and [`run_resolve`](Self::run_resolve) /
  /// [`locate_at`](Self::locate_at), which wrap it) is the single point every
  /// path resolution flows through - `get`/`has`/`count`/`iter`/`walk`/`select`
  /// all route here. The structural-index cache (`index_cache`) lives at exactly
  /// this boundary: a chain of cached container hops starts the scan as deep as
  /// possible (an all-hit returns the offset without faulting a single chunk),
  /// the first uncached level resumes from the deepest landmark, and the scan's
  /// harvested child offsets are written back. Keep these three the only
  /// resolution entry points so the cache has one place to live.
  pub(crate) async fn run_locate(
    &self,
    path: &[Segment],
    anchor_start: u64,
    window: &mut ByteWindow,
  ) -> Result<Option<u64>, SessionError> {
    // 1. Walk cached container hops to the deepest landmark (lock held only for
    //    this synchronous lookup, never across the drive below).
    let (start, seg, hint) = if self.cache_enabled {
      let mut cache = self.cache.lock().unwrap();
      chain_hops(&mut cache, anchor_start, path)
    } else {
      (anchor_start, 0, None)
    };
    // 2. Drive the resolver from the seed. ResolveState persists across
    //    `ChunkMiss` retries; the floor follows the resolver's committed
    //    iteration offset so chunks behind it are dropped while the key being
    //    read (which `read_range`s behind the scan frontier) stays resident.
    let mut state = ResolveState::resume(start, seg, hint, self.cache_enabled);
    let floor = AtomicU64::new(start);
    let result = self
      .drive(window, &floor, doubling_burst(), |walker| {
        let r = resolve::resolve_step(walker, path, &mut state);
        floor.store(state.floor(), Ordering::Relaxed);
        r
      })
      .await?;
    // 3. Write the harvested child offsets back.
    if let Some(harvest) = state.take_harvest() {
      let mut cache = self.cache.lock().unwrap();
      write_back(&mut cache, anchor_start, path, &harvest);
    }
    Ok(result)
  }

  /// Resolve `path` to a full `[start, end)` byte range.
  pub(crate) async fn run_resolve(
    &self,
    path: &[Segment],
    anchor_start: u64,
    window: &mut ByteWindow,
  ) -> Result<Option<ValueLocation>, SessionError> {
    let Some(start) = self.run_locate(path, anchor_start, window).await? else {
      return Ok(None);
    };
    if !self.cache_enabled {
      let end = self.skip_value_at(start, window).await?;
      return Ok(Some(ValueLocation { start, end }));
    }
    // A cached close skips the extent walk entirely for a large container.
    let cached = {
      let cache = self.cache.lock().unwrap();
      cache.get(anchor_start, path).and_then(|n| n.location())
    };
    if let Some(loc) = cached {
      return Ok(Some(loc));
    }
    // Peek the kind first (loads the start chunk, which `skip_value_at` then
    // reuses) so only containers - not scalars - get a cache node.
    let kind = self.peek_container_kind(start, window).await?;
    let end = self.skip_value_at(start, window).await?;
    if let Some(kind) = kind {
      self.record_close(anchor_start, path, kind, start, end);
    }
    Ok(Some(ValueLocation { start, end }))
  }

  /// The container kind at `from` (whitespace already implicit), or `None` if
  /// the value there is a scalar. One cheap byte read, usually hot.
  async fn peek_container_kind(
    &self,
    from: u64,
    window: &mut ByteWindow,
  ) -> Result<Option<ContainerKind>, SessionError> {
    let floor = AtomicU64::new(from);
    self
      .drive(
        window,
        &floor,
        |_| 1,
        |walker| {
          let s = walker.skip_whitespace(from)?;
          match walker.byte_at(s)? {
            Some(b'{') => Ok(Some(ContainerKind::Object)),
            Some(b'[') => Ok(Some(ContainerKind::Array)),
            Some(_) => Ok(None),
            None => Err(TraverseError::UnexpectedEof(s)),
          }
        },
      )
      .await
  }

  pub(crate) async fn skip_value_at(
    &self,
    from: u64,
    window: &mut ByteWindow,
  ) -> Result<u64, SessionError> {
    // Resumable: the SkipState commits at block boundaries, so the floor tracks
    // the skip position and the window stays bounded even for a large value.
    let mut state = SkipState::start(from);
    let floor = AtomicU64::new(from);
    self
      .drive(window, &floor, doubling_burst(), |walker| {
        let r = walker::skip_value_step(walker, &mut state);
        floor.store(state.floor(), Ordering::Relaxed);
        r
      })
      .await
  }

  pub(crate) async fn read_range(
    &self,
    from: u64,
    to: u64,
    window: &mut ByteWindow,
  ) -> Result<Vec<u8>, SessionError> {
    let chunk_size = self.chunk_size;
    // The full byte range is known, so fetch the rest in one shot on a miss.
    // `read_range` isn't resumable (it restarts from `from`), so the floor is
    // `from`: the value's chunks must all be resident together to copy them out.
    let floor = AtomicU64::new(from);
    self
      .drive(
        window,
        &floor,
        move |off| to.saturating_sub(off).div_ceil(chunk_size).max(1),
        |walker| walker.read_range(from, to).map_err(TraverseError::from),
      )
      .await
  }

  /// The shared retry loop: build a fresh [`Walker`] over the window, run a sync
  /// `step`, and on `ChunkMiss(off)` read a burst of chunks (sized by
  /// `burst_for`) into the window, drop everything below `floor`, and retry.
  ///
  /// `floor` is the lowest offset the step might still read; the step updates it
  /// as it commits forward progress. Dropping below it keeps the window bounded
  /// while never evicting a chunk a behind-frontier `read_range` (object keys)
  /// still needs - the floor sits at or below the current iteration's start.
  pub(crate) async fn drive<T>(
    &self,
    window: &mut ByteWindow,
    floor: &AtomicU64,
    mut burst_for: impl FnMut(u64) -> u64,
    mut step: impl FnMut(&mut Walker) -> Result<T, TraverseError>,
  ) -> Result<T, SessionError> {
    loop {
      let outcome = {
        let mut walker = Walker::new(window);
        step(&mut walker)
      };
      match outcome {
        Ok(v) => return Ok(v),
        Err(TraverseError::Pending(ChunkMiss(off))) => {
          let n = burst_for(off);
          for (o, b) in self.reader.read_chunks(off, n).await? {
            window.insert(o, b);
          }
          window.drop_below(floor.load(Ordering::Relaxed));
        }
        Err(e) => return Err(e.into()),
      }
    }
  }
}

/// Walk cached container hops from `anchor` along `path`, returning the deepest
/// `(start, segment_idx, hint)` to seed the resolver with. Each tabled object
/// member is an O(1) hop; the first uncached level returns the container's start
/// plus a frontier hint (object high-water, or array landmark at/under the
/// target) so the resolver resumes near the target instead of at the open.
fn chain_hops(
  cache: &mut IndexCache,
  anchor: u64,
  path: &[Segment],
) -> (u64, usize, Option<ResumeHint>) {
  let mut start = anchor;
  let mut seg = 0;
  while seg < path.len() {
    let prefix = &path[..seg];
    // Read the node's fields into owned values (ending the borrow) before the
    // recency bump.
    let Some((kind, frontier, member_vs)) = cache.get(anchor, prefix).map(|n| {
      let mvs = match &path[seg] {
        Segment::Member(name) => n.member(name),
        Segment::Element(_) => None,
      };
      (n.kind(), n.frontier(), mvs)
    }) else {
      return (start, seg, None);
    };
    cache.touch(anchor, prefix);
    match (&path[seg], kind) {
      (Segment::Member(_), ContainerKind::Object) => {
        if let Some(vs) = member_vs {
          start = vs;
          seg += 1;
          continue; // O(1) hop into a tabled member
        }
        let hint = match frontier {
          Frontier::Object { offset } => Some(ResumeHint::ObjectFrontier { offset }),
          Frontier::Array { .. } => None,
        };
        return (start, seg, hint);
      }
      (Segment::Element(idx), ContainerKind::Array) => {
        let hint = match frontier {
          Frontier::Array { index, offset } if index <= *idx => {
            Some(ResumeHint::ArrayFrontier { index, offset })
          }
          _ => None, // frontier past the target: scan from the open
        };
        return (start, seg, hint);
      }
      // Kind/segment mismatch: resolve will return None; seed at the container
      // start with no hint.
      _ => return (start, seg, None),
    }
  }
  (start, seg, None) // whole path hopped: `start` is the resolved value
}

/// Drain a scan's harvested child offsets into the cache, one node per entered
/// container.
fn write_back(cache: &mut IndexCache, anchor: u64, path: &[Segment], harvest: &ScanHarvest) {
  for cs in &harvest.containers {
    let prefix = &path[..cs.seg];
    match cs.kind {
      ContainerKind::Object => cache.record_object_scan(
        anchor,
        prefix,
        cs.value_start,
        &cs.members,
        cs.object_terminal,
      ),
      ContainerKind::Array => {
        cache.record_array_scan(anchor, prefix, cs.value_start, cs.array_frontier)
      }
    }
  }
}

/// RAII scope for a one-shot query (`locate_at` / `get_at` / `count_at`): owns
/// the transient [`ByteWindow`] so its chunks are released when the query
/// returns, including on early-`?` and early-`return` paths.
///
/// The iterators in `cursor.rs` don't use this: they keep a long-lived window
/// across yields and prune the frontier explicitly via [`Session::prune_window`].
pub(crate) struct Query {
  pub(crate) window: ByteWindow,
}

impl Query {
  pub(crate) fn new(session: &Session) -> Self {
    Self {
      window: session.new_window(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::source::InMemorySource;

  #[test]
  fn burst_doubling_schedule_caps_at_max_burst() {
    let mut b = doubling_burst();
    let mut expected = 1u64;
    for _ in 0..16 {
      assert_eq!(b(0), expected, "expected {expected} at this step");
      expected = expected.saturating_mul(2).min(MAX_BURST);
    }
    assert_eq!(b(0), MAX_BURST);
    assert_eq!(b(0), MAX_BURST);
  }

  /// A full linear scan of a many-chunk document keeps the byte window bounded
  /// by the burst, never by document size: the bounded-memory contract, now an
  /// internal invariant (there is no `cacheStats` to assert it through).
  #[tokio::test]
  async fn window_stays_bounded_under_full_scan() {
    // 4 MiB doc of a flat array of small objects; 4 KiB chunks => ~1000 chunks.
    let mut doc = String::from("{\"items\":[");
    let mut i = 0;
    while doc.len() < 4 * 1024 * 1024 {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&format!("{{\"n\":{i}}}"));
      i += 1;
    }
    doc.push_str("]}");
    let source: Arc<dyn Source> = Arc::new(InMemorySource::new(doc.into_bytes()));
    let session = Session::new(source, 4096, DEFAULT_INDEX_CACHE_ENTRIES).unwrap();

    let start = session
      .locate_at(&[Segment::Member("items".into())], 0)
      .await
      .unwrap()
      .expect("items resolves");

    let mut window = session.new_window();
    let mut cw = session
      .enter_container(start, &mut window)
      .await
      .unwrap()
      .expect("array");
    let bound = (MAX_BURST as usize) + 4; // one burst + small slack
    let mut seen = 0;
    while let Some(_child) = session.next_child(&mut cw, &mut window).await.unwrap() {
      seen += 1;
      session.prune_window(&mut window, cw.next_offset);
      assert!(
        window.len() <= bound,
        "window held {} chunks at element {seen} (bound {bound})",
        window.len()
      );
    }
    assert!(seen > 1000, "scanned {seen} elements");
  }

  use std::sync::atomic::AtomicUsize;

  use async_trait::async_trait;
  use bytes::Bytes;

  use crate::source::SourceError;

  /// Wraps an [`InMemorySource`] and counts its `read` calls, so the cache's
  /// effect on chunk faulting is directly observable.
  struct CountingSource {
    inner: InMemorySource,
    reads: Arc<AtomicUsize>,
  }

  #[async_trait]
  impl Source for CountingSource {
    fn size(&self) -> u64 {
      self.inner.size()
    }
    async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      self.inner.read(offset, length).await
    }
  }

  fn counting_session(
    doc: String,
    chunk: usize,
    budget: usize,
  ) -> (Arc<Session>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn Source> = Arc::new(CountingSource {
      inner: InMemorySource::new(doc.into_bytes()),
      reads: reads.clone(),
    });
    (Session::new(source, chunk, budget).unwrap(), reads)
  }

  fn member(name: &str) -> Segment {
    Segment::Member(name.into())
  }

  /// `{"a":{"b":{"f0":0,...,"f199":199,"c":1,"d":2}}}` - c and d are the last two
  /// members of a large object, so a cold scan of `b` is expensive.
  fn deep_object_doc() -> String {
    let mut b = String::from("{");
    for i in 0..200 {
      b.push_str(&format!("\"f{i}\":{i},"));
    }
    b.push_str("\"c\":1,\"d\":2}");
    format!("{{\"a\":{{\"b\":{b}}}}}")
  }

  #[tokio::test]
  async fn object_sibling_access_faults_fewer_chunks() {
    let path_c = [member("a"), member("b"), member("c")];
    let path_d = [member("a"), member("b"), member("d")];

    // Warm: resolve c (populates the chain + b's member table), then d resumes
    // from c's frontier - a one-member scan.
    let (warm, warm_reads) = counting_session(deep_object_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let mut w = warm.new_window();
    warm.run_locate(&path_c, 0, &mut w).await.unwrap().unwrap();
    warm_reads.store(0, Ordering::Relaxed);
    let mut w2 = warm.new_window();
    assert!(warm
      .run_locate(&path_d, 0, &mut w2)
      .await
      .unwrap()
      .is_some());
    let warm_n = warm_reads.load(Ordering::Relaxed);

    // Cold: d on a fresh session scans root, a, and all of b from their opens.
    let (cold, cold_reads) = counting_session(deep_object_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let mut c = cold.new_window();
    assert!(cold.run_locate(&path_d, 0, &mut c).await.unwrap().is_some());
    let cold_n = cold_reads.load(Ordering::Relaxed);

    assert!(
      warm_n < cold_n,
      "warm sibling access ({warm_n} reads) should fault fewer chunks than cold ({cold_n})"
    );
  }

  /// `{"arr":[{"v":"<padding>"}, ... 100 elements ...]}` - elements are large
  /// enough that the comma popcount to a deep index faults several chunks.
  fn big_array_doc() -> String {
    let pad = "x".repeat(40);
    let mut s = String::from("{\"arr\":[");
    for i in 0..100 {
      if i > 0 {
        s.push(',');
      }
      s.push_str(&format!("{{\"v\":\"{pad}\"}}"));
    }
    s.push_str("]}");
    s
  }

  #[tokio::test]
  async fn array_frontier_resume_faults_fewer_chunks() {
    let at = |i: usize| [member("arr"), Segment::Element(i)];

    let (warm, warm_reads) = counting_session(big_array_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let mut w = warm.new_window();
    warm.run_locate(&at(40), 0, &mut w).await.unwrap().unwrap();
    warm_reads.store(0, Ordering::Relaxed);
    let mut w2 = warm.new_window();
    assert!(warm
      .run_locate(&at(50), 0, &mut w2)
      .await
      .unwrap()
      .is_some());
    let warm_n = warm_reads.load(Ordering::Relaxed);

    let (cold, cold_reads) = counting_session(big_array_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let mut c = cold.new_window();
    assert!(cold.run_locate(&at(50), 0, &mut c).await.unwrap().is_some());
    let cold_n = cold_reads.load(Ordering::Relaxed);

    assert!(
      warm_n < cold_n,
      "warm index resume ({warm_n} reads) should fault fewer chunks than cold ({cold_n})"
    );
  }

  #[tokio::test]
  async fn repeat_count_issues_no_reads() {
    let (s, reads) = counting_session(big_array_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let path = [member("arr")];
    let first = crate::count::at(&s, &path, 0).await.unwrap();
    assert_eq!(first, 100);
    assert!(reads.load(Ordering::Relaxed) > 0, "cold count must read");
    reads.store(0, Ordering::Relaxed);
    let second = crate::count::at(&s, &path, 0).await.unwrap();
    assert_eq!(second, 100);
    assert_eq!(
      reads.load(Ordering::Relaxed),
      0,
      "a repeat count must be served from the cache with no reads"
    );
  }

  #[tokio::test]
  async fn disabled_cache_still_resolves() {
    // budget 0 => no caching; results must be identical and reads still happen.
    let (s, _reads) = counting_session(deep_object_doc(), 256, 0);
    assert!(!s.cache_enabled);
    let path = [member("a"), member("b"), member("d")];
    let mut w = s.new_window();
    assert!(s.run_locate(&path, 0, &mut w).await.unwrap().is_some());
    // Repeat count is not short-circuited when disabled.
    let n = crate::count::at(&s, &[member("a"), member("b")], 0)
      .await
      .unwrap();
    assert_eq!(n, 202);
    // Nothing was cached.
    assert!(s.cache.lock().unwrap().get(0, &path).is_none());
  }

  /// Inspect the cache directly (the test module is a child of `session`, so it
  /// can read the private `cache` field). Pins the *mechanism* - which nodes and
  /// members a scan tables - complementing the read-count tests that prove the
  /// resulting benefit.
  #[tokio::test]
  async fn cache_state_records_walked_containers() {
    // {"a":{"b":{"c":1,"d":2,"e":3}}}: resolving c then d tables both on the
    // [a,b] node; e is never queried, so it stays untabled.
    let doc = r#"{"a":{"b":{"c":1,"d":2,"e":3}}}"#.to_string();
    let (s, _reads) = counting_session(doc, 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let ab = [member("a"), member("b")];

    // Cold: nothing cached yet.
    assert!(s.cache.lock().unwrap().get(0, &ab).is_none());

    let mut w = s.new_window();
    s.run_locate(&[member("a"), member("b"), member("c")], 0, &mut w)
      .await
      .unwrap()
      .expect("c resolves");
    {
      let cache = s.cache.lock().unwrap();
      // The whole ancestor chain is tabled: root -> a, [a] -> b, [a,b] -> c.
      assert!(cache.get(0, &[]).unwrap().member("a").is_some());
      assert!(cache.get(0, &[member("a")]).unwrap().member("b").is_some());
      let n = cache.get(0, &ab).expect("the [a,b] container is cached");
      assert!(n.member("c").is_some(), "c was just resolved");
      assert!(n.member("d").is_none(), "d not yet seen");
      assert!(n.member("e").is_none());
    }

    let mut w2 = s.new_window();
    s.run_locate(&[member("a"), member("b"), member("d")], 0, &mut w2)
      .await
      .unwrap()
      .expect("d resolves");
    {
      let cache = s.cache.lock().unwrap();
      let n = cache.get(0, &ab).unwrap();
      assert!(n.member("c").is_some(), "c still tabled");
      assert!(n.member("d").is_some(), "d now tabled (resumed past c)");
      assert!(n.member("e").is_none(), "e never queried, never tabled");
    }
  }

  #[tokio::test]
  async fn cache_state_array_frontier_advances() {
    let (s, _reads) = counting_session(big_array_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let arr = [member("arr")];

    let mut w = s.new_window();
    s.run_locate(&[member("arr"), Segment::Element(40)], 0, &mut w)
      .await
      .unwrap()
      .expect("element 40 resolves");
    let frontier = s
      .cache
      .lock()
      .unwrap()
      .get(0, &arr)
      .expect("arr node")
      .frontier();
    match frontier {
      Frontier::Array { index, .. } => {
        assert_eq!(index, 40, "frontier landmark at the resolved index")
      }
      other => panic!("expected an array frontier, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn cache_state_count_records_child_count() {
    let (s, _reads) = counting_session(big_array_doc(), 256, DEFAULT_INDEX_CACHE_ENTRIES);
    let arr = [member("arr")];
    assert_eq!(crate::count::at(&s, &arr, 0).await.unwrap(), 100);
    assert_eq!(
      s.cache.lock().unwrap().get(0, &arr).unwrap().child_count(),
      Some(100),
      "count must record the child count on the node"
    );
  }
}
