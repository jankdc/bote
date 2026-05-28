//! `Cursor` - the napi-exported class JavaScript holds.
//!
//! A Cursor is a handle around an [`Arc<Session>`] plus an optional anchor
//! [`ValueLocation`]. The root Cursor returned by `open()` has no anchor -
//! pointer resolution starts at byte 0. Sub-cursors yielded by `walk` carry
//! an anchor and resolve pointers relative to that location.
//!
//! `scan` / `walk` are sync methods returning [`CursorScan`] / [`CursorWalk`] -
//! napi async-iterators that lazily resolve their pointer on first `next()`
//! and then step through children one entry at a time. Each step retries
//! through `Pending` by fetching chunks as needed.

use std::collections::HashMap;
use std::sync::Arc;

use napi::bindgen_prelude::{Either, Error as NapiError};
use napi::tokio::sync::Mutex as AsyncMutex;
use napi_derive::napi;

use crate::cache::ChunkRef;
use crate::predicate::CompiledPredicate;
use crate::resolve::{ChildEntry, Children, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};

fn map_err(e: SessionError) -> NapiError {
  NapiError::from_reason(e.to_string())
}

/// Compile an optional serialized predicate IR. A malformed predicate errors
/// here - for `count`, as the promise rejection; `scan`/`walk` defer parsing
/// to the first `next()` (see `initialize_walker`).
fn parse_where(where_ir: Option<&str>) -> napi::Result<Option<CompiledPredicate>> {
  match where_ir {
    None => Ok(None),
    Some(json) => CompiledPredicate::parse(json)
      .map(Some)
      .map_err(|e| NapiError::from_reason(e.to_string())),
  }
}

/// Live snapshot of the session's chunk-cache occupancy. The cache is
/// shared by every cursor derived from one `open()` call, so any cursor
/// reports the same figures. `residentBytes + bitmapBytes` is the total
/// native memory held for source data and stays at or below `ceilingBytes`
/// regardless of document size - the library's bounded-memory contract.
#[napi(object)]
pub struct CacheStats {
  pub resident_bytes: f64,
  pub bitmap_bytes: f64,
  pub resident_chunks: f64,
  pub ceiling_bytes: f64,
}

/// Options for `scan`. A `#[napi(object)]` so the facade can grow it (select,
/// batch, ...) without changing the method's arity. `whereIr` is the
/// serialized predicate IR (see `predicate.rs`); `None` means no filter.
#[napi(object)]
pub struct ScanArgs {
  pub where_ir: Option<String>,
  /// Serialized projection IR (see `select.rs`); `None` yields the whole child.
  pub select_ir: Option<String>,
  /// Yield arrays of up to `batch` items instead of one at a time.
  pub batch: Option<f64>,
}

#[napi]
pub struct Cursor {
  session: Arc<Session>,
  anchor: Option<ValueLocation>,
  /// For sub-cursors yielded by `walk`: the key (for object members) or
  /// stringified index (for array elements). `None` for the root cursor.
  key: Option<CursorKey>,
}

#[derive(Clone)]
enum CursorKey {
  Member(String),
  Element(usize),
}

impl Cursor {
  pub(crate) fn root(session: Arc<Session>) -> Self {
    Self {
      session,
      anchor: None,
      key: None,
    }
  }

  fn child(session: Arc<Session>, location: ValueLocation, key: CursorKey) -> Self {
    Self {
      session,
      anchor: Some(location),
      key: Some(key),
    }
  }

  fn anchor_start(&self) -> u64 {
    self.anchor.map(|a| a.start).unwrap_or(0)
  }
}

#[napi]
impl Cursor {
  #[napi]
  pub async fn has(&self, pointer: String) -> napi::Result<bool> {
    self
      .session
      .has_at(&pointer, self.anchor_start())
      .await
      .map_err(map_err)
  }

  #[napi(ts_return_type = "Promise<unknown>")]
  pub async fn get(&self, pointer: String) -> napi::Result<serde_json::Value> {
    self
      .session
      .get_at(&pointer, self.anchor_start())
      .await
      .map_err(map_err)
  }

