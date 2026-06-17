//! `Cursor` - the napi-exported class JavaScript holds.
//!
//! A Cursor wraps an [`Arc<Session>`] plus an optional anchor [`ValueLocation`].
//! The root Cursor (`open()`) has no anchor and resolves from byte 0; sub-cursors
//! reached by `hop` resolve paths relative to their anchor.
//!

use std::sync::Arc;

use napi::bindgen_prelude::{Either, Error as NapiError};
use napi::tokio::sync::Mutex as AsyncMutex;
use napi_derive::napi;

use crate::chunks::ReaderError;
use crate::path::{self, Segment};
use crate::resolve::ValueLocation;
use crate::session::{Session, SessionError};
use crate::source::{SourceError, SourceFaultCode};
use crate::stream::{BatchLimits, IterState};
use crate::walker::{JsonFaultCode, TraverseError};

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

  #[napi(ts_args_type = "path: Array<string | number>, options: IterArgs")]
  pub fn iter(&self, path: Vec<Either<String, u32>>, options: IterArgs) -> CursorIter {
    CursorIter::new(
      self.session.clone(),
      path::from_napi(path),
      self.anchor_start(),
      self.depth,
      options.select_ir,
      BatchLimits {
        max_count: (options.max_batch_count as usize).max(1),
        max_bytes: (options.max_batch_bytes as usize).max(1),
      },
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
  /// Upper bound on items per fetch. A fetch flushes when it reaches this many
  /// items or `max_batch_bytes`, whichever comes first.
  pub max_batch_count: f64,
  /// Upper bound on serialized bytes per fetch (clamped to at least 1). At least
  /// one item is always emitted even if it alone exceeds this, so the stream
  /// always makes progress.
  pub max_batch_bytes: f64,
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
    limits: BatchLimits,
    with_key: bool,
  ) -> Self {
    let state = IterState::new(
      &session,
      path,
      anchor_start,
      base_depth,
      select_ir,
      limits,
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
      if !guard.initialized {
        guard.initialize(&session).await.map_err(map_err)?;
      }
      guard.fill_batch(&session).await.map_err(map_err)
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
      guard.record_early_break(&session);
      guard.release();
      Ok(None)
    }
  }
}

fn map_err(e: SessionError) -> NapiError {
  NapiError::from_reason(serialize(&e))
}

