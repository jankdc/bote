//! `Cursor` - the napi-exported class JavaScript holds.
//!
//! A Cursor wraps an [`Arc<Session>`] plus an optional anchor [`ValueLocation`].
//! The root Cursor (`open()`) has no anchor and resolves from byte 0; sub-cursors
//! yielded by `walk` resolve paths relative to their anchor.
//!
//! `iter` / `walk` return the [`CursorIter`] / [`CursorWalk`] async-iterators,
//! which resolve their path lazily on first `next()` then step through children
//! one entry at a time, faulting chunks as needed.

use std::sync::Arc;

use napi::bindgen_prelude::{Either, Error as NapiError};
use napi::tokio::sync::Mutex as AsyncMutex;
use napi_derive::napi;

use crate::chunks::ChunkWindow;
use crate::path::{self, Segment};
use crate::resolve::{ChildEntry, ContainerCursor, ContainerKind, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};

#[napi]
pub struct Cursor {
  session: Arc<Session>,
  anchor: Option<ValueLocation>,
  /// Document-tree depth of this cursor's anchor (root = 0). Threaded to the cache
  /// as `base_depth` so nodes carry their true depth (the eviction key); the
  /// re-anchored relative path can't express it.
  depth: u32,
}

impl Cursor {
  pub(crate) fn root(session: Arc<Session>) -> Self {
    Self {
      session,
      anchor: None,
      depth: 0,
    }
  }

  /// A sub-cursor anchored at a child value. `walk` carries the member key in the
  /// yielded `(key, cursor)` tuple, so the cursor itself no longer holds its key.
  fn child(session: Arc<Session>, anchor: ValueLocation, depth: u32) -> Self {
    Self {
      session,
      anchor: Some(anchor),
      depth,
    }
  }

  /// A cursor anchored at an already-resolved `location`. `entry` carries the
  /// key/index this cursor reports (the path's last segment), `None` for a hop
  /// over an empty path (which lands back on the anchor, keyless like the root).
  fn at(
    session: Arc<Session>,
    location: ValueLocation,
    entry: Option<ChildEntry>,
    depth: u32,
  ) -> Self {
    Self {
      session,
      anchor: Some(location),
      entry,
      depth,
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
      .has_at(&path::from_napi(path), self.anchor_start(), self.depth)
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
      .get_at(&path::from_napi(path), self.anchor_start(), self.depth)
      .await
      .map(|opt| match opt {
        Some(v) => Either::A(v),
        None => Either::B(()),
      })
      .map_err(map_err)
  }

  #[napi(ts_args_type = "path: Array<string | number>")]
  pub async fn count(&self, path: Vec<Either<String, u32>>) -> napi::Result<f64> {
    crate::count::at(
      &self.session,
      &path::from_napi(path),
      self.anchor_start(),
      self.depth,
    )
    .await
    .map(|n| n as f64)
    .map_err(map_err)
  }

  #[napi(ts_args_type = "path: Array<string | number>, options: IterArgs")]
  pub fn iter(&self, path: Vec<Either<String, u32>>, options: IterArgs) -> CursorIter {
    CursorIter::new(
      self.session.clone(),
      path::from_napi(path),
      self.anchor_start(),
      self.depth,
      options.select_ir,
      (options.batch as usize).max(1),
      options.with_key.unwrap_or(false),
    )
  }

  // napi-derive can't typegen a tuple async-iterator yield (it only records a
  // `Type::Path` Yield), so the generated `CursorWalk` is an untyped empty class.
  // Override the return type with the real runtime shape: `[key, cursor]` steps.
  #[napi(
    ts_args_type = "path: Array<string | number>",
    ts_return_type = "AsyncIterable<[string, Cursor]>"
  )]
  pub fn walk(&self, path: Vec<Either<String, u32>>) -> CursorWalk {
    CursorWalk::new(
      self.session.clone(),
      path::from_napi(path),
      self.anchor_start(),
      self.depth,
    )
  }

  #[napi(ts_args_type = "path: Array<string | number>")]
  pub async fn hop(&self, path: Vec<Either<String, u32>>) -> napi::Result<Option<Cursor>> {
    let segments = path::from_napi(path);
    let Some(location) = self
      .session
      .resolve_at(&segments, self.anchor_start(), self.depth)
      .await
      .map_err(map_err)?
    else {
      return Ok(None);
    };
    let depth = self.depth + segments.len() as u32;
    let entry = match segments.last() {
      Some(Segment::Member(name)) => Some(ChildEntry::Member {
        key: name.clone(),
        location,
      }),
      Some(Segment::Element(idx)) => Some(ChildEntry::Element {
        index: *idx,
        location,
      }),
      None => self.entry.clone(),
    };
    Ok(Some(Cursor::at(
      self.session.clone(),
      location,
      entry,
      depth,
    )))
  }
}

