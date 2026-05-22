//! The `Source` abstraction: a seekable, async byte stream.
//!
//! Every source advertises its total `size` and serves arbitrary byte ranges
//! via `read(offset, length)`. The parser only ever asks for chunk-aligned
//! ranges of a fixed length (typically 64 KiB); the final chunk may return
//! fewer bytes than requested when the range straddles end-of-source.

use std::sync::{Arc, Mutex};

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

/// Default chunk size when a `Source` doesn't advertise one. Matches the
/// historical hard-coded value; chosen to fit two windows of typical CPU
/// L1 and amortize one filesystem readahead.
pub const DEFAULT_SOURCE_CHUNK_BYTES: usize = 64 * 1024;

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
///
/// We funnel `(offset, buf)` through a single object so the napi-rs
/// `ThreadsafeFunction` only has to model a single argument - keeping the
/// type signature in Rust simple. The TS facade wraps the user's
/// `read(offset, buf)` API into this shape.
///
/// `buf` is an external `Uint8Array` view over a Rust-owned heap buffer of
/// `chunk_size` bytes; JS writes into it and resolves the returned promise
/// with the number of bytes written. JS must not retain a reference to `buf`
/// or read from it after that promise resolves - see [`JsSource::read`] for
/// the ownership protocol.
#[napi_derive::napi(object)]
pub struct ReadArgs {
  pub offset: f64,
  pub buf: Uint8Array,
}

/// `ThreadsafeFunction` returned by `Function::build_threadsafe_function().weak::<true>().build()` -
/// `CalleeHandled = false`, so `call_async` takes the args directly (no
/// `Result` wrapper). We always pass a success value; JS-side rejections from
/// the returned `Promise` propagate via the inner `.await`.
///
/// JS returns `Promise<number>` where the number is `bytesRead`. We accept it
/// as `f64` to match the rest of the napi boundary (sizes are passed as `f64`
/// everywhere - JS `Number` has no integer type).
///
/// `Weak = true` so the tsfn does not, on its own, keep the Node event loop
/// alive - a dormant Cursor reference shouldn't pin the process. Pending
/// `await`s on cursor operations are real Promises and still keep the loop
/// alive on their own merits.
pub type ReadFn = ThreadsafeFunction<ReadArgs, Promise<f64>, ReadArgs, napi::Status, false, true>;

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

    // Push-into-buffer protocol. We allocate the chunk buffer on the Rust
    // heap up-front and lend JS a `Uint8Array` view over it (external
    // arraybuffer). JS writes into the buffer and resolves with `bytesRead`;
    // we reclaim the buffer and return it as `Bytes` without an extra copy
    // at the V8 to Rust boundary.
    //
    // Ownership coordination: the buffer is parked in a shared
    // `Arc<Mutex<Option<Box<[u8]>>>>`. The `Uint8Array` finalizer captures a
    // second `Arc` clone, so:
    //   * On success: we `.take()` the buffer back out under the mutex; the
    //     V8 finalizer eventually fires (whenever V8 GCs the typed array)
    //     and finds `None`, freeing nothing.
    //   * On cancellation (this future dropped mid-await): our local `Arc`
    //     drops, but V8 still holds its clone via the external arraybuffer;
    //     when V8 GCs the typed array the finalizer drops the last `Arc`,
    //     which frees the `Box<[u8]>`.
    //
    // The JS contract - necessary for safety - is that the caller's `read`
    // implementation must not retain `buf` or read from it after the
    // returned promise resolves. The shipped factory functions
    // (`fromBuffer`, `fromFile`, `fromHttpRange`) honor this.
    let owner = Arc::new(Mutex::new(Some(vec![0u8; length].into_boxed_slice())));

    let (ptr, buf_len) = {
      let mut guard = owner.lock().unwrap();
      let slice = guard.as_mut().expect("buffer just installed");
      (slice.as_mut_ptr(), slice.len())
    };

    let owner_for_finalizer = Arc::clone(&owner);

    // SAFETY: `ptr` points into the `Box<[u8]>` parked in `owner`. While the
    // V8 typed array is live, either (a) we still hold `owner` and won't
    // free until either the JS contract releases the view *or* the
    // finalizer drops the last Arc, or (b) we've taken the buffer out via
    // `owner.lock().take()` and now own it directly, in which case JS must
    // not access it (the contract). `buf_len` is the exact allocated length.
    let buf = unsafe {
      Uint8Array::with_external_data(ptr, buf_len, move |_, _| {
        drop(owner_for_finalizer);
      })
    };

    let args = ReadArgs {
      offset: offset as f64,
      buf,
    };

    let promise = read_fn
      .call_async(args)
      .await
      .map_err(|e| SourceError::Io(format!("threadsafe call failed: {e}")))?;
    let bytes_read_f = promise
      .await
      .map_err(|e| SourceError::Io(format!("read() promise rejected: {e}")))?;

    if !bytes_read_f.is_finite() || bytes_read_f < 0.0 {
      return Err(SourceError::Io(format!(
        "read() returned invalid bytesRead {bytes_read_f}"
      )));
    }
    let bytes_read = (bytes_read_f as usize).min(buf_len);

    let boxed = owner
      .lock()
      .unwrap()
      .take()
      .expect("buffer parked under mutex was already taken");

    let mut vec = boxed.into_vec();
    vec.truncate(bytes_read);

    Ok(Bytes::from(vec))
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
