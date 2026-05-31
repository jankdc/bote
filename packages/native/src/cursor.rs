//! `Cursor` - the napi-exported class JavaScript holds.
//!
//! A Cursor is a handle around an [`Arc<Session>`] plus an optional anchor
//! [`ValueLocation`]. The root Cursor returned by `open()` has no anchor - path
//! resolution starts at byte 0. Sub-cursors yielded by `walk` carry an anchor
//! and resolve paths relative to that location.
//!
//! `iter` / `walk` are sync methods returning [`CursorIter`] / [`CursorWalk`] -
//! napi async-iterators that lazily resolve their path on first `next()` and
//! then step through children one entry at a time. Each step retries through
//! `Pending` by fetching chunks as needed.

use std::sync::Arc;

use napi::bindgen_prelude::{Either, Error as NapiError};
use napi::tokio::sync::Mutex as AsyncMutex;
use napi_derive::napi;

use crate::chunks::ByteWindow;
use crate::path::{self, Segment};
use crate::resolve::{ChildEntry, Children, ContainerKind, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};

fn map_err(e: SessionError) -> NapiError {
  NapiError::from_reason(e.to_string())
}

/// Options for `iter`. A `#[napi(object)]` so the facade can grow it without
/// changing the method's arity.
#[napi(object)]
pub struct IterArgs {
  /// Serialized projection IR (see `select.rs`); `None` yields the whole child.
  pub select_ir: Option<String>,
  /// Batch size: each yield is an array of up to this many items.
  pub batch: f64,
  /// Yield `[key, value]` tuples instead of bare values. The key is a string for
  /// object members and a number for array elements.
  pub with_key: Option<bool>,
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
  #[napi(ts_args_type = "path: Array<string | number>")]
  pub async fn has(&self, path: Vec<Either<String, u32>>) -> napi::Result<bool> {
    self
      .session
      .has_at(&path::from_napi(path), self.anchor_start())
      .await
      .map_err(map_err)
  }

  #[napi(
    ts_args_type = "path: Array<string | number>",
    ts_return_type = "Promise<unknown>"
  )]
  pub async fn get(
    &self,
    path: Vec<Either<String, u32>>,
  ) -> napi::Result<Either<serde_json::Value, ()>> {
    self
      .session
      .get_at(&path::from_napi(path), self.anchor_start())
      .await
      .map(|opt| match opt {
        Some(v) => Either::A(v),
        None => Either::B(()),
      })
      .map_err(map_err)
  }

  #[napi(ts_args_type = "path: Array<string | number>")]
  pub async fn count(&self, path: Vec<Either<String, u32>>) -> napi::Result<f64> {
    crate::count::at(&self.session, &path::from_napi(path), self.anchor_start())
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

  #[napi(ts_args_type = "path: Array<string | number>, options: IterArgs")]
  pub fn iter(&self, path: Vec<Either<String, u32>>, options: IterArgs) -> CursorIter {
    CursorIter::new(
      self.session.clone(),
      path::from_napi(path),
      self.anchor_start(),
      options.select_ir,
      (options.batch as usize).max(1),
      options.with_key.unwrap_or(false),
    )
  }

  #[napi(ts_args_type = "path: Array<string | number>")]
  pub fn walk(&self, path: Vec<Either<String, u32>>) -> CursorWalk {
    CursorWalk::new(
      self.session.clone(),
      path::from_napi(path),
      self.anchor_start(),
    )
  }
}

/// State shared by `iter` and `walk`: the lazily-resolved container walker plus
/// the byte window carried across yields. Each `next()` call locks this briefly
/// to snapshot or update; awaits happen with the lock held (tokio Mutex).
struct StreamCore {
  path: Vec<Segment>,
  anchor_start: u64,
  initialized: bool,
  /// Set after first `next()` finishes initialization. `None` if the path didn't
  /// resolve, or resolved to a non-container (iteration yields nothing).
  walker: Option<Children>,
  /// Byte window reused across yields. At rest it holds at most the single chunk
  /// covering the walker's `next_offset`, so the next yield's first read is a
  /// hit; everything else is pruned after each yield, bounding resident chunks
  /// to ~1 between yields.
  window: ByteWindow,
}