  #[napi]
  pub async fn count(&self, pointer: String, where_ir: Option<String>) -> napi::Result<f64> {
    let pred = parse_where(where_ir.as_deref())?;
    crate::count::at(&self.session, &pointer, self.anchor_start(), pred.as_ref())
      .await
      .map(|n| n as f64)
      .map_err(map_err)
  }

  #[napi(getter)]
  pub fn key(&self) -> Option<Either<String, u32>> {
    match &self.key {
      None => None,
      Some(CursorKey::Member(k)) => Some(Either::A(k.clone())),
      Some(CursorKey::Element(i)) => Some(Either::B(*i as u32)),
    }
  }

  #[napi]
  pub fn scan(&self, pointer: String, options: Option<ScanArgs>) -> CursorScan {
    let (where_ir, select_ir, batch) = match options {
      Some(o) => (
        o.where_ir,
        o.select_ir,
        // The facade rejects batch <= 0; guard here too (sub-1 -> no batching).
        o.batch.filter(|b| *b >= 1.0).map(|b| b as usize),
      ),
      None => (None, None, None),
    };
    CursorScan::new(
      self.session.clone(),
      pointer,
      self.anchor_start(),
      where_ir,
      select_ir,
      batch,
    )
  }

  #[napi]
  pub fn walk(&self, pointer: String, where_ir: Option<String>) -> CursorWalk {
    CursorWalk::new(self.session.clone(), pointer, self.anchor_start(), where_ir)
  }

  #[napi]
  pub fn cache_stats(&self) -> CacheStats {
    let cache = &self.session.cache;
    CacheStats {
      resident_bytes: cache.resident_bytes() as f64,
      bitmap_bytes: cache.bitmap_bytes() as f64,
      resident_chunks: cache.resident_chunks() as f64,
      ceiling_bytes: cache.derived_ceiling_bytes() as f64,
    }
  }
}

/// Shared iteration state. Each `next()` call locks this briefly to
/// snapshot or update; awaits happen with the lock held (tokio Mutex).
struct IterState {
  pointer: String,
  anchor_start: u64,
  initialized: bool,
  /// Set after first `next()` finishes initialization. `None` if the
  /// pointer resolved to a non-container (iteration yields nothing).
  walker: Option<Children>,
  /// Pin map reused across yields. At rest (between yields) it holds at
  /// most the single chunk covering the walker's `next_offset`, so the
  /// next yield's first byte read is a cache hit. After each yield we
  /// prune everything else and that keeps consecutive same-chunk yields
  /// from re-pinning the same chunk on every call while still bounding
  /// resident-pin count to 1, so the cap contract that motivated the
  /// original per-yield map is preserved.
  pinned: HashMap<u64, ChunkRef>,
  /// Serialized predicate IR (the `where` filter), parsed lazily into `pred`
  /// on first `next()` so a malformed predicate surfaces there.
  where_ir: Option<String>,
  /// Compiled `where` filter. `None` yields every child; `Some` yields only
  /// matches - rejected children are never materialized.
  pred: Option<CompiledPredicate>,
  /// Serialized projection IR, parsed lazily into `select` on first `next()`.
  select_ir: Option<String>,
  /// Compiled `select` projection (scan only). `None` yields the whole child.
  select: Option<CompiledSelect>,
  /// Batch size (scan only). `Some(n)` yields arrays of up to `n` items.
  batch: Option<usize>,
}

impl IterState {
  fn new(
    pointer: String,
    anchor_start: u64,
    where_ir: Option<String>,
    select_ir: Option<String>,
    batch: Option<usize>,
  ) -> Self {
    Self {
      pointer,
      anchor_start,
      initialized: false,
      walker: None,
      pinned: HashMap::new(),
      where_ir,
      pred: None,
      select_ir,
      select: None,
      batch,
    }
  }
}

