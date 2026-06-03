//! Async session: glues the sync walker to the chunk reader.
//!
//! A [`Session`] owns the immutable source metadata and a [`ChunkReader`].
//! Per-query it runs the [`Session::drive`] retry loop over a transient
//! [`ChunkWindow`]:
//!
//!   1. Build a [`Walker`] over the window's resident chunks and run the
//!      caller-supplied sync step.
//!   2. On `ChunkMiss(off)`, read a burst of chunks async, insert them into the
//!      window, drop everything below the step's retention bound, and retry.
//!   3. On success, return.
//!
//! The window is owned by the query ([`Query`]) or the iterator and is dropped
//! or pruned to the scan position as the walk advances, so resident source
//! memory stays bounded by the burst window regardless of document size.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::cache::StructuralIndex;
use crate::chunks::{ChunkMiss, ChunkReader, ChunkWindow, ReaderError};
use crate::path::Segment;
use crate::resolve::{
  self, ChildEntry, ContainerCursor, ContainerKind, ResolveState, ValueLocation,
};
use crate::select::SelectError;
use crate::source::ByteStream;
use crate::walker::{self, SkipState, TraverseError, Walker};

/// Default `indexCacheEntries`: one slot per node + one per tabled member. `0` disables.
pub(crate) const DEFAULT_INDEX_CACHE_ENTRIES: usize = 1024;
/// Default `objectMemberCap`: unbounded.
pub(crate) const DEFAULT_OBJECT_MEMBER_CAP: usize = usize::MAX;
/// Default `arrayIndexInterval`: array-member element stride.
pub(crate) const DEFAULT_ARRAY_INDEX_INTERVAL: usize = 16;

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
  pub chunk_bytes: u64,
  pub reader: Arc<ChunkReader>,
  /// Shared across every cursor over this source. The lock is held only for the
  /// synchronous lookup/write-back, never across an `.await`.
  cache: Mutex<StructuralIndex>,
  /// Mirror of the cache config so the hot path reads it without the lock.
  cache_enabled: bool,
  /// Object members are recorded only when `> 0`.
  object_member_cap: usize,
  array_interval: usize,
}

/// Cap on the adaptive doubling burst. The resolver restarts from the anchor on
/// every chunk fault, so unbounded restarts give O(N²) traversal for an N-chunk
/// query; doubling (1, 2, 4, ..., capped here) lets short queries avoid
/// over-fetch and long ones converge to ~one pass. Also the dominant bound on
/// resident source memory: the window holds at most ~one burst between prunes.
pub(crate) const MAX_BURST: u64 = 256;

/// Adaptive doubling burst for drivers that don't know the value's extent up
/// front (`run_locate`, `skip_value_at`, `count::children`). Stateful; use one
/// per `Session::drive`.
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
    source: Arc<dyn ByteStream>,
    chunk_bytes: usize,
    index_cache_budget: usize,
    object_member_cap: usize,
    array_interval: usize,
  ) -> Result<Arc<Self>, SessionError> {
    let source_size = source.size();
    let reader = ChunkReader::new(source, chunk_bytes)?;
    let cache = StructuralIndex::new(index_cache_budget, object_member_cap, array_interval);
    let cache_enabled = cache.is_enabled();
    Ok(Arc::new(Self {
      source_size,
      chunk_bytes: reader.chunk_bytes(),
      reader,
      cache: Mutex::new(cache),
      cache_enabled,
      object_member_cap,
      array_interval,
    }))
  }

  pub(crate) fn new_window(&self) -> ChunkWindow {
    ChunkWindow::new(self.chunk_bytes, self.source_size)
  }

  /// Prune the iterator window to the scan position: keep just the chunk
  /// covering `next_offset` so the next yield's first read is hot, dropping the
  /// rest (clearing once iteration walks off the end). Bounds resident chunks to
  /// ~1 between yields.
  pub(crate) fn prune_window(&self, window: &mut ChunkWindow, next_offset: u64) {
    if next_offset >= self.source_size {
      window.clear();
    } else {
      window.drop_below(next_offset);
    }
  }
}