impl StreamCore {
  fn new(session: &Session, path: Vec<Segment>, anchor_start: u64) -> Self {
    Self {
      path,
      anchor_start,
      initialized: false,
      walker: None,
      window: session.new_window(),
    }
  }
}

/// `iter`-only state: the shared [`StreamCore`] plus projection, batching, and
/// key-wrapping. `walk` navigates positions and uses [`StreamCore`] directly.
struct IterState {
  core: StreamCore,
  /// Serialized projection IR, parsed lazily into `select` on first `next()`.
  select_ir: Option<String>,
  /// Compiled `select` projection. `None` yields the whole child.
  select: Option<CompiledSelect>,
  /// Batch size: each yield is an array of up to `batch` items.
  batch: usize,
  /// Wrap each yielded value in a `[key, value]` array.
  with_key: bool,
}

impl IterState {
  fn new(
    session: &Session,
    path: Vec<Segment>,
    anchor_start: u64,
    select_ir: Option<String>,
    batch: usize,
    with_key: bool,
  ) -> Self {
    Self {
      core: StreamCore::new(session, path, anchor_start),
      select_ir,
      select: None,
      batch,
      with_key,
    }
  }
}

/// Resolve the path and open its container walker, pruning to the frontier so
/// the first yield's read is hot. Shared by `iter` and `walk`; `select`
/// compilation (iter-only) happens in the caller before this runs.
async fn locate_and_enter(session: &Session, core: &mut StreamCore) -> Result<(), SessionError> {
  if let Some(start) = session.locate_at(&core.path, core.anchor_start).await? {
    core.walker = session.enter_container(start, &mut core.window).await?;
    if let Some(w) = &core.walker {
      session.prune_window(&mut core.window, w.next_offset);
    } else {
      core.window.clear();
    }
  }
  core.initialized = true;
  Ok(())
}

/// Release the window held by an iterator on early termination (`complete`).
fn release_core(core: &mut StreamCore) {
  core.window.clear();
  core.walker = None;
}

#[napi(async_iterator)]
pub struct CursorIter {
  session: Arc<Session>,
  state: Arc<AsyncMutex<IterState>>,
}

impl CursorIter {
  fn new(
    session: Arc<Session>,
    path: Vec<Segment>,
    anchor_start: u64,
    select_ir: Option<String>,
    batch: usize,
    with_key: bool,
  ) -> Self {
    let state = IterState::new(&session, path, anchor_start, select_ir, batch, with_key);
    Self {
      session,
      state: Arc::new(AsyncMutex::new(state)),
    }
  }
}