async fn initialize_walker(session: &Session, state: &mut IterState) -> Result<(), SessionError> {
  // Compile the `where` predicate once, on first use. A malformed predicate
  // (bad JSON or sub-pointer) surfaces here, as this first `next()`'s error.
  let pred = match state.where_ir.as_deref() {
    Some(json) => Some(CompiledPredicate::parse(json)?),
    None => None,
  };
  state.pred = pred;
  let select = match state.select_ir.as_deref() {
    Some(json) => Some(CompiledSelect::parse(json)?),
    None => None,
  };
  state.select = select;

  let start_opt = session
    .locate_at(&state.pointer, state.anchor_start)
    .await?;

  if let Some(start) = start_opt {
    state.walker = session.enter_container(start, &mut state.pinned).await?;
    // Hand off to the per-yield pruning loop: keep just the chunk at
    // the upcoming `next_offset` so the first yield's read is hot.
    if let Some(w) = &state.walker {
      prune_pins(session, &mut state.pinned, w.next_offset);
    } else {
      state.pinned.clear();
    }
    session.sync_bitmap_evictions();
  }
  state.initialized = true;
  Ok(())
}

/// Release all pins held by an iterator on early termination.
async fn release_on_complete<Y>(
  session: Arc<Session>,
  state: Arc<AsyncMutex<IterState>>,
) -> napi::Result<Option<Y>> {
  let mut guard = state.lock().await;
  guard.pinned.clear();
  guard.walker = None;
  session.sync_bitmap_evictions();
  Ok(None)
}

/// Drop every pin except the one covering `next_offset`. This bounds the
/// resident-pin count to 1 between yields so the cache's own eviction
/// loop is free to maintain `maxResidentChunks`.
fn prune_pins(session: &Session, pinned: &mut HashMap<u64, ChunkRef>, next_offset: u64) {
  if next_offset >= session.source_size {
    pinned.clear();
    return;
  }
  let keep = (next_offset / session.chunk_size) * session.chunk_size;
  pinned.retain(|&off, _| off == keep);
}

#[napi(async_iterator)]
pub struct CursorScan {
  session: Arc<Session>,
  state: Arc<AsyncMutex<IterState>>,
}

impl CursorScan {
  fn new(
    session: Arc<Session>,
    pointer: String,
    anchor_start: u64,
    where_ir: Option<String>,
    select_ir: Option<String>,
    batch: Option<usize>,
  ) -> Self {
    Self {
      session,
      state: Arc::new(AsyncMutex::new(IterState::new(
        pointer,
        anchor_start,
        where_ir,
        select_ir,
        batch,
      ))),
    }
  }
}

#[napi]
impl napi::bindgen_prelude::AsyncGenerator for CursorScan {
  type Yield = serde_json::Value;
  type Next = ();
  type Return = ();

  fn next(
    &mut self,
    _value: Option<Self::Next>,
  ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
    let session = self.session.clone();
    let state = self.state.clone();
    async move {
      let mut guard = state.lock().await;
      if !guard.initialized {
        initialize_walker(&session, &mut guard)
          .await
          .map_err(map_err)?;
      }
      let IterState {
        walker,
        pinned,
        pred,
        select,
        batch,
        ..
      } = &mut *guard;
      let Some(walker) = walker.as_mut() else {
        return Ok(None);
      };
      let pred = pred.as_ref();
      let select = select.as_ref();
      let batch = *batch;
      let result = async {
        // Items are materialized eagerly and accumulated here, so the buffer
        // (not chunk pins) is the in-flight batch; pins are pruned after each
        // item. The buffer lives in this `next()` frame, so early termination
        // via `complete` needs no special handling.
        let mut buf: Vec<serde_json::Value> = Vec::new();
        loop {
          let Some(child) = session.next_child(walker, pinned).await? else {
            // Exhausted: flush a final partial batch, otherwise end.
            if batch.is_some() && !buf.is_empty() {
              return Ok(Some(serde_json::Value::Array(buf)));
            }
            return Ok(None);
          };
          let keep = match pred {
            Some(p) => crate::eval::matches(&session, p, child.location().start, pinned).await?,
            None => true,
          };
          if !keep {
            // Non-match: never materialized. Prune the frontier, then advance.
            prune_pins(&session, pinned, walker.next_offset);
            session.sync_bitmap_evictions();
            continue;
          }
          let value = match select {
            Some(sel) => {
              crate::eval::project(&session, sel, child.location().start, pinned).await?
            }
            None => session.materialize(child.location(), pinned).await?,
          };
          // The value is owned now; release the chunks that backed it.
          prune_pins(&session, pinned, walker.next_offset);
          session.sync_bitmap_evictions();
          match batch {
            None => return Ok(Some(value)),
            Some(n) => {
              buf.push(value);
              if buf.len() >= n {
                return Ok(Some(serde_json::Value::Array(buf)));
              }
            }
          }
        }
      }
      .await
      .map_err(map_err);
      prune_pins(&session, pinned, walker.next_offset);
      session.sync_bitmap_evictions();
      result
    }
  }