/// One-shot public queries. Each opens a transient window, resolves, and drops
/// the window on return.
impl Session {
  pub async fn locate_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
    base_depth: u32,
  ) -> Result<Option<u64>, SessionError> {
    let mut window = self.new_window();
    self
      .run_locate(path, anchor_start, base_depth, &mut window)
      .await
  }

  pub async fn has_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
    base_depth: u32,
  ) -> Result<bool, SessionError> {
    let mut window = self.new_window();
    Ok(
      self
        .run_resolve(path, anchor_start, base_depth, &mut window)
        .await?
        .is_some(),
    )
  }

  pub async fn get_at(
    &self,
    path: &[Segment],
    anchor_start: u64,
    base_depth: u32,
  ) -> Result<Option<serde_json::Value>, SessionError> {
    let mut window = self.new_window();
    let Some(loc) = self
      .run_resolve(path, anchor_start, base_depth, &mut window)
      .await?
    else {
      return Ok(None);
    };
    let bytes = self.read_range(loc.start, loc.end, &mut window).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
  }
}

/// Cursor/iterator support, called from `cursor.rs`. These take a caller-owned,
/// long-lived window that `cursor.rs` prunes to the scan frontier between yields.
impl Session {
  /// Open a child iterator over the container at `value_start`, or `Ok(None)` if
  /// the value isn't an object or array.
  pub async fn enter_container(
    &self,
    value_start: u64,
    window: &mut ChunkWindow,
  ) -> Result<Option<ContainerCursor>, SessionError> {
    let min_reachable = AtomicU64::new(value_start);
    self
      .drive(
        window,
        &min_reachable,
        |_| 1,
        |walker| resolve::enter_container(walker, value_start),
      )
      .await
  }

  /// Advance `cursor` to the next child entry.
  pub async fn next_child(
    &self,
    cursor: &mut ContainerCursor,
    window: &mut ChunkWindow,
  ) -> Result<Option<ChildEntry>, SessionError> {
    // One element typically fits one chunk, and per-call invocation means no
    // quadratic-restart risk to amortize, so burst=1.
    let min_reachable = AtomicU64::new(cursor.next_offset);
    self
      .drive(
        window,
        &min_reachable,
        |_| 1,
        |walker| resolve::next_child(walker, cursor),
      )
      .await
  }

  /// Materialize the JSON value at `loc` by reading and parsing its bytes.
  pub async fn materialize(
    &self,
    loc: ValueLocation,
    window: &mut ChunkWindow,
  ) -> Result<serde_json::Value, SessionError> {
    let bytes = self.read_range(loc.start, loc.end, window).await?;
    Ok(serde_json::from_slice(&bytes)?)
  }
}

/// Path resolution - the structural-index memoization seam.
impl Session {
  /// Resolve `path` from `anchor_start`, returning only the resolved value's
  /// start offset (no extent walk).
  ///
  /// Memoization seam: every path resolution flows through here (or its wrappers
  /// `run_resolve`/`locate_at`) - `get`/`has`/`count`/`iter`/`walk`/`select` all
  /// route in. So the structural-index cache lives here: cached container hops
  /// start the scan as deep as possible (an all-hit returns the offset faulting
  /// no chunks), the first uncached level resumes from the deepest array member, and
  /// the scan's child offsets are written back. Keep these three the only
  /// resolution entry points so the cache has one place to live.
  pub(crate) async fn run_locate(
    &self,
    path: &[Segment],
    anchor_start: u64,
    base_depth: u32,
    window: &mut ChunkWindow,
  ) -> Result<Option<u64>, SessionError> {
    // Lock held only for this lookup, never across the drive below.
    let (start, seg, hint) = if self.cache_enabled {
      self.cache.lock().unwrap().chain_hops(anchor_start, path)
    } else {
      (anchor_start, 0, None)
    };
    // min_reachable follows the resolver's committed offset so chunks behind it
    // drop, yet sits below the scan position so a key `read_range`d there stays
    // resident.
    let mut state = ResolveState::resume(
      start,
      seg,
      hint,
      self.object_member_cap > 0,
      self.array_interval,
    );
    let min_reachable = AtomicU64::new(start);
    let result = self
      .drive(window, &min_reachable, doubling_burst(), |walker| {
        let r = resolve::resolve_step(walker, path, &mut state);
        min_reachable.store(state.min_reachable(), Ordering::Relaxed);
        r
      })
      .await?;
    if let Some(scan_record) = state.take_scan_record() {
      self
        .cache
        .lock()
        .unwrap()
        .apply_scan_record(base_depth, anchor_start, path, &scan_record);
    }
    Ok(result)
  }

