//! The `ByteStream` abstraction: a seekable, async byte stream serving byte
//! ranges via `read(offset, length)`.
//!
//! `ByteStream` corresponds to the TS `SourceReader` (the live stream), not the
//! TS `SeekableSource` (a factory that `open()`s a reader). The facade opens its
//! `SeekableSource`, then hands the resulting reader to `open()` as a `JsByteStream`.

use async_trait::async_trait;
use bytes::Bytes;
use napi::bindgen_prelude::{Promise, Uint8Array};
use napi::threadsafe_function::ThreadsafeFunction;
use thiserror::Error;

#[async_trait]
pub trait ByteStream: Send + Sync {
  fn size(&self) -> Option<u64>;
  async fn read(&self, offset: u64, length: usize) -> Result<ReadOutcome, SourceError>;
}

#[derive(Debug)]
pub struct ReadOutcome {
  pub bytes: Bytes,
  pub eof: bool,
}

#[derive(Debug, Error)]
pub enum SourceError {
  #[cfg(test)]
  #[error("read offset {offset} is past end of source (size {size})")]
  OutOfBounds { offset: u64, size: u64 },
  #[error("source I/O error: {0}")]
  Io(String),
}

#[napi_derive::napi(string_enum = "snake_case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFaultCode {
  SourceIo,
}

impl SourceFaultCode {
  pub(crate) fn as_str(self) -> &'static str {
    match self {
      Self::SourceIo => "source_io",
    }
  }
}

/// Arguments passed to the JS `read(args)` callback.
#[napi_derive::napi(object)]
pub struct JsReadArgs {
  pub offset: f64,
  pub length: f64,
}

/// Value the JS `read(args)` callback resolves to: the bytes plus an
/// end-of-stream flag. `eof` lets an unknown-size source declare where the data
/// ends; for a sized source it's ignored (the engine already knows the end).
#[napi_derive::napi(object)]
pub struct JsReadResult {
  pub data: Uint8Array,
  pub eof: bool,
}

/// `CalleeHandled = false`: the call takes args directly. We invoke it via
/// `call_async_catch`, so a *synchronous* throw inside the JS `read` fn comes
/// back as `Err` instead of aborting the host process through
/// `napi_fatal_exception` (plain `call_async` would crash); an async rejection
/// of the returned `Promise` surfaces via its own `.await`. `Weak = true` so a
/// dormant Cursor's tsfn doesn't pin the Node event loop (pending `await`s keep
/// it alive).
pub type ReadFn =
  ThreadsafeFunction<JsReadArgs, Promise<JsReadResult>, JsReadArgs, napi::Status, false, true>;

/// ByteStream backed by a JS `read(args): Promise<ReadResult>`, held as a
/// [`ThreadsafeFunction`] so it can be awaited from any tokio task. `size` is
/// `None` for a forward source whose end is discovered from `read`'s `eof`.
pub struct JsByteStream {
  read_fn: ReadFn,
  size: Option<u64>,
}

impl JsByteStream {
  pub fn new(read_fn: ReadFn, size: Option<u64>) -> Self {
    Self { read_fn, size }
  }
}

#[async_trait]
impl ByteStream for JsByteStream {
  fn size(&self) -> Option<u64> {
    self.size
  }