  fn complete(
    &mut self,
    _value: Option<Self::Return>,
  ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
    release_on_complete(self.session.clone(), self.state.clone())
  }
}

#[napi(async_iterator)]
pub struct CursorWalk {
  session: Arc<Session>,
  state: Arc<AsyncMutex<IterState>>,
}

impl CursorWalk {
  fn new(
    session: Arc<Session>,
    pointer: String,
    anchor_start: u64,
    where_ir: Option<String>,
  ) -> Self {
    Self {
      session,
      // walk navigates positions, so it never projects or batches.
      state: Arc::new(AsyncMutex::new(IterState::new(
        pointer,
        anchor_start,
        where_ir,
        None,
        None,
      ))),
    }
  }
}

#[napi]
impl napi::bindgen_prelude::AsyncGenerator for CursorWalk {
  // Yield Cursor directly - napi-rs's ToNapiValue impl (synthesized by the
  // `#[napi]` attribute on Cursor) wraps it into a JS class instance when
  // the iterator's `next()` finalizes the yield in JS-thread context.
  type Yield = Cursor;
  type Next = ();
  type Return = ();

  fn next(
    &mut self,
    _value: Option<Self::Next>,
  ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
    let session = self.session.clone();
    let state = self.state.clone();
    async move {
      let mut guard = state.lock().await;
      if !guard.initialized {
        initialize_walker(&session, &mut guard)
          .await
          .map_err(map_err)?;
      }
      let IterState {
        walker,
        pinned,
        pred,
        ..
      } = &mut *guard;
      let Some(walker) = walker.as_mut() else {
        return Ok(None);
      };
      let pred = pred.as_ref();
      let result = async {
        loop {
          let Some(child) = session.next_child(walker, pinned).await? else {
            return Ok::<Option<ChildEntry>, SessionError>(None);
          };
          let keep = match pred {
            Some(p) => crate::eval::matches(&session, p, child.location().start, pinned).await?,
            None => true,
          };
          if keep {
            return Ok(Some(child));
          }
          // Non-match: prune to the frontier, then advance to the next child.
          prune_pins(&session, pinned, walker.next_offset);
          session.sync_bitmap_evictions();
        }
      }
      .await
      .map_err(map_err);
      prune_pins(&session, pinned, walker.next_offset);
      session.sync_bitmap_evictions();
      let entry = result?;
      let Some(child) = entry else {
        return Ok(None);
      };
      let key = match &child {
        ChildEntry::Member { key, .. } => CursorKey::Member(key.clone()),
        ChildEntry::Element { index, .. } => CursorKey::Element(*index),
      };
      Ok(Some(Cursor::child(session.clone(), child.location(), key)))
    }
  }

  fn complete(
    &mut self,
    _value: Option<Self::Return>,
  ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
    release_on_complete(self.session.clone(), self.state.clone())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cache::CacheOptions;
  use crate::source::{InMemorySource, Source};
  use napi::bindgen_prelude::AsyncGenerator;

  /// `{"items":[{"name":"i0000",...}, ...]}` sized to span many chunks so a
  /// walk pins a real frontier chunk and the cap is in force throughout.
  fn array_doc(items: usize) -> Vec<u8> {
    let mut doc = String::from("{\"items\":[");
    for i in 0..items {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&format!("{{\"name\":\"i{i:04}\",\"total\":{i}}}"));
    }
    doc.push_str("]}");
    doc.into_bytes()
  }