  /// Resolve `path` to a full `[start, end)` byte range.
  pub(crate) async fn run_resolve(
    &self,
    path: &[Segment],
    anchor_start: u64,
    base_depth: u32,
    window: &mut ChunkWindow,
  ) -> Result<Option<ValueLocation>, SessionError> {
    let Some(start) = self
      .run_locate(path, anchor_start, base_depth, window)
      .await?
    else {
      return Ok(None);
    };
    if !self.cache_enabled {
      let end = self.skip_value_at(start, window).await?;
      return Ok(Some(ValueLocation { start, end }));
    }
    // A cached close skips the extent walk for a large container.
    let cached = self
      .with_cache(|c| c.get(anchor_start, path).and_then(|n| n.location()))
      .flatten();
    if let Some(loc) = cached {
      return Ok(Some(loc));
    }
    // Peek the kind first so only containers - not scalars - get a cache node;
    // it loads the start chunk that `skip_value_at` then reuses.
    let kind = self.peek_container_kind(start, window).await?;
    let end = self.skip_value_at(start, window).await?;
    if let Some(kind) = kind {
      self.store_close(base_depth, anchor_start, path, kind, start, end);
    }
    Ok(Some(ValueLocation { start, end }))
  }

  /// The container kind at `from`, or `None` if the value there is a scalar.
  /// One cheap byte read, usually hot.
  async fn peek_container_kind(
    &self,
    from: u64,
    window: &mut ChunkWindow,
  ) -> Result<Option<ContainerKind>, SessionError> {
    let min_reachable = AtomicU64::new(from);
    self
      .drive(
        window,
        &min_reachable,
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
    window: &mut ChunkWindow,
  ) -> Result<u64, SessionError> {
    // SkipState commits at block boundaries, so min_reachable tracks the skip
    // position and the window stays bounded even for a large value.
    let mut state = SkipState::start(from);
    let min_reachable = AtomicU64::new(from);
    self
      .drive(window, &min_reachable, doubling_burst(), |walker| {
        let r = walker::skip_value_step(walker, &mut state);
        min_reachable.store(state.min_reachable(), Ordering::Relaxed);
        r
      })
      .await
  }

  pub(crate) async fn read_range(
    &self,
    from: u64,
    to: u64,
    window: &mut ChunkWindow,
  ) -> Result<Vec<u8>, SessionError> {
    let chunk_bytes = self.chunk_bytes;
    // The range is known, so fetch the rest in one shot on a miss. It restarts
    // from `from` on each retry, so min_reachable is `from`: all the value's
    // chunks must be resident together to copy them out.
    let min_reachable = AtomicU64::new(from);
    self
      .drive(
        window,
        &min_reachable,
        move |off| to.saturating_sub(off).div_ceil(chunk_bytes).max(1),
        |walker| walker.read_range(from, to).map_err(TraverseError::from),
      )
      .await
  }
}

/// Structural-index cache accessors (table logic itself lives in `cache.rs`).
/// Each is a no-op when caching is disabled. Called from `count.rs` /
/// `cursor.rs` as scans learn child counts, closes, and array members.
impl Session {
  /// Run `f` against the cache under the lock, skipping when caching is off. The
  /// lock is never held across an `.await`.
  fn with_cache<R>(&self, f: impl FnOnce(&mut StructuralIndex) -> R) -> Option<R> {
    if !self.cache_enabled {
      return None;
    }
    Some(f(&mut self.cache.lock().unwrap()))
  }

  /// Cached child count for `(anchor, path)` if a prior `count`/`iter`/`walk`
  /// learned it - lets a repeat `count` skip the scan.
  pub(crate) fn cached_child_count(&self, anchor: u64, path: &[Segment]) -> Option<u64> {
    self
      .with_cache(|c| c.get(anchor, path)?.child_count())
      .flatten()
  }

  pub(crate) fn store_child_count(
    &self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    count: u64,
  ) {
    self.with_cache(|c| c.store_child_count(base_depth, anchor, path, kind, value_start, count));
  }

  /// Record the close offset (`}`/`]` + 1) of the container at `(anchor, path)`.
  pub(crate) fn store_close(
    &self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    close: u64,
  ) {
    self.with_cache(|c| c.store_close(base_depth, anchor, path, kind, value_start, close));
  }