  async fn read(&self, offset: u64, length: usize) -> Result<ReadOutcome, SourceError> {
    // Pulling the buffer *from* JS (vs pushing a Rust-owned `with_external_data`
    // view *to* JS) keeps it V8-owned/V8-GC'd: a pushed view needs a strong napi
    // ref whose drop queues through `CUSTOM_GC_TSFN`, and under a continuous scan
    // the JS thread never idles, so that queue backs up and resident bytes grow
    // with bytes-read.
    let promise = self
      .read_fn
      .call_async_catch(JsReadArgs {
        offset: offset as f64,
        length: length as f64,
      })
      .await
      .map_err(|e| SourceError::Io(format!("read() call failed: {e}")))?;
    let result: JsReadResult = promise
      .await
      .map_err(|e| SourceError::Io(format!("read() promise rejected: {e}")))?;

    let view = result.data;
    let view_len = view.len();
    if view_len > length {
      return Err(SourceError::Io(format!(
        "read() returned {view_len} bytes for a {length}-byte request"
      )));
    }
    // A non-eof read must make progress; 0 bytes without `eof` is a stuck source,
    // not the end. (With `eof`, 0 bytes legitimately means end-of-stream.)
    if view_len == 0 && length > 0 && !result.eof {
      return Err(SourceError::Io(format!(
        "read() returned 0 bytes for a {length}-byte request at offset {offset} without signaling eof"
      )));
    }
    // Copy out so `Bytes` owns its allocation; don't carry the JS view further.
    Ok(ReadOutcome {
      bytes: Bytes::copy_from_slice(&view[..view_len]),
      eof: result.eof,
    })
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
  fn size(&self) -> Option<u64> {
    Some(self.data.len() as u64)
  }

  async fn read(&self, offset: u64, length: usize) -> Result<ReadOutcome, SourceError> {
    let size = self.data.len() as u64;
    if offset > size {
      return Err(SourceError::OutOfBounds { offset, size });
    }
    let start = offset as usize;
    let end = start.saturating_add(length).min(self.data.len());
    Ok(ReadOutcome {
      bytes: self.data.slice(start..end),
      eof: end as u64 >= size,
    })
  }
}

#[cfg(test)]
pub struct ForwardStream {
  data: Bytes,
}

#[cfg(test)]
impl ForwardStream {
  pub fn new(data: impl Into<Bytes>) -> Self {
    Self { data: data.into() }
  }
}

#[cfg(test)]
#[async_trait]
impl ByteStream for ForwardStream {
  fn size(&self) -> Option<u64> {
    None
  }

  async fn read(&self, offset: u64, length: usize) -> Result<ReadOutcome, SourceError> {
    let size = self.data.len() as u64;
    if offset > size {
      return Err(SourceError::OutOfBounds { offset, size });
    }
    let start = offset as usize;
    let end = start.saturating_add(length).min(self.data.len());
    Ok(ReadOutcome {
      bytes: self.data.slice(start..end),
      eof: end as u64 >= size,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn in_memory_basic_read() {
    let src = InMemoryStream::new(b"hello, world".to_vec());
    assert_eq!(src.size(), Some(12));
    let chunk = src.read(0, 5).await.unwrap();
    assert_eq!(&chunk.bytes[..], b"hello");
    assert!(!chunk.eof, "a read short of the end is not eof");
  }

  #[tokio::test]
  async fn in_memory_read_clipped_to_size() {
    let src = InMemoryStream::new(b"abc".to_vec());
    let chunk = src.read(1, 100).await.unwrap();
    assert_eq!(&chunk.bytes[..], b"bc");
    assert!(chunk.eof, "a read reaching the end is eof");
  }

  #[tokio::test]
  async fn in_memory_read_at_exact_end_returns_empty() {
    let src = InMemoryStream::new(b"abc".to_vec());
    let chunk = src.read(3, 16).await.unwrap();
    assert!(chunk.bytes.is_empty());
    assert!(chunk.eof);
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

  #[tokio::test]
  async fn forward_stream_reports_no_size_and_eof_at_end() {
    let src = ForwardStream::new(b"abcde".to_vec());
    assert_eq!(src.size(), None, "a forward source has no known size");
    let mid = src.read(0, 3).await.unwrap();
    assert_eq!(&mid.bytes[..], b"abc");
    assert!(!mid.eof);
    let tail = src.read(3, 100).await.unwrap();
    assert_eq!(&tail.bytes[..], b"de");
    assert!(tail.eof, "the read reaching the end declares eof");
  }
}