  fn session(items: usize, chunk_size: usize, max_chunks: u32) -> Arc<Session> {
    let source: Arc<dyn Source> = Arc::new(InMemorySource::new(array_doc(items)));
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
  async fn pins_walk_released_on_complete() {
    let s = session(500, 256, 4);
    let mut w = CursorWalk::new(s.clone(), "/items".into(), 0, None);
    // Advance a few elements so the walker holds a live frontier pin.
    for _ in 0..3 {
      assert!(w.next(None).await.unwrap().is_some());
    }
    assert!(
      !w.state.lock().await.pinned.is_empty(),
      "walk should hold a frontier pin between yields"
    );

    w.complete(None).await.unwrap();

    {
      let guard = w.state.lock().await;
      assert!(guard.pinned.is_empty(), "complete() must clear all pins");
      assert!(guard.walker.is_none(), "complete() must drop the walker");
    }
    // A resumed next() after completion yields nothing.
    assert!(w.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_walk_safe_when_child_escapes() {
    let s = session(500, 256, 4);
    let mut w = CursorWalk::new(s.clone(), "/items".into(), 0, None);
    let child = w.next(None).await.unwrap().expect("first child");

    w.complete(None).await.unwrap();

    assert!(w.state.lock().await.pinned.is_empty());
    // The escaped child is still fully usable: its session outlives the walk.
    assert_eq!(
      child.get("/name".into()).await.unwrap(),
      serde_json::json!("i0000")
    );
  }

  #[tokio::test]
  async fn pins_walk_abandoned_stay_under_cap() {
    let s = session(2000, 256, 4);
    let ceiling = s.cache.derived_ceiling_bytes();
    let mut abandoned = Vec::new();
    for _ in 0..64 {
      let mut w = CursorWalk::new(s.clone(), "/items".into(), 0, None);
      // Partial walk, then early-terminate via complete() (the break path).
      assert!(w.next(None).await.unwrap().is_some());
      w.complete(None).await.unwrap();
      abandoned.push(w); // keep alive: no Drop, no GC

      let total = s.cache.resident_bytes() + s.cache.bitmap_bytes();
      assert!(
        total <= ceiling,
        "resident {} + bitmap {} exceeded ceiling {} with {} abandoned iterators",
        s.cache.resident_bytes(),
        s.cache.bitmap_bytes(),
        ceiling,
        abandoned.len(),
      );
    }
  }

  #[tokio::test]
  async fn pins_scan_released_on_complete() {
    let s = session(500, 256, 4);
    let mut it = CursorScan::new(s.clone(), "/items".into(), 0, None, None, None);
    for _ in 0..3 {
      assert!(it.next(None).await.unwrap().is_some());
    }
    assert!(!it.state.lock().await.pinned.is_empty());

    it.complete(None).await.unwrap();

    {
      let guard = it.state.lock().await;
      assert!(guard.pinned.is_empty());
      assert!(guard.walker.is_none());
    }
    assert!(it.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_scan_batch_early_break_releases() {
    let s = session(500, 256, 4);
    let mut it = CursorScan::new(s.clone(), "/items".into(), 0, None, None, Some(8));
    // Pull one batch, then early-terminate via complete() (the break path).
    assert!(it.next(None).await.unwrap().is_some());
    it.complete(None).await.unwrap();
    let guard = it.state.lock().await;
    assert!(
      guard.pinned.is_empty(),
      "complete() must clear pins after a batch"
    );
    assert!(guard.walker.is_none());
  }

  #[tokio::test]
  async fn pins_scan_batch_peak_bounded() {
    // Batching a large array under a tight cap stays under the cache ceiling.
    let s = session(2000, 256, 4);
    let ceiling = s.cache.derived_ceiling_bytes();
    let mut it = CursorScan::new(
      s.clone(),
      "/items".into(),
      0,
      None,
      Some(r#"{"one":"/total"}"#.to_string()),
      Some(64),
    );
    while it.next(None).await.unwrap().is_some() {
      let total = s.cache.resident_bytes() + s.cache.bitmap_bytes();
      assert!(
        total <= ceiling,
        "resident {total} exceeded ceiling {ceiling} while batching"
      );
    }
  }
}