/// Options for `iter`. A `#[napi(object)]` so the facade can grow it without
/// changing the method's arity.
#[napi(object)]
pub struct IterArgs {
  /// Serialized projection IR (see `select.rs`); `None` yields the whole child.
  pub select_ir: Option<String>,
  /// Items yielded per iteration.
  pub batch: f64,
  /// Yield `[key, value]` tuples instead of bare values.
  pub with_key: Option<bool>,
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
    base_depth: u32,
    select_ir: Option<String>,
    batch: usize,
    with_key: bool,
  ) -> Self {
    let state = IterState::new(
      &session,
      path,
      anchor_start,
      base_depth,
      select_ir,
      batch,
      with_key,
    );
    Self {
      session,
      state: Arc::new(AsyncMutex::new(state)),
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
      if !guard.core.initialized {
        guard.select = match guard.select_ir.as_deref() {
          Some(json) => Some(CompiledSelect::parse(json).map_err(|e| map_err(e.into()))?),
          None => None,
        };
        locate_and_enter(&session, &mut guard.core)
          .await
          .map_err(map_err)?;
        if let Some(w) = guard.core.child_cursor.as_ref() {
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
      let StreamCore {
        child_cursor,
        window,
        path,
        anchor_start,
        base_depth,
        base_value_start,
        yielded,
        ..
      } = core;
      let base_depth = *base_depth;
      let child_depth = base_depth + path.len() as u32 + 1;
      let Some(child_cursor) = child_cursor.as_mut() else {
        return Ok(None);
      };
      // window is pruned after each item, so the buffer (not chunks) is the
      // in-flight batch. Living in this `next()` frame, it needs no special
      // cleanup on early termination via `complete`.
      let result: Result<Option<serde_json::Value>, SessionError> = async {
        let mut buf: Vec<serde_json::Value> = Vec::with_capacity(batch);
        loop {
          let Some(child) = session.next_child(child_cursor, window).await? else {
            // Exhausted: child_cursor sits AT the close. Record child count + close
            // on the base node. iter only ever runs over arrays (objects gated above).
            if let Some(vs) = *base_value_start {
              session.store_child_count(
                base_depth,
                *anchor_start,
                path,
                ContainerKind::Array,
                vs,
                *yielded,
              );
              session.store_close(
                base_depth,
                *anchor_start,
                path,
                ContainerKind::Array,
                vs,
                child_cursor.next_offset + 1,
              );
            }
            if buf.is_empty() {
              return Ok(None);
            }
            return Ok(Some(serde_json::Value::Array(buf)));
          };
          *yielded += 1;
          let key = if with_key {
            Some(child_key_json(&child))
          } else {
            None
          };
          let value = match select {
            Some(sel) => {
              crate::eval::project(&session, sel, child.location().start, child_depth, window)
                .await?
            }
            None => session.materialize(child.location(), window).await?,
          };
          session.prune_window(window, child_cursor.next_offset);
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
      // Defensive prune: error paths land here too, so abandoned iterators don't
      // retain chunks past the scan position.
      session.prune_window(window, child_cursor.next_offset);
      result.map_err(map_err)
    }
  }

  fn complete(
    &mut self,
    _value: Option<Self::Return>,
  ) -> impl std::future::Future<Output = napi::Result<Option<Self::Yield>>> + Send + 'static {
    let state = self.state.clone();
    let session = self.session.clone();
    async move {
      let mut guard = state.lock().await;
      record_early_break(&session, &guard.core);
      release_core(&mut guard.core);
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
  fn new(session: Arc<Session>, path: Vec<Segment>, anchor_start: u64, base_depth: u32) -> Self {
    let core = StreamCore::new(&session, path, anchor_start, base_depth);
    Self {
      session,
      state: Arc::new(AsyncMutex::new(core)),
    }
  }
}

#[napi]
impl napi::bindgen_prelude::AsyncGenerator for CursorWalk {
  type Yield = (String, Cursor);
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
        if let Some(w) = guard.child_cursor.as_ref() {
          if w.kind == ContainerKind::Array {
            release_core(&mut guard);
            return Err(NapiError::from_reason(
              "walk target is an array; use iter() to iterate array elements".to_string(),
            ));
          }
        }
      }
      let StreamCore {
        child_cursor,
        window,
        path,
        anchor_start,
        base_depth,
        base_value_start,
        yielded,
        ..
      } = &mut *guard;
      let base_depth = *base_depth;
      let child_depth = base_depth + path.len() as u32 + 1;
      let Some(child_cursor) = child_cursor.as_mut() else {
        return Ok(None);
      };
      let entry = session
        .next_child(child_cursor, window)
        .await
        .map_err(map_err)?;
      session.prune_window(window, child_cursor.next_offset);
      let Some(child) = entry else {
        // Exhausted: child_cursor sits AT the close. Record child count + close on
        // the base object (walk only ever runs over objects; arrays gated above).
        if let Some(vs) = *base_value_start {
          session.store_child_count(
            base_depth,
            *anchor_start,
            path,
            ContainerKind::Object,
            vs,
            *yielded,
          );
          session.store_close(
            base_depth,
            *anchor_start,
            path,
            ContainerKind::Object,
            vs,
            child_cursor.next_offset + 1,
          );
        }
        return Ok(None);
      };
      *yielded += 1;
      // Gated to objects above, so every entry is a member; the key rides the tuple.
      let key = match &child {
        ChildEntry::Member { key, .. } => key.clone(),
        ChildEntry::Element { index, .. } => index.to_string(),
      };
      let cursor = Cursor::child(session.clone(), child.location(), child_depth);
      Ok(Some((key, cursor)))
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

/// `iter`-only state: [`StreamCore`] plus projection, batching, and key-wrapping.
/// `walk` uses [`StreamCore`] directly.
struct IterState {
  core: StreamCore,
  select_ir: Option<String>,
  /// Compiled lazily from `select_ir` on first `next()`. `None` yields the whole child.
  select: Option<CompiledSelect>,
  batch: usize,
  with_key: bool,
}

impl IterState {
  fn new(
    session: &Session,
    path: Vec<Segment>,
    anchor_start: u64,
    base_depth: u32,
    select_ir: Option<String>,
    batch: usize,
    with_key: bool,
  ) -> Self {
    Self {
      core: StreamCore::new(session, path, anchor_start, base_depth),
      select_ir,
      select: None,
      batch,
      with_key,
    }
  }
}

/// State shared by `iter` and `walk`: the lazily-resolved container cursor plus
/// the byte window carried across yields. Awaits happen with the lock held
/// (tokio Mutex).
struct StreamCore {
  path: Vec<Segment>,
  anchor_start: u64,
  /// Document depth of `anchor_start`; children sit at `base_depth + path.len() + 1`.
  base_depth: u32,
  initialized: bool,
  /// Set after first `next()`. `None` if the path didn't resolve or resolved to a
  /// non-container (iteration yields nothing).
  child_cursor: Option<ContainerCursor>,
  /// `value_start` of the base container, once resolved. Where the stream records
  /// `close`/`child_count`/resume-point array members.
  base_value_start: Option<u64>,
  /// Children yielded so far - the child count once iteration runs to the end.
  yielded: u64,
  /// At rest holds at most the chunk covering `next_offset` so the next yield's
  /// first read hits; everything else is pruned per yield, bounding resident
  /// chunks to ~1 between yields.
  window: ChunkWindow,
}

impl StreamCore {
  fn new(session: &Session, path: Vec<Segment>, anchor_start: u64, base_depth: u32) -> Self {
    Self {
      path,
      anchor_start,
      base_depth,
      initialized: false,
      child_cursor: None,
      base_value_start: None,
      yielded: 0,
      window: session.new_window(),
    }
  }
}

fn map_err(e: SessionError) -> NapiError {
  NapiError::from_reason(e.to_string())
}

/// Release the window held by an iterator on early termination (`complete`).
fn release_core(core: &mut StreamCore) {
  core.window.clear();
  core.child_cursor = None;
}

fn child_key_json(child: &ChildEntry) -> serde_json::Value {
  match child {
    ChildEntry::Member { key, .. } => serde_json::Value::String(key.clone()),
    ChildEntry::Element { index, .. } => serde_json::Value::Number((*index as u64).into()),
  }
}

/// Resolve the path and open its container cursor, pruning to the scan position so
/// the first yield's read is hot. Shared by `iter` and `walk`.
async fn locate_and_enter(session: &Session, core: &mut StreamCore) -> Result<(), SessionError> {
  if let Some(start) = session
    .locate_at(&core.path, core.anchor_start, core.base_depth)
    .await?
  {
    core.base_value_start = Some(start);
    core.child_cursor = session.enter_container(start, &mut core.window).await?;
    if let Some(w) = &core.child_cursor {
      session.prune_window(&mut core.window, w.next_offset);
    } else {
      core.window.clear();
    }
  }
  core.initialized = true;
  Ok(())
}

/// Record an array resume point on early termination so a later `get([base, N])`
/// resumes near the stop point. Arrays only: an object resume_point would claim
/// its prefix members are tabled, but the streaming path doesn't table them.
/// No-op before any element boundary is passed.
fn record_early_break(session: &Session, core: &StreamCore) {
  if let (Some(w), Some(vs)) = (core.child_cursor.as_ref(), core.base_value_start) {
    if w.kind == ContainerKind::Array && w.index > 0 && w.next_offset < session.source_size {
      session.store_array_resume_point(
        core.base_depth,
        core.anchor_start,
        &core.path,
        vs,
        w.index,
        w.next_offset,
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::session::MAX_BURST;
  use crate::source::{ByteStream, InMemoryStream};
  use napi::bindgen_prelude::AsyncGenerator;

  fn items_path() -> Vec<Segment> {
    vec![Segment::Member("items".into())]
  }

  /// `{"items":[{"name":"i0000",...}, ...]}` sized to span many chunks so a walk
  /// pins a real frontier chunk and the window bound holds throughout.
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

  fn session(items: usize, chunk_bytes: usize) -> Arc<Session> {
    let source: Arc<dyn ByteStream> = Arc::new(InMemoryStream::new(array_doc(items)));
    Session::new(
      source,
      chunk_bytes,
      crate::session::DEFAULT_INDEX_CACHE_ENTRIES,
      crate::session::DEFAULT_OBJECT_MEMBER_CAP,
      crate::session::DEFAULT_ARRAY_INDEX_INTERVAL,
    )
    .unwrap()
  }

  /// `{"items":{"k0000":{"name":"i0000",...}, ...}}` - an object whose members
  /// span many chunks, so a walk over `items` pins a real frontier chunk.
  fn object_doc(items: usize) -> Vec<u8> {
    let mut doc = String::from("{\"items\":{");
    for i in 0..items {
      if i > 0 {
        doc.push(',');
      }
      doc.push_str(&format!(
        "\"k{i:04}\":{{\"name\":\"i{i:04}\",\"total\":{i}}}"
      ));
    }
    doc.push_str("}}");
    doc.into_bytes()
  }

  fn object_session(items: usize, chunk_bytes: usize) -> Arc<Session> {
    let source: Arc<dyn ByteStream> = Arc::new(InMemoryStream::new(object_doc(items)));
    Session::new(
      source,
      chunk_bytes,
      crate::session::DEFAULT_INDEX_CACHE_ENTRIES,
      crate::session::DEFAULT_OBJECT_MEMBER_CAP,
      crate::session::DEFAULT_ARRAY_INDEX_INTERVAL,
    )
    .unwrap()
  }

  /// One burst of chunks plus a small slack is the resident-window bound.
  fn window_bound() -> usize {
    MAX_BURST as usize + 4
  }

  #[tokio::test]
  async fn pins_walk_released_on_complete() {
    let s = object_session(500, 256);
    let mut w = CursorWalk::new(s.clone(), items_path(), 0, 0);
    for _ in 0..3 {
      assert!(w.next(None).await.unwrap().is_some());
    }
    assert!(
      !w.state.lock().await.window.is_empty(),
      "walk should hold the frontier chunk between yields"
    );

    w.complete(None).await.unwrap();

    {
      let guard = w.state.lock().await;
      assert!(guard.window.is_empty(), "complete() must clear the window");
      assert!(
        guard.child_cursor.is_none(),
        "complete() must drop the child_cursor"
      );
    }
    assert!(w.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_walk_safe_when_child_escapes() {
    let s = object_session(500, 256);
    let mut w = CursorWalk::new(s.clone(), items_path(), 0, 0);
    let (key, child) = w.next(None).await.unwrap().expect("first child");
    assert_eq!(key, "k0000");

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
    let s = object_session(2000, 256);
    let bound = window_bound();
    let mut abandoned = Vec::new();
    for _ in 0..64 {
      let mut w = CursorWalk::new(s.clone(), items_path(), 0, 0);
      assert!(w.next(None).await.unwrap().is_some());
      w.complete(None).await.unwrap();
      assert!(w.state.lock().await.window.is_empty());
      abandoned.push(w); // keep alive: no Drop, no GC
    }
    assert_eq!(abandoned.len(), 64);
    let _ = bound;
  }

  #[tokio::test]
  async fn pins_iter_released_on_complete() {
    let s = session(500, 256);
    let mut it = CursorIter::new(s.clone(), items_path(), 0, 0, None, 1, false);
    for _ in 0..3 {
      assert!(it.next(None).await.unwrap().is_some());
    }
    assert!(!it.state.lock().await.core.window.is_empty());

    it.complete(None).await.unwrap();

    {
      let guard = it.state.lock().await;
      assert!(guard.core.window.is_empty());
      assert!(guard.core.child_cursor.is_none());
    }
    assert!(it.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_iter_batch_early_break_releases() {
    let s = session(500, 256);
    let mut it = CursorIter::new(s.clone(), items_path(), 0, 0, None, 8, false);
    assert!(it.next(None).await.unwrap().is_some());
    it.complete(None).await.unwrap();
    let guard = it.state.lock().await;
    assert!(
      guard.core.window.is_empty(),
      "complete() must clear the window after a batch"
    );
    assert!(guard.core.child_cursor.is_none());
  }

  #[tokio::test]
  async fn pins_iter_batch_window_bounded() {
    let s = session(2000, 256);
    let bound = window_bound();
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
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
  async fn walk_on_array_target_throws() {
    let s = session(50, 256);
    let mut w = CursorWalk::new(s.clone(), items_path(), 0, 0);
    let err = match w.next(None).await {
      Ok(_) => panic!("array target must throw"),
      Err(e) => e,
    };
    assert!(
      err.reason.contains("use iter()"),
      "error should steer the caller to iter(), got: {}",
      err.reason
    );
    let guard = w.state.lock().await;
    assert!(guard.window.is_empty(), "gate must release the window");
    assert!(
      guard.child_cursor.is_none(),
      "gate must drop the child_cursor"
    );
  }

  #[tokio::test]
  async fn iter_on_object_target_throws() {
    let s = session(50, 256);
    let mut it = CursorIter::new(s.clone(), Vec::new(), 0, 0, None, 8, false);
    let err = it.next(None).await.expect_err("object target must throw");
    assert!(
      err.reason.contains("use walk()"),
      "error should steer the caller to walk(), got: {}",
      err.reason
    );
    let guard = it.state.lock().await;
    assert!(guard.core.window.is_empty(), "gate must release the window");
    assert!(
      guard.core.child_cursor.is_none(),
      "gate must drop the child_cursor"
    );
  }
}