  /// Record an array resume-point member `(index, offset)` so a later random
  /// index resumes near where `iter`/`walk` stopped.
  pub(crate) fn store_array_resume_point(
    &self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    index: usize,
    offset: u64,
  ) {
    self.with_cache(|c| {
      c.merge_array_scan(base_depth, anchor, path, value_start, &[(index, offset)])
    });
  }
}

impl Session {
  /// The shared chunk-fault retry loop: build a fresh [`Walker`] over the
  /// window, run a sync `step`, and on `ChunkMiss(off)` read a burst (sized by
  /// `burst_for`) into the window, drop everything below `min_reachable`, retry.
  ///
  /// `min_reachable` is the lowest offset the step might still read; the step
  /// advances it as it commits forward progress. Dropping below it keeps the
  /// window bounded without evicting a chunk a behind-frontier `read_range`
  /// (object keys) still needs.
  pub(crate) async fn drive<T>(
    &self,
    window: &mut ChunkWindow,
    min_reachable: &AtomicU64,
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
          window.drop_below(min_reachable.load(Ordering::Relaxed));
        }
        Err(e) => return Err(e.into()),
      }
    }
  }
}

/// RAII scope for a one-shot query: owns the transient [`ChunkWindow`] so its
/// chunks are released on return, including early-`?` and early-`return` paths.
///
/// The iterators in `cursor.rs` don't use this - they keep a long-lived window
/// across yields and prune it explicitly via [`Session::prune_window`].
pub(crate) struct Query {
  pub(crate) window: ChunkWindow,
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
  use std::sync::atomic::AtomicUsize;

  use async_trait::async_trait;
  use bytes::Bytes;

  use super::*;
  use crate::source::{InMemoryStream, SourceError};

  /// Counts `read` calls so the cache's effect on chunk faulting is observable.
  struct CountingSource {
    inner: InMemoryStream,
    reads: Arc<AtomicUsize>,
  }

  #[async_trait]
  impl ByteStream for CountingSource {
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
    index_cache_budget: usize,
    object_member_cap: usize,
    array_interval: usize,
  ) -> (Arc<Session>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn ByteStream> = Arc::new(CountingSource {
      inner: InMemoryStream::new(doc.into_bytes()),
      reads: reads.clone(),
    });
    (
      Session::new(
        source,
        chunk,
        index_cache_budget,
        object_member_cap,
        array_interval,
      )
      .unwrap(),
      reads,
    )
  }

  fn member(name: &str) -> Segment {
    Segment::Member(name.into())
  }