/// Render a `ChildEntry`'s key as a JSON value for tuple yields. Member keys
/// become strings; element indices become numbers.
fn child_key_json(child: &ChildEntry) -> serde_json::Value {
  match child {
    ChildEntry::Member { key, .. } => serde_json::Value::String(key.clone()),
    ChildEntry::Element { index, .. } => serde_json::Value::Number((*index as u64).into()),
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
      if !guard.core.initialized {
        guard.select = match guard.select_ir.as_deref() {
          Some(json) => Some(CompiledSelect::parse(json).map_err(|e| map_err(e.into()))?),
          None => None,
        };
        locate_and_enter(&session, &mut guard.core)
          .await
          .map_err(map_err)?;
        if let Some(w) = guard.core.walker.as_ref() {
          if w.kind == ContainerKind::Object {
            release_core(&mut guard.core);
            return Err(NapiError::from_reason(
              "iter target is an object; use walk() to iterate object members".to_string(),
            ));
          }
        }
      }
      let IterState {
        core,
        select,
        batch,
        with_key,
        ..
      } = &mut *guard;
      let select = select.as_ref();
      let batch = *batch;
      let with_key = *with_key;
      let StreamCore { walker, window, .. } = core;
      let Some(walker) = walker.as_mut() else {
        return Ok(None);
      };
      // Items are materialized eagerly and accumulated here; the window is
      // pruned after each item, so the buffer (not chunks) is the in-flight
      // batch. The buffer lives in this `next()` frame, so early termination via
      // `complete` needs no special handling.
      let result: Result<Option<serde_json::Value>, SessionError> = async {
        let mut buf: Vec<serde_json::Value> = Vec::with_capacity(batch);
        loop {
          let Some(child) = session.next_child(walker, window).await? else {
            if buf.is_empty() {
              return Ok(None);
            }
            return Ok(Some(serde_json::Value::Array(buf)));
          };
          let key = if with_key {
            Some(child_key_json(&child))
          } else {
            None
          };
          let value = match select {
            Some(sel) => {
              crate::eval::project(&session, sel, child.location().start, window).await?
            }
            None => session.materialize(child.location(), window).await?,
          };
          session.prune_window(window, walker.next_offset);
          let item = match key {
            Some(k) => serde_json::Value::Array(vec![k, value]),
            None => value,
          };
          buf.push(item);
          if buf.len() >= batch {
            return Ok(Some(serde_json::Value::Array(buf)));
          }
        }
      }
      .await;
      // End-of-next defensive prune: any error path also lands here so abandoned
      // iterators don't retain chunks past the frontier.
      session.prune_window(window, walker.next_offset);
      result.map_err(map_err)
    }
  }

  fn complete(
    &mut self,
    _value: Option<Self::Return>,
  ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
    let state = self.state.clone();
    async move {
      release_core(&mut state.lock().await.core);
      Ok(None)
    }
  }
}

#[napi(async_iterator)]
pub struct CursorWalk {
  session: Arc<Session>,
  state: Arc<AsyncMutex<StreamCore>>,
}

impl CursorWalk {
  fn new(session: Arc<Session>, path: Vec<Segment>, anchor_start: u64) -> Self {
    let core = StreamCore::new(&session, path, anchor_start);
    Self {
      session,
      state: Arc::new(AsyncMutex::new(core)),
    }
  }
}