fn serialize(e: &SessionError) -> String {
  match e {
    SessionError::Path(fault) => format!("bote:{}", fault.code()),
    SessionError::Traverse(TraverseError::Malformed(_)) => {
      format!("bote:{}", JsonFaultCode::MalformedJson.as_str())
    }
    SessionError::Traverse(TraverseError::UnexpectedEof(_)) => {
      format!("bote:{}", JsonFaultCode::UnexpectedEof.as_str())
    }
    SessionError::Reader(ReaderError::ByteStream(SourceError::Io(reason))) => {
      format!("bote:{}:{reason}", SourceFaultCode::SourceIo.as_str())
    }
    _ => e.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};

  use async_trait::async_trait;

  use super::*;
  use crate::session::MAX_BURST;
  use crate::source::{ByteStream, InMemoryStream, ReadOutcome, SourceError};
  use napi::bindgen_prelude::AsyncGenerator;

  fn items_path() -> Vec<Segment> {
    vec![Segment::Member("items".into())]
  }

  fn session(doc: Vec<u8>, chunk_bytes: usize) -> Arc<Session> {
    let source: Arc<dyn ByteStream> = Arc::new(InMemoryStream::new(doc));
    Session::new(
      source,
      chunk_bytes,
      crate::session::DEFAULT_INDEX_CACHE_ENTRIES,
      crate::session::DEFAULT_OBJECT_MEMBER_CAP,
      crate::session::DEFAULT_ARRAY_INDEX_INTERVAL,
    )
    .unwrap()
  }

  /// Wraps an [`InMemoryStream`] and counts `read` calls, so an abandoned scan's
  /// effect on chunk faulting is observable in-process.
  struct CountingStream {
    inner: InMemoryStream,
    reads: Arc<AtomicUsize>,
  }

  #[async_trait]
  impl ByteStream for CountingStream {
    fn size(&self) -> Option<u64> {
      self.inner.size()
    }
    async fn read(&self, offset: u64, length: usize) -> Result<ReadOutcome, SourceError> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      self.inner.read(offset, length).await
    }
  }

  fn counting_session(doc: Vec<u8>, chunk_bytes: usize) -> (Arc<Session>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn ByteStream> = Arc::new(CountingStream {
      inner: InMemoryStream::new(doc),
      reads: reads.clone(),
    });
    let s = Session::new(
      source,
      chunk_bytes,
      crate::session::DEFAULT_INDEX_CACHE_ENTRIES,
      crate::session::DEFAULT_OBJECT_MEMBER_CAP,
      crate::session::DEFAULT_ARRAY_INDEX_INTERVAL,
    )
    .unwrap();
    (s, reads)
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

  /// One burst of chunks plus a small slack is the resident-window bound.
  fn window_bound() -> usize {
    MAX_BURST as usize + 4
  }

  #[tokio::test]
  async fn iter_withkey_reencodes_escaped_object_key() {
    let doc = br#"{"items":{"a\"b":{"name":"x"}}}"#.to_vec();
    let s = session(doc, 256);
    let mut it = CursorIter::new(s, items_path(), 0, 0, None, BatchLimits::count(1), true);
    let batch = it.next(None).await.unwrap().expect("one member");
    assert_eq!(batch, r#"[["a\"b",{"name":"x"}]]"#);
  }

  #[tokio::test]
  async fn pins_iter_released_on_complete() {
    let s = session(array_doc(500), 256);
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
      0,
      None,
      BatchLimits::count(1),
      false,
    );
    for _ in 0..3 {
      assert!(it.next(None).await.unwrap().is_some());
    }
    assert!(!it.state.lock().await.window.is_empty());

    it.complete(None).await.unwrap();

    {
      let guard = it.state.lock().await;
      assert!(guard.window.is_empty());
      assert!(guard.child_cursor.is_none());
    }
    assert!(it.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_iter_batch_early_break_releases() {
    let s = session(array_doc(500), 256);
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
      0,
      None,
      BatchLimits::count(8),
      false,
    );
    assert!(it.next(None).await.unwrap().is_some());
    it.complete(None).await.unwrap();
    let guard = it.state.lock().await;
    assert!(
      guard.window.is_empty(),
      "complete() must clear the window after a batch"
    );
    assert!(guard.child_cursor.is_none());
  }

  #[tokio::test]
  async fn pins_iter_complete_stops_reads_far_below_full_walk() {
    let (full, full_reads) = counting_session(array_doc(5000), 256);
    let mut walk = CursorIter::new(full, items_path(), 0, 0, None, BatchLimits::count(1), false);
    while walk.next(None).await.unwrap().is_some() {}
    let full_n = full_reads.load(Ordering::Relaxed);

    let (partial, partial_reads) = counting_session(array_doc(5000), 256);
    let mut it = CursorIter::new(
      partial,
      items_path(),
      0,
      0,
      None,
      BatchLimits::count(1),
      false,
    );
    for _ in 0..3 {
      assert!(it.next(None).await.unwrap().is_some());
    }
    it.complete(None).await.unwrap();
    let after_complete = partial_reads.load(Ordering::Relaxed);

    // A completed iter is exhausted and faults nothing more.
    assert!(it.next(None).await.unwrap().is_none());
    assert_eq!(
      partial_reads.load(Ordering::Relaxed),
      after_complete,
      "complete() must not fault further chunks"
    );
    assert!(
      after_complete < full_n / 10,
      "abandoned scan faulted {after_complete} reads; should be far below the full walk's {full_n}"
    );
  }

  #[tokio::test]
  async fn pins_iter_batch_window_bounded() {
    let s = session(array_doc(2000), 256);
    let bound = window_bound();
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
      0,
      Some(r#"{"one":["total"]}"#.to_string()),
      BatchLimits::count(64),
      false,
    );
    while it.next(None).await.unwrap().is_some() {
      let len = it.state.lock().await.window.len();
      assert!(
        len <= bound,
        "window held {len} chunks while batching (bound {bound})"
      );
    }
  }

  #[tokio::test]
  async fn pins_iter_object_released_on_complete() {
    let s = session(object_doc(500), 256);
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
      0,
      None,
      BatchLimits::count(1),
      true,
    );
    for _ in 0..3 {
      assert!(
        it.next(None).await.unwrap().is_some(),
        "object iter should yield member tuples"
      );
    }
    assert!(
      !it.state.lock().await.window.is_empty(),
      "iter should hold the frontier chunk between yields"
    );

    it.complete(None).await.unwrap();

    {
      let guard = it.state.lock().await;
      assert!(guard.window.is_empty(), "complete() must clear the window");
      assert!(
        guard.child_cursor.is_none(),
        "complete() must drop the child_cursor"
      );
    }
    assert!(it.next(None).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn pins_iter_object_withkey_window_bounded() {
    let s = session(object_doc(2000), 256);
    let bound = window_bound();
    let mut it = CursorIter::new(
      s.clone(),
      items_path(),
      0,
      0,
      None,
      BatchLimits::count(64),
      true,
    );
    while it.next(None).await.unwrap().is_some() {
      let len = it.state.lock().await.window.len();
      assert!(
        len <= bound,
        "object withKey iter held {len} chunks while batching (bound {bound})"
      );
    }
  }

  #[tokio::test]
  async fn pins_iter_object_abandoned_windows_empty() {
    let s = session(object_doc(2000), 256);
    let mut abandoned = Vec::new();
    for _ in 0..64 {
      let mut it = CursorIter::new(
        s.clone(),
        items_path(),
        0,
        0,
        None,
        BatchLimits::count(1),
        true,
      );
      assert!(it.next(None).await.unwrap().is_some());
      it.complete(None).await.unwrap();
      assert!(it.state.lock().await.window.is_empty());
      abandoned.push(it); // keep alive: no Drop, no GC
    }
    assert_eq!(abandoned.len(), 64);
  }
}
