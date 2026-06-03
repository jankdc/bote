#![deny(clippy::all)]
#![feature(portable_simd)]

mod chunks;
mod source;

mod bitmap;
mod simd;

mod walker;

mod path;
mod resolve;
mod select;

mod cache;
mod session;

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

/// Build a [`Cursor`] from a JS source object:
///   - `size: number` total source size in bytes
///   - `read(args): Promise<Uint8Array>` (`args.offset`, `args.length`); resolved
///     `.byteLength` is the actual count read, `<= length`
///   - `chunkBytes: number` read granularity (whole, multiple of 64)
///   - `indexCacheEntries?: number` structural-index cache slot budget (0 disables; default 1024)
///   - `objectMemberCap?: number` max tabled members per object (0 disables; default unbounded)
///   - `arrayIndexInterval?: number` element stride between array members (0 disables; default 16)
#[napi]
pub fn open(
  #[napi(
    ts_arg_type = "{ size: number; chunkBytes: number; indexCacheEntries?: number; objectMemberCap?: number; arrayIndexInterval?: number; read: (args: ReadArgs) => Promise<Uint8Array> }"
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

  // reject non-whole/non-positive rather than truncating (`0.5 as usize == 0`).
  // multiple-of-64 enforced by `ChunkReader::new`.
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

  // cache knobs allow 0 (unlike chunkBytes): disables that dimension; missing => default.
  let index_cache_budget = whole_nonneg(
    &source,
    "indexCacheEntries",
    crate::session::DEFAULT_INDEX_CACHE_ENTRIES,
  )?;
  let object_member_cap = whole_nonneg(
    &source,
    "objectMemberCap",
    crate::session::DEFAULT_OBJECT_MEMBER_CAP,
  )?;
  let array_index_interval = whole_nonneg(
    &source,
    "arrayIndexInterval",
    crate::session::DEFAULT_ARRAY_INDEX_INTERVAL,
  )?;

  let session = Session::new(
    Arc::new(JsByteStream::new(ts_read_fn, size as u64)),
    chunk_bytes,
    index_cache_budget,
    object_member_cap,
    array_index_interval,
  )
  .map_err(|e| napi::Error::from_reason(e.to_string()))?;

  Ok(Cursor::root(session))
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

fn whole_nonneg(source: &Object<'_>, name: &str, default: usize) -> napi::Result<usize> {
  match source.get_named_property::<Option<f64>>(name) {
    Ok(Some(n)) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= usize::MAX as f64 => {
      Ok(n as usize)
    }
    Ok(Some(n)) => Err(napi::Error::from_reason(format!(
      "{name} must be a whole non-negative number, got {n}"
    ))),
    _ => Ok(default),
  }
}
