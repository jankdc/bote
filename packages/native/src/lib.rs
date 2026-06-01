#![deny(clippy::all)]
#![feature(portable_simd)]

// I/O
mod chunks;
mod source;

// bitmaps
mod bitmap;
mod simd;

// traversal
mod walker;

// evaluation
mod path;
mod resolve;
mod select;

// structural-index cache (above resolve, below session)
mod cache;

// async orchestration
mod session;

// operations layered above the session
mod count;
mod eval;

mod cursor;

use std::sync::Arc;

use napi::bindgen_prelude::{Function, JsObjectValue, Object, Promise, Uint8Array};
use napi_derive::napi;

use crate::cursor::Cursor;
use crate::session::Session;
use crate::source::{JsByteStream, ReadArgs};

#[cfg(feature = "heap-profile")]
#[global_allocator]
static GLOBAL: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "heap-profile")]
static PROFILER: std::sync::Mutex<Option<dhat::Profiler>> = std::sync::Mutex::new(None);

/// Build a [`Cursor`] from a JS source object.
///
/// The `source` argument must be a JS object with:
///   - `size: number`                 total source size in bytes
///   - `read(args): Promise<Uint8Array>` `args.offset: number`, `args.length: number`;
///                                    JS resolves with a `Uint8Array` of bytes read
///                                    (its `.byteLength` is the actual count, `<= length`)
///   - `chunkBytes: number`           read granularity in bytes (whole, multiple of 64).
///                                    Required: the `@botejs/core` facade resolves the
///                                    per-source default before calling in.
#[napi]
pub fn open(
  #[napi(
    ts_arg_type = "{ size: number; chunkBytes: number; indexCacheEntries?: number; read: (args: ReadArgs) => Promise<Uint8Array> }"
  )]
  source: Object<'_>,
) -> napi::Result<Cursor> {
  let size = source.get_named_property::<f64>("size")?;
  if !size.is_finite() || size < 0.0 {
    return Err(napi::Error::from_reason(format!(
      "source.size must be a non-negative finite number, got {size}"
    )));
  }
  let read_fn: Function<ReadArgs, Promise<Uint8Array>> = source.get_named_property("read")?;
  let ts_read_fn = read_fn.build_threadsafe_function().weak::<true>().build()?;

  // chunkBytes is required and must be a whole positive number; reject anything
  // else outright rather than truncating (e.g. `0.5 as usize == 0`). The non-zero
  // multiple-of-64 rule is enforced by `ChunkReader::new`. The core facade fills
  // in the per-source default before calling, so there is no default here.
  let chunk_bytes = match source.get_named_property::<Option<f64>>("chunkBytes") {
    Ok(Some(n)) if n.is_finite() && n >= 1.0 && n.fract() == 0.0 && n <= usize::MAX as f64 => {
      n as usize
    }
    Ok(Some(n)) => {
      return Err(napi::Error::from_reason(format!(
        "chunkBytes must be a whole positive number of bytes, got {n}"
      )));
    }
    _ => {
      return Err(napi::Error::from_reason(
        "source.chunkBytes is required: a whole positive number of bytes".to_string(),
      ));
    }
  };

  // indexCacheEntries is the structural-index cache's children budget. Optional;
  // unlike chunkBytes it permits 0 (which disables the cache). Missing => the
  // default. The core facade also validates, but enforce the same hygiene here.
  let index_cache_budget = match source.get_named_property::<Option<f64>>("indexCacheEntries") {
    Ok(Some(n)) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= usize::MAX as f64 => {
      n as usize
    }
    Ok(Some(n)) => {
      return Err(napi::Error::from_reason(format!(
        "indexCacheEntries must be a whole non-negative number of entries, got {n}"
      )));
    }
    _ => crate::session::DEFAULT_INDEX_CACHE_ENTRIES,
  };

  let session = Session::new(
    Arc::new(JsByteStream::new(ts_read_fn, size as u64)),
    chunk_bytes,
    index_cache_budget,
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
