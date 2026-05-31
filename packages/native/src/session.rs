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
use crate::path::Segment;
use crate::resolve::{self, ChildEntry, Children, ResolveState, ValueLocation};
use crate::select::SelectError;
use crate::source::Source;
use crate::walker::{self, SkipState, TraverseError, Walker};

use std::sync::Arc;

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
  pub fn new(source: Arc<dyn Source>, chunk_size: usize) -> Result<Arc<Self>, SessionError> {
    let source_size = source.size();
    let reader = ChunkReader::new(source, chunk_size)?;
    Ok(Arc::new(Self {
      source_size,
      chunk_size: reader.chunk_size(),
      reader,
    }))
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
  /// all route here. A future structural-index cache keyed by
  /// `(anchor_start, path) -> ValueLocation` belongs at exactly this boundary:
  /// it sits above the byte/bitmap layer with no coupling to the walk, so a
  /// cache hit returns the offset without faulting a single chunk. Keep these
  /// three the only resolution entry points so that cache has one place to live.
  pub(crate) async fn run_locate(
    &self,
    path: &[Segment],
    anchor_start: u64,
    window: &mut ByteWindow,
  ) -> Result<Option<u64>, SessionError> {
    // ResolveState persists across `ChunkMiss` retries; the floor follows the
    // resolver's committed iteration offset so chunks behind it are dropped
    // while the key currently being read (which `read_range`s behind the scan
    // frontier) stays resident.
    let mut state = ResolveState::new(anchor_start);
    let floor = AtomicU64::new(anchor_start);
    self
      .drive(window, &floor, doubling_burst(), |walker| {
        let r = resolve::resolve_step(walker, path, &mut state);
        floor.store(state.floor(), Ordering::Relaxed);
        r
      })
      .await
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
    let end = self.skip_value_at(start, window).await?;
    Ok(Some(ValueLocation { start, end }))
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
    let session = Session::new(source, 4096).unwrap();

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
}