#[napi]
impl napi::bindgen_prelude::AsyncGenerator for CursorWalk {
  // Yield Cursor directly - napi-rs's ToNapiValue impl wraps it into a JS class
  // instance when the iterator's `next()` finalizes the yield in JS-thread context.
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
        locate_and_enter(&session, &mut guard)
          .await
          .map_err(map_err)?;
      }
      let StreamCore { walker, window, .. } = &mut *guard;
      let Some(walker) = walker.as_mut() else {
        return Ok(None);
      };
      let entry = session.next_child(walker, window).await.map_err(map_err)?;
      session.prune_window(window, walker.next_offset);
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
    let state = self.state.clone();
    async move {
      let mut guard = state.lock().await;
      release_core(&mut guard);
      Ok(None)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::session::MAX_BURST;
  use crate::source::{InMemorySource, Source};
  use napi::bindgen_prelude::AsyncGenerator;

  fn items_path() -> Vec<Segment> {
    vec![Segment::Member("items".into())]
  }

  /// `{"items":[{"name":"i0000",...}, ...]}` sized to span many chunks so a walk
  /// pins a real frontier chunk and the window bound is in force throughout.
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

  fn session(items: usize, chunk_size: usize) -> Arc<Session> {
    let source: Arc<dyn Source> = Arc::new(InMemorySource::new(array_doc(items)));
    Session::new(source, chunk_size).unwrap()
  }

  /// One burst of chunks plus a small slack is the resident-window bound.
  fn window_bound() -> usize {
    MAX_BURST as usize + 4
  }

  #[tokio::test]
  async fn pins_walk_released_on_complete() {
    let s = session(500, 256);
    let mut w = CursorWalk::new(s.clone(), items_path(), 0);
    for _ in 0..3 {
      assert!(w.next(None).await.unwrap().is_some());
    }
    assert!(
      !w.state.lock().await.window.is_empty(),
      "walk should hold a frontier chunk between yields"
    );

    w.complete(None).await.unwrap();

    {
      let guard = w.state.lock().await;
      assert!(guard.window.is_empty(), "complete() must clear the window");
      assert!(guard.walker.is_none(), "complete() must drop the walker");
    }
    assert!(w.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_walk_safe_when_child_escapes() {
    let s = session(500, 256);
    let mut w = CursorWalk::new(s.clone(), items_path(), 0);
    let child = w.next(None).await.unwrap().expect("first child");

    w.complete(None).await.unwrap();

    assert!(w.state.lock().await.window.is_empty());
    // The escaped child is still fully usable: its session outlives the walk.
    assert!(matches!(
      child.get(vec![Either::A("name".into())]).await.unwrap(),
      Either::A(ref v) if v == &serde_json::json!("i0000")
    ));
  }

  #[tokio::test]
  async fn pins_walk_abandoned_stay_bounded() {
    let s = session(2000, 256);
    let bound = window_bound();
    let mut abandoned = Vec::new();
    for _ in 0..64 {
      let mut w = CursorWalk::new(s.clone(), items_path(), 0);
      assert!(w.next(None).await.unwrap().is_some());
      w.complete(None).await.unwrap();
      // After complete(), the abandoned iterator's window is cleared - no leak.
      assert!(w.state.lock().await.window.is_empty());
      abandoned.push(w); // keep alive: no Drop, no GC
    }
    assert_eq!(abandoned.len(), 64);
    let _ = bound;
  }

  #[tokio::test]
  async fn pins_iter_released_on_complete() {
    let s = session(500, 256);
    let mut it = CursorIter::new(s.clone(), items_path(), 0, None, 1, false);
    for _ in 0..3 {
      assert!(it.next(None).await.unwrap().is_some());
    }
    assert!(!it.state.lock().await.core.window.is_empty());

    it.complete(None).await.unwrap();

    {
      let guard = it.state.lock().await;
      assert!(guard.core.window.is_empty());
      assert!(guard.core.walker.is_none());
    }
    assert!(it.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_iter_batch_early_break_releases() {
    let s = session(500, 256);
    let mut it = CursorIter::new(s.clone(), items_path(), 0, None, 8, false);
    assert!(it.next(None).await.unwrap().is_some());
    it.complete(None).await.unwrap();
    let guard = it.state.lock().await;
    assert!(
      guard.core.window.is_empty(),
      "complete() must clear the window after a batch"
    );
    assert!(guard.core.walker.is_none());
  }

  #[tokio::test]
  async fn pins_iter_batch_window_bounded() {
    // Batching a large array stays bounded by the burst window, not doc size.
    let s = session(2000, 256);
    let bound = window_bound();
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
      Some(r#"{"one":["total"]}"#.to_string()),
      64,
      false,
    );
    while it.next(None).await.unwrap().is_some() {
      let len = it.state.lock().await.core.window.len();
      assert!(
        len <= bound,
        "window held {len} chunks while batching (bound {bound})"
      );
    }
  }

  #[tokio::test]
  async fn iter_on_object_target_throws() {
    let s = session(50, 256);
    let mut it = CursorIter::new(s.clone(), Vec::new(), 0, None, 8, false);
    let err = it.next(None).await.expect_err("object target must throw");
    assert!(
      err.reason.contains("use walk()"),
      "error should steer the caller to walk(), got: {}",
      err.reason
    );
    let guard = it.state.lock().await;
    assert!(guard.core.window.is_empty(), "gate must release the window");
    assert!(guard.core.walker.is_none(), "gate must drop the walker");
  }
}
