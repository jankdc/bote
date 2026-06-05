//! The `ByteStream` abstraction: a seekable, async byte stream serving byte
//! ranges via `read(offset, length)`.
//!
//! `ByteStream` corresponds to the TS `SourceReader` (the live stream), not the
//! TS `Source` (a factory that `open()`s a reader). The facade opens its
//! `Source`, then hands the resulting reader to `open()` as a `JsByteStream`.

use async_trait::async_trait;
use bytes::Bytes;
use napi::bindgen_prelude::{Promise, Uint8Array};
use napi::threadsafe_function::ThreadsafeFunction;
use thiserror::Error;

#[async_trait]
pub trait ByteStream: Send + Sync {
  fn size(&self) -> u64;

  /// Read up to `length` bytes at `offset`; returns fewer only when the range
  /// extends past `size()`.
  async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError>;
}

#[derive(Debug, Error)]
pub enum SourceError {
  #[cfg(test)]
  #[error("read offset {offset} is past end of source (size {size})")]
  OutOfBounds { offset: u64, size: u64 },
  #[error("source I/O error: {0}")]
  Io(String),
}

/// Arguments passed to the JS `read(args)` callback.
#[napi_derive::napi(object)]
pub struct ReadArgs {
  pub offset: f64,
  pub length: f64,
}

/// `CalleeHandled = false`: the call takes args directly. We invoke it via
/// `call_async_catch`, so a *synchronous* throw inside the JS `read` fn comes
/// back as `Err` instead of aborting the host process through
/// `napi_fatal_exception` (plain `call_async` would crash); an async rejection
/// of the returned `Promise` surfaces via its own `.await`. `Weak = true` so a
/// dormant Cursor's tsfn doesn't pin the Node event loop (pending `await`s keep
/// it alive).
pub type ReadFn =
  ThreadsafeFunction<ReadArgs, Promise<Uint8Array>, ReadArgs, napi::Status, false, true>;

/// ByteStream backed by a JS `read(args): Promise<Uint8Array>`, held as a
/// [`ThreadsafeFunction`] so it can be awaited from any tokio task.
pub struct JsByteStream {
  read_fn: Option<ReadFn>,
  size: u64,
}

impl JsByteStream {
  pub fn new(read_fn: ReadFn, size: u64) -> Self {
    Self {
      read_fn: Some(read_fn),
      size,
    }
  }
}

impl Drop for JsByteStream {
  fn drop(&mut self) {
    drop(self.read_fn.take());
  }
}

#[async_trait]
impl ByteStream for JsByteStream {
  fn size(&self) -> u64 {
    self.size
  }

  async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError> {
    let read_fn = self
      .read_fn
      .as_ref()
      .ok_or_else(|| SourceError::Io("source already closed".into()))?;

    // Pulling the buffer *from* JS (vs pushing a Rust-owned `with_external_data`
    // view *to* JS) keeps it V8-owned/V8-GC'd: a pushed view needs a strong napi
    // ref whose drop queues through `CUSTOM_GC_TSFN`, and under a continuous scan
    // the JS thread never idles, so that queue backs up and resident bytes grow
    // with bytes-read.
    let promise = read_fn
      .call_async_catch(ReadArgs {
        offset: offset as f64,
        length: length as f64,
      })
      .await
      .map_err(|e| SourceError::Io(format!("read() call failed: {e}")))?;
    let view: Uint8Array = promise
      .await
      .map_err(|e| SourceError::Io(format!("read() promise rejected: {e}")))?;

    let view_len = view.len();
    if view_len > length {
      return Err(SourceError::Io(format!(
        "read() returned {view_len} bytes for a {length}-byte request"
      )));
    }
    if view_len == 0 && length > 0 {
      return Err(SourceError::Io(format!(
        "read() returned 0 bytes for a {length}-byte request at offset {offset} (before declared EOF)"
      )));
    }
    // Copy out so `Bytes` owns its allocation; don't carry the JS view further.
    Ok(Bytes::copy_from_slice(&view[..view_len]))
  }
}

/// In-memory source backed by an owned buffer. Test fixture only; production
/// feeds bytes in via [`JsByteStream`].
#[cfg(test)]
pub struct InMemoryStream {
  data: Bytes,
}

#[cfg(test)]
impl InMemoryStream {
  pub fn new(data: impl Into<Bytes>) -> Self {
    Self { data: data.into() }
  }
}

#[cfg(test)]
#[async_trait]
impl ByteStream for InMemoryStream {
  fn size(&self) -> u64 {
    self.data.len() as u64
  }

  async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError> {
    let size = self.size();
    if offset > size {
      return Err(SourceError::OutOfBounds { offset, size });
    }
    let start = offset as usize;
    let end = start.saturating_add(length).min(self.data.len());
    Ok(self.data.slice(start..end))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn in_memory_basic_read() {
    let src = InMemoryStream::new(b"hello, world".to_vec());
    assert_eq!(src.size(), 12);
    let chunk = src.read(0, 5).await.unwrap();
    assert_eq!(&chunk[..], b"hello");
  }

  #[tokio::test]
  async fn in_memory_read_clipped_to_size() {
    let src = InMemoryStream::new(b"abc".to_vec());
    let chunk = src.read(1, 100).await.unwrap();
    assert_eq!(&chunk[..], b"bc");
  }

  #[tokio::test]
  async fn in_memory_read_at_exact_end_returns_empty() {
    let src = InMemoryStream::new(b"abc".to_vec());
    let chunk = src.read(3, 16).await.unwrap();
    assert!(chunk.is_empty());
  }

  #[tokio::test]
  async fn in_memory_read_past_end_is_error() {
    let src = InMemoryStream::new(b"abc".to_vec());
    let err = src.read(4, 1).await.unwrap_err();
    assert!(matches!(
      err,
      SourceError::OutOfBounds { offset: 4, size: 3 }
    ));
  }
}
