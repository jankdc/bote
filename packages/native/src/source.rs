//! The `Source` abstraction: a seekable, async byte stream.
//!
//! Every source advertises its total `size` and serves arbitrary byte ranges
//! via `read(offset, length)`. The parser only ever asks for chunk-aligned
//! ranges of a fixed length (typically 64 KiB); the final chunk may return
//! fewer bytes than requested when the range straddles end-of-source.

use async_trait::async_trait;
use bytes::Bytes;
use napi::bindgen_prelude::{Promise, Uint8Array};
use napi::threadsafe_function::ThreadsafeFunction;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SourceError {
  #[cfg(test)]
  #[error("read offset {offset} is past end of source (size {size})")]
  OutOfBounds { offset: u64, size: u64 },
  #[error("source I/O error: {0}")]
  Io(String),
}

#[async_trait]
pub trait Source: Send + Sync {
  /// Total length of the underlying byte stream.
  fn size(&self) -> u64;

  /// Read up to `length` bytes starting at `offset`. May return fewer bytes
  /// than requested only when the range extends past `size()`.
  async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError>;
}

/// In-memory source backed by an owned byte buffer. Test fixture only -
/// production callers feed bytes in via [`JsSource`].
#[cfg(test)]
pub struct InMemorySource {
  data: Bytes,
}

#[cfg(test)]
impl InMemorySource {
  pub fn new(data: impl Into<Bytes>) -> Self {
    Self { data: data.into() }
  }
}

#[cfg(test)]
#[async_trait]
impl Source for InMemorySource {
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

/// Arguments passed to the JS `read(args)` callback.
#[napi_derive::napi(object)]
pub struct ReadArgs {
  pub offset: f64,
  pub length: f64,
}

/// `ThreadsafeFunction` returned by `Function::build_threadsafe_function().weak::<true>().build()` -
/// `CalleeHandled = false`, so `call_async` takes the args directly (no
/// `Result` wrapper). We always pass a success value; JS-side rejections from
/// the returned `Promise` propagate via the inner `.await`.
///
/// JS returns `Promise<Uint8Array>` whose `.byteLength` is the bytes read.
///
/// `Weak = true` so the tsfn does not, on its own, keep the Node event loop
/// alive - a dormant Cursor reference shouldn't pin the process. Pending
/// `await`s on cursor operations are real Promises and still keep the loop
/// alive on their own merits.
pub type ReadFn =
  ThreadsafeFunction<ReadArgs, Promise<Uint8Array>, ReadArgs, napi::Status, false, true>;

/// Source backed by a JavaScript object exposing `size: number` and
/// `read(args): Promise<number>`. The JS function is held as a
/// [`ThreadsafeFunction`], which can be awaited from any tokio task.
pub struct JsSource {
  read_fn: Option<ReadFn>,
  size: u64,
}

impl JsSource {
  pub fn new(read_fn: ReadFn, size: u64) -> Self {
    Self {
      read_fn: Some(read_fn),
      size,
    }
  }
}

impl Drop for JsSource {
  fn drop(&mut self) {
    drop(self.read_fn.take());
  }
}

#[async_trait]
impl Source for JsSource {
  fn size(&self) -> u64 {
    self.size
  }

  async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError> {
    let read_fn = self
      .read_fn
      .as_ref()
      .ok_or_else(|| SourceError::Io("source already closed".into()))?;

    // Pull-from-JS protocol. We ask the JS callback for up to `length`
    // bytes at `offset`; it resolves with a `Uint8Array` whose
    // `.byteLength` is the bytes actually read (`<= length`). We copy
    // those bytes out, return `Bytes`, and the JS-side view becomes
    // immediately GC-eligible.
    //
    // The previous protocol pushed a Rust-owned buffer to JS via
    // `Uint8Array::with_external_data`, which on each call (a) registered
    // an external-memory entry against V8's `arrayBuffers` accounting and
    // (b) added a strong napi reference. The reference's drop ran on a
    // tokio worker thread and was queued through napi-rs's
    // `CUSTOM_GC_TSFN`; under a continuous scan the JS thread never
    // idled, the queue backed up, and `arrayBuffers` grew with bytes-read
    // (not with the live cache). Returning the buffer from JS instead -
    // V8-owned and V8-GC'd - sidesteps both pieces of machinery.
    let promise = read_fn
      .call_async(ReadArgs {
        offset: offset as f64,
        length: length as f64,
      })
      .await
      .map_err(|e| SourceError::Io(format!("threadsafe call failed: {e}")))?;
    let view: Uint8Array = promise
      .await
      .map_err(|e| SourceError::Io(format!("read() promise rejected: {e}")))?;

    let view_len = view.len();
    if view_len > length {
      return Err(SourceError::Io(format!(
        "read() returned {view_len} bytes for a {length}-byte request"
      )));
    }
    // Copy out so `Bytes` owns its allocation - the JS view stays valid
    // through this `.await` boundary but we shouldn't carry it further.
    Ok(Bytes::copy_from_slice(&view[..view_len]))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn in_memory_basic_read() {
    let src = InMemorySource::new(b"hello, world".to_vec());
    assert_eq!(src.size(), 12);
    let chunk = src.read(0, 5).await.unwrap();
    assert_eq!(&chunk[..], b"hello");
  }

  #[tokio::test]
  async fn in_memory_read_clipped_to_size() {
    let src = InMemorySource::new(b"abc".to_vec());
    let chunk = src.read(1, 100).await.unwrap();
    assert_eq!(&chunk[..], b"bc");
  }

  #[tokio::test]
  async fn in_memory_read_at_exact_end_returns_empty() {
    let src = InMemorySource::new(b"abc".to_vec());
    let chunk = src.read(3, 16).await.unwrap();
    assert!(chunk.is_empty());
  }

  #[tokio::test]
  async fn in_memory_read_past_end_is_error() {
    let src = InMemorySource::new(b"abc".to_vec());
    let err = src.read(4, 1).await.unwrap_err();
    assert!(matches!(
      err,
      SourceError::OutOfBounds { offset: 4, size: 3 }
    ));
  }
}
