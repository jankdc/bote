//! `Cursor` - the napi-exported class JavaScript holds.
//!
//! A Cursor is a handle around an [`Arc<Session>`] plus an optional anchor
//! [`ValueLocation`]. The root Cursor returned by `open()` has no anchor -
//! pointer resolution starts at byte 0. Sub-cursors yielded by `walk` carry
//! an anchor and resolve pointers relative to that location.
//!
//! `iter` / `walk` are sync methods returning [`CursorIter`] / [`CursorWalk`] -
//! napi async-iterators that lazily resolve their pointer on first `next()`
//! and then step through children one entry at a time. Each step retries
//! through `Pending` by fetching chunks as needed.

use std::collections::HashMap;
use std::sync::Arc;

use napi::bindgen_prelude::{Either, Error as NapiError};
use napi::tokio::sync::Mutex as AsyncMutex;
use napi_derive::napi;

use crate::cache::ChunkRef;
use crate::resolve::{ChildEntry, Children, ValueLocation};
use crate::session::{Session, SessionError};

fn map_err(e: SessionError) -> NapiError {
  NapiError::from_reason(e.to_string())
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

  #[napi(getter)]
  pub fn key(&self) -> Option<Either<String, u32>> {
    match &self.key {
      None => None,
      Some(CursorKey::Member(k)) => Some(Either::A(k.clone())),
      Some(CursorKey::Element(i)) => Some(Either::B(*i as u32)),
    }
  }

  #[napi]
  pub fn iter(&self, pointer: String) -> CursorIter {
    CursorIter::new(self.session.clone(), pointer, self.anchor_start())
  }

  #[napi]
  pub fn walk(&self, pointer: String) -> CursorWalk {
    CursorWalk::new(self.session.clone(), pointer, self.anchor_start())
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
}

impl IterState {
  fn new(pointer: String, anchor_start: u64) -> Self {
    Self {
      pointer,
      anchor_start,
      initialized: false,
      walker: None,
      pinned: HashMap::new(),
    }
  }
}

async fn initialize_walker(session: &Session, state: &mut IterState) -> Result<(), SessionError> {
  let loc_opt = session
    .resolve_at(&state.pointer, state.anchor_start)
    .await?;
  if let Some(loc) = loc_opt {
    state.walker = session.enter_container(loc, &mut state.pinned).await?;
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
pub struct CursorIter {
  session: Arc<Session>,
  state: Arc<AsyncMutex<IterState>>,
}

impl CursorIter {
  fn new(session: Arc<Session>, pointer: String, anchor_start: u64) -> Self {
    Self {
      session,
      state: Arc::new(AsyncMutex::new(IterState::new(pointer, anchor_start))),
    }
  }
}

#[napi]
impl napi::bindgen_prelude::AsyncGenerator for CursorIter {
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
      let IterState { walker, pinned, .. } = &mut *guard;
      let Some(walker) = walker.as_mut() else {
        return Ok(None);
      };
      let result = async {
        let entry = session.next_child(walker, pinned).await?;
        match entry {
          None => Ok(None),
          Some(child) => Ok(Some(session.materialize(child.location(), pinned).await?)),
        }
      }
      .await
      .map_err(map_err);
      prune_pins(&session, pinned, walker.next_offset);
      session.sync_bitmap_evictions();
      result
    }
  }
}

#[napi(async_iterator)]
pub struct CursorWalk {
  session: Arc<Session>,
  state: Arc<AsyncMutex<IterState>>,
}

impl CursorWalk {
  fn new(session: Arc<Session>, pointer: String, anchor_start: u64) -> Self {
    Self {
      session,
      state: Arc::new(AsyncMutex::new(IterState::new(pointer, anchor_start))),
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
      let IterState { walker, pinned, .. } = &mut *guard;
      let Some(walker) = walker.as_mut() else {
        return Ok(None);
      };
      let result = session.next_child(walker, pinned).await.map_err(map_err);
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
}