  /// `{"a":{"b":{"f0":0,...,"f199":199,"c":1,"d":2}}}` - c and d are the last
  /// members of a large object, so a cold scan of `b` is expensive.
  fn deep_object_doc() -> String {
    let mut b = String::from("{");
    for i in 0..200 {
      b.push_str(&format!("\"f{i}\":{i},"));
    }
    b.push_str("\"c\":1,\"d\":2}");
    format!("{{\"a\":{{\"b\":{b}}}}}")
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

  /// `{"arr":["<pad>", ... n elements ...]}` - a long flat array where a deep
  /// index sits many chunks in, so a backward re-get has real distance to save.
  fn flat_array_doc(n: usize, pad: usize) -> String {
    let p = "x".repeat(pad);
    let mut s = String::from("{\"arr\":[");
    for i in 0..n {
      if i > 0 {
        s.push(',');
      }
      s.push('"');
      s.push_str(&p);
      s.push('"');
    }
    s.push_str("]}");
    s
  }

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

  #[tokio::test]
  async fn cache_object_sibling_faults_fewer_chunks() {
    let path_c = [member("a"), member("b"), member("c")];
    let path_d = [member("a"), member("b"), member("d")];

    // Warm: resolve c (populates the chain + b's member table), so d resumes
    // from c's resume_point - a one-member scan.
    let (warm, warm_reads) = counting_session(
      deep_object_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let mut w = warm.new_window();
    warm
      .run_locate(&path_c, 0, 0, &mut w)
      .await
      .unwrap()
      .unwrap();
    warm_reads.store(0, Ordering::Relaxed);
    let mut w2 = warm.new_window();
    assert!(warm
      .run_locate(&path_d, 0, 0, &mut w2)
      .await
      .unwrap()
      .is_some());
    let warm_n = warm_reads.load(Ordering::Relaxed);

    // Cold: d on a fresh session scans root, a, and all of b from their opens.
    let (cold, cold_reads) = counting_session(
      deep_object_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let mut c = cold.new_window();
    assert!(cold
      .run_locate(&path_d, 0, 0, &mut c)
      .await
      .unwrap()
      .is_some());
    let cold_n = cold_reads.load(Ordering::Relaxed);

    assert!(
      warm_n < cold_n,
      "warm sibling access ({warm_n} reads) should fault fewer chunks than cold ({cold_n})"
    );
  }

  #[tokio::test]
  async fn cache_array_resume_faults_fewer_chunks() {
    let at = |i: usize| [member("arr"), Segment::Element(i)];

    let (warm, warm_reads) = counting_session(
      big_array_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let mut w = warm.new_window();
    warm
      .run_locate(&at(40), 0, 0, &mut w)
      .await
      .unwrap()
      .unwrap();
    warm_reads.store(0, Ordering::Relaxed);
    let mut w2 = warm.new_window();
    assert!(warm
      .run_locate(&at(50), 0, 0, &mut w2)
      .await
      .unwrap()
      .is_some());
    let warm_n = warm_reads.load(Ordering::Relaxed);

    let (cold, cold_reads) = counting_session(
      big_array_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let mut c = cold.new_window();
    assert!(cold
      .run_locate(&at(50), 0, 0, &mut c)
      .await
      .unwrap()
      .is_some());
    let cold_n = cold_reads.load(Ordering::Relaxed);

    assert!(
      warm_n < cold_n,
      "warm index resume ({warm_n} reads) should fault fewer chunks than cold ({cold_n})"
    );
  }

  #[tokio::test]
  async fn cache_backward_array_get_faults_fewer_chunks() {
    // The multi-member payoff: one deep get plants chunk-cadence array members
    // across the array, so a backward re-get resumes from the nearest array member
    // below its index instead of rescanning from the open.
    let doc = flat_array_doc(200, 120);
    let deep = [member("arr"), Segment::Element(180)];
    let back = [member("arr"), Segment::Element(20)];

    let (warm, warm_reads) = counting_session(
      doc.clone(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let mut w = warm.new_window();
    warm.run_locate(&deep, 0, 0, &mut w).await.unwrap().unwrap();
    warm_reads.store(0, Ordering::Relaxed);
    let mut w2 = warm.new_window();
    assert!(warm
      .run_locate(&back, 0, 0, &mut w2)
      .await
      .unwrap()
      .is_some());
    let warm_n = warm_reads.load(Ordering::Relaxed);

    let (cold, cold_reads) = counting_session(
      doc,
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let mut c = cold.new_window();
    assert!(cold
      .run_locate(&back, 0, 0, &mut c)
      .await
      .unwrap()
      .is_some());
    let cold_n = cold_reads.load(Ordering::Relaxed);

    assert!(
      warm_n < cold_n,
      "warm backward get ({warm_n} reads) should fault fewer chunks than cold ({cold_n})"
    );
  }

  #[tokio::test]
  async fn cache_repeat_count_issues_no_reads() {
    let (s, reads) = counting_session(
      big_array_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let path = [member("arr")];
    let first = crate::count::at(&s, &path, 0, 0).await.unwrap();
    assert_eq!(first, 100);
    assert!(reads.load(Ordering::Relaxed) > 0, "cold count must read");
    reads.store(0, Ordering::Relaxed);
    let second = crate::count::at(&s, &path, 0, 0).await.unwrap();
    assert_eq!(second, 100);
    assert_eq!(
      reads.load(Ordering::Relaxed),
      0,
      "a repeat count must be served from the cache with no reads"
    );
  }

  /// Pins the mechanism - which nodes and members a scan tables - complementing
  /// the read-count tests that prove the resulting benefit.
  #[tokio::test]
  async fn cache_state_records_walked_containers() {
    // {"a":{"b":{"c":1,"d":2,"e":3}}}: resolving c then d tables both on the
    // [a,b] node; e is never queried, so it stays untabled.
    let doc = r#"{"a":{"b":{"c":1,"d":2,"e":3}}}"#.to_string();
    let (s, _reads) = counting_session(
      doc,
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let ab = [member("a"), member("b")];

    // Cold: nothing cached yet.
    assert!(s.cache.lock().unwrap().get(0, &ab).is_none());

    let mut w = s.new_window();
    s.run_locate(&[member("a"), member("b"), member("c")], 0, 0, &mut w)
      .await
      .unwrap()
      .expect("c resolves");
    {
      let cache = s.cache.lock().unwrap();
      // The whole ancestor chain is tabled: root -> a, [a] -> b, [a,b] -> c.
      assert!(cache.get(0, &[]).unwrap().object_member("a").is_some());
      assert!(cache
        .get(0, &[member("a")])
        .unwrap()
        .object_member("b")
        .is_some());
      let n = cache.get(0, &ab).expect("the [a,b] container is cached");
      assert!(n.object_member("c").is_some(), "c was just resolved");
      assert!(n.object_member("d").is_none(), "d not yet seen");
      assert!(n.object_member("e").is_none());
    }

    let mut w2 = s.new_window();
    s.run_locate(&[member("a"), member("b"), member("d")], 0, 0, &mut w2)
      .await
      .unwrap()
      .expect("d resolves");
    {
      let cache = s.cache.lock().unwrap();
      let n = cache.get(0, &ab).unwrap();
      assert!(n.object_member("c").is_some(), "c still tabled");
      assert!(
        n.object_member("d").is_some(),
        "d now tabled (resumed past c)"
      );
      assert!(
        n.object_member("e").is_none(),
        "e never queried, never tabled"
      );
    }
  }

  #[tokio::test]
  async fn cache_state_array_records_array_member_at_target() {
    let (s, _reads) = counting_session(
      big_array_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let arr = [member("arr")];

    let mut w = s.new_window();
    s.run_locate(&[member("arr"), Segment::Element(40)], 0, 0, &mut w)
      .await
      .unwrap()
      .expect("element 40 resolves");
    // An exact array member was planted at the target, so the nearest at or below 40
    // is 40 itself.
    let nearest = s
      .cache
      .lock()
      .unwrap()
      .get(0, &arr)
      .expect("arr node")
      .nearest_array_member(40);
    match nearest {
      Some((index, _)) => assert_eq!(index, 40, "exact array member at the resolved index"),
      None => panic!("expected an array member at or below 40"),
    }
  }

  #[tokio::test]
  async fn cache_state_count_records_child_count() {
    let (s, _reads) = counting_session(
      big_array_doc(),
      256,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    let arr = [member("arr")];
    assert_eq!(crate::count::at(&s, &arr, 0, 0).await.unwrap(), 100);
    assert_eq!(
      s.cache.lock().unwrap().get(0, &arr).unwrap().child_count(),
      Some(100),
      "count must record the child count on the node"
    );
  }

  #[tokio::test]
  async fn cache_disabled_still_resolves() {
    // budget 0 => no caching; results must still resolve.
    let (s, _reads) = counting_session(
      deep_object_doc(),
      256,
      0,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    );
    assert!(!s.cache_enabled);
    let path = [member("a"), member("b"), member("d")];
    let mut w = s.new_window();
    assert!(s.run_locate(&path, 0, 0, &mut w).await.unwrap().is_some());
    // Repeat count is not short-circuited when disabled.
    let n = crate::count::at(&s, &[member("a"), member("b")], 0, 0)
      .await
      .unwrap();
    assert_eq!(n, 202);
    assert!(s.cache.lock().unwrap().get(0, &path).is_none());
  }

  /// The bounded-memory contract: a full scan of a many-chunk document keeps the
  /// byte window bounded by the burst, not by document size. An internal
  /// invariant (no `cacheStats` to assert it through).
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
    let source: Arc<dyn ByteStream> = Arc::new(InMemoryStream::new(doc.into_bytes()));
    let session = Session::new(
      source,
      4096,
      DEFAULT_INDEX_CACHE_ENTRIES,
      DEFAULT_OBJECT_MEMBER_CAP,
      DEFAULT_ARRAY_INDEX_INTERVAL,
    )
    .unwrap();

    let start = session
      .locate_at(&[Segment::Member("items".into())], 0, 0)
      .await
      .unwrap()
      .expect("items resolves");

    let mut window = session.new_window();
    let mut cursor = session
      .enter_container(start, &mut window)
      .await
      .unwrap()
      .expect("array");
    let bound = (MAX_BURST as usize) + 4; // one burst + small slack
    let mut seen = 0;
    while let Some(_child) = session.next_child(&mut cursor, &mut window).await.unwrap() {
      seen += 1;
      session.prune_window(&mut window, cursor.next_offset);
      assert!(
        window.len() <= bound,
        "window held {} chunks at element {seen} (bound {bound})",
        window.len()
      );
    }
    assert!(seen > 1000, "scanned {seen} elements");
  }
}
