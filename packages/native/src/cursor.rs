//! `Cursor` - the napi-exported class JavaScript holds.
//!
//! A Cursor wraps an [`Arc<Session>`] plus an optional anchor [`ValueLocation`].
//! The root Cursor (`open()`) has no anchor and resolves from byte 0; sub-cursors
//! reached by `hop` resolve paths relative to their anchor.
//!
//! `iter` returns the [`CursorIter`] async-iterator, which resolves its path
//! lazily on first `next()` then steps through children one entry at a time,
//! faulting chunks as needed. It works over either container kind: array elements
//! or object members.

use std::sync::Arc;

use napi::bindgen_prelude::{Either, Error as NapiError};
use napi::tokio::sync::Mutex as AsyncMutex;
use napi_derive::napi;

use crate::chunks::ChunkWindow;
use crate::path::{self, Segment};
use crate::resolve::{ChildEntry, ContainerCursor, ContainerKind, PathFault, ValueLocation};
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

  /// A cursor anchored at an already-resolved `location` reached by `hop`.
  fn at(session: Arc<Session>, location: ValueLocation, depth: u32) -> Self {
    Self {
      session,
      anchor: Some(location),
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
    ts_return_type = "Promise<string | undefined>"
  )]
  pub async fn get(&self, path: Vec<Either<String, u32>>) -> napi::Result<Either<String, ()>> {
    self
      .session
      .get_at(&path::from_napi(path), self.anchor_start(), self.depth)
      .await
      .map(|opt| match opt {
        // SAFETY: bytes are a slice of a UTF-8 JSON source.
        Some(b) => Either::A(unsafe { String::from_utf8_unchecked(b) }),
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
    Ok(Some(Cursor::at(self.session.clone(), location, depth)))
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
  /// Stitch each yield as a `[key, value]` tuple instead of a bare value. The key
  /// is the member name for objects (a JSON string) and the element index for
  /// arrays (a JSON number).
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
  type Yield = String;
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
      let kind = child_cursor.kind;
      // window is pruned after each item, so the buffer (not chunks) is the
      // in-flight batch. Living in this `next()` frame, it needs no special
      // cleanup on early termination via `complete`.
      let result: Result<Option<String>, SessionError> = async {
        // The batch accumulates as JSON array text: `[`, items comma-joined, `]`.
        // With `with_key` each item is itself a `[key,value]` sub-array.
        let mut buf: Vec<u8> = Vec::new();
        buf.push(b'[');
        let mut count = 0usize;
        loop {
          let Some(child) = session.next_child(child_cursor, window).await? else {
            // Exhausted: child_cursor sits AT the close. Record child count + close
            // on the base node, keyed on the entered container kind.
            if let Some(vs) = *base_value_start {
              session.store_child_count(base_depth, *anchor_start, path, kind, vs, *yielded);
              session.store_close(
                base_depth,
                *anchor_start,
                path,
                kind,
                vs,
                child_cursor.next_offset + 1,
              );
            }
            if count == 0 {
              return Ok(None);
            }
            buf.push(b']');
            // SAFETY: stitched from valid-UTF-8 JSON source slices and ASCII punctuation.
            return Ok(Some(unsafe { String::from_utf8_unchecked(buf) }));
          };
          *yielded += 1;
          let loc = child.location();
          let value = match select {
            Some(sel) => {
              crate::eval::project(&session, sel, loc.start, child_depth, window).await?
            }
            None => session.materialize(loc, window).await?,
          };
          session.prune_window(window, child_cursor.next_offset);
          if count > 0 {
            buf.push(b',');
          }
          if with_key {
            buf.push(b'[');
            match &child {
              // Array key: the bare numeric index.
              ChildEntry::Element { index, .. } => {
                buf.extend_from_slice(index.to_string().as_bytes())
              }
              // Object key: re-encode the decoded name as a JSON string (escapes
              // only shrink, so this never mis-renders a source key).
              ChildEntry::Member { key, .. } => serde_json::to_writer(&mut buf, key)
                .expect("serializing a JSON string key is infallible"),
            }
            buf.push(b',');
            buf.extend_from_slice(&value);
            buf.push(b']');
          } else {
            buf.extend_from_slice(&value);
          }
          count += 1;
          if count >= batch {
            buf.push(b']');
            // SAFETY: as above.
            return Ok(Some(unsafe { String::from_utf8_unchecked(buf) }));
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

/// `iter` state: [`StreamCore`] plus projection, batching, and key-wrapping.
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

/// Resolve the path and open its container cursor, pruning to the scan position so
/// the first yield's read is hot.
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
      core.initialized = true;
      return Err(SessionError::Path(PathFault::ScalarTarget));
    }
  }
  core.initialized = true;
  Ok(())
}

/// Record an array resume point on early termination so a later `get([base, N])`
/// resumes near the stop point. Arrays only: an object resume_point would claim
/// its prefix members are tabled, but the streaming path doesn't table them, so
/// for object iters this is a no-op. No-op before any element boundary is passed.
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

  /// `{"items":[{"name":"i0000",...}, ...]}` sized to span many chunks so an iter
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
  /// span many chunks, so an iter over `items` pins a real frontier chunk.
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
  async fn pins_iter_object_released_on_complete() {
    let s = object_session(500, 256);
    let mut it = CursorIter::new(s.clone(), items_path(), 0, 0, None, 1, true);
    for _ in 0..3 {
      assert!(
        it.next(None).await.unwrap().is_some(),
        "object iter should yield member tuples"
      );
    }
    assert!(
      !it.state.lock().await.core.window.is_empty(),
      "iter should hold the frontier chunk between yields"
    );

    it.complete(None).await.unwrap();

    {
      let guard = it.state.lock().await;
      assert!(
        guard.core.window.is_empty(),
        "complete() must clear the window"
      );
      assert!(
        guard.core.child_cursor.is_none(),
        "complete() must drop the child_cursor"
      );
    }
    assert!(it.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_iter_object_withkey_window_bounded() {
    let s = object_session(2000, 256);
    let bound = window_bound();
    let mut it = CursorIter::new(s.clone(), items_path(), 0, 0, None, 64, true);
    while it.next(None).await.unwrap().is_some() {
      let len = it.state.lock().await.core.window.len();
      assert!(
        len <= bound,
        "object withKey iter held {len} chunks while batching (bound {bound})"
      );
    }
  }

  #[tokio::test]
  async fn pins_iter_object_abandoned_stay_bounded() {
    let s = object_session(2000, 256);
    let bound = window_bound();
    let mut abandoned = Vec::new();
    for _ in 0..64 {
      let mut it = CursorIter::new(s.clone(), items_path(), 0, 0, None, 1, true);
      assert!(it.next(None).await.unwrap().is_some());
      it.complete(None).await.unwrap();
      assert!(it.state.lock().await.core.window.is_empty());
      abandoned.push(it); // keep alive: no Drop, no GC
    }
    assert_eq!(abandoned.len(), 64);
    let _ = bound;
  }
}
