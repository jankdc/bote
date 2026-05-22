#![deny(clippy::all)]
#![feature(portable_simd)]

// I/O
mod cache;
mod source;

// bitmaps
mod bitmap;
mod simd;

// traversal
mod walker;

// evaluation
mod pointer;
mod resolve;

// async orchestration
mod session;

mod cursor;

use std::sync::Arc;

use napi::bindgen_prelude::{Function, JsObjectValue, Object, Promise};
use napi_derive::napi;

use crate::cache::CacheOptions;
use crate::cursor::Cursor;
use crate::session::Session;
use crate::source::{JsSource, ReadArgs, DEFAULT_SOURCE_CHUNK_BYTES};

const DEFAULT_MAX_RESIDENT_CHUNKS: u32 = 512;

#[cfg(feature = "heap-profile")]
#[global_allocator]
static GLOBAL: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "heap-profile")]
static PROFILER: std::sync::Mutex<Option<dhat::Profiler>> = std::sync::Mutex::new(None);

#[napi(object)]
pub struct BoteOptions {
  /// Maximum number of source chunks held resident at once. Each slot
  /// accounts for one chunk's bytes plus its bitmaps. Defaults to 512.
  pub max_resident_chunks: Option<f64>,
}

/// Build a [`Cursor`] from a JS source object.
///
/// The `source` argument must be a JS object with:
///   - `size: number`                 total source size in bytes
///   - `read(args): Promise<number>`  `args.offset: number`, `args.buf: Uint8Array`;
///                                    JS writes bytes into `args.buf` and resolves
///                                    with the number of bytes written
///   - `chunkBytes?: number`          preferred read granularity in bytes (multiple of 64, optional)
#[napi]
pub fn open(
  #[napi(
    ts_arg_type = "{ size: number; chunkBytes?: number; read: (args: ReadArgs) => Promise<number> }"
  )]
  source: Object<'_>,
  options: Option<BoteOptions>,
) -> napi::Result<Cursor> {
  let size = source.get_named_property::<f64>("size")?;
  if !size.is_finite() || size < 0.0 {
    return Err(napi::Error::from_reason(format!(
      "source.size must be a non-negative finite number, got {size}"
    )));
  }
  let read_fn: Function<ReadArgs, Promise<f64>> = source.get_named_property("read")?;
  let ts_read_fn = read_fn.build_threadsafe_function().weak::<true>().build()?;

  let chunk_size = match source.get_named_property::<Option<f64>>("chunkBytes") {
    Ok(Some(n)) if n.is_finite() && n > 0.0 => n as usize,
    _ => DEFAULT_SOURCE_CHUNK_BYTES,
  };

  let max_resident_chunks = options
    .as_ref()
    .and_then(|o| o.max_resident_chunks)
    .filter(|n| n.is_finite() && *n >= 1.0)
    .map(|n| n as u32)
    .unwrap_or(DEFAULT_MAX_RESIDENT_CHUNKS);

  let session = Session::new(
    Arc::new(JsSource::new(ts_read_fn, size as u64)),
    CacheOptions {
      chunk_size,
      max_resident_chunks,
    },
  )
  .map_err(|e| napi::Error::from_reason(e.to_string()))?;

  Ok(Cursor::root(session))
}

#[napi]
pub fn heap_profile_start(file_path: Option<String>) -> napi::Result<()> {
  #[cfg(feature = "heap-profile")]
  {
    let mut guard = PROFILER.lock().unwrap();
    if guard.is_some() {
      return Err(napi::Error::from_reason("heap profiler already started"));
    }
    let mut builder = dhat::Profiler::builder();
    if let Some(path) = file_path {
      builder = builder.file_name(path);
    }
    *guard = Some(builder.build());
    Ok(())
  }
  #[cfg(not(feature = "heap-profile"))]
  {
    let _ = file_path;
    Err(napi::Error::from_reason(
      "native built without `heap-profile` feature; rebuild with `--features heap-profile`",
    ))
  }
}

#[napi]
pub fn heap_profile_peak_bytes() -> napi::Result<f64> {
  #[cfg(feature = "heap-profile")]
  {
    let stats = dhat::HeapStats::get();
    Ok(stats.max_bytes as f64)
  }
  #[cfg(not(feature = "heap-profile"))]
  Err(napi::Error::from_reason(
    "native built without `heap-profile` feature; rebuild with `--features heap-profile`",
  ))
}

#[napi]
pub fn heap_profile_stop() -> napi::Result<()> {
  #[cfg(feature = "heap-profile")]
  {
    let mut guard = PROFILER.lock().unwrap();
    if guard.take().is_none() {
      return Err(napi::Error::from_reason("heap profiler was not started"));
    }
    Ok(())
  }
  #[cfg(not(feature = "heap-profile"))]
  Err(napi::Error::from_reason(
    "native built without `heap-profile` feature; rebuild with `--features heap-profile`",
  ))
}
