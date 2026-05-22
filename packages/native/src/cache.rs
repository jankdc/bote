//! Chunk cache with LRU eviction and RAII pinning.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use thiserror::Error;

use crate::source::{Source, SourceError};

const MIN_CHUNK_SIZE: usize = 64;

/// Multiplier applied to `max_resident_chunks x chunk_size` to derive the
/// total-bytes ceiling. The factor of 2 covers worst-case bitmap growth:
/// one `in_string` bitmap (~chunk/8 bytes) plus up to six structural
/// bitmaps (each ~chunk/8 bytes) =~ chunk x 7/8, comfortably below 1x
/// chunk per slot of bitmap overhead. The 2x total leaves room for
/// padding without ever causing the cache to evict purely on bytes for
/// well-behaved workloads.
const CEILING_FACTOR: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct CacheOptions {
  pub chunk_size: usize,
  pub max_resident_chunks: u32,
}

#[derive(Debug, Error)]
pub enum CacheError {
  #[error("chunk size must be a non-zero multiple of {MIN_CHUNK_SIZE}, got {0}")]
  InvalidChunkSize(usize),
  #[error("max_resident_chunks must be non-zero")]
  ZeroMaxResidentChunks,
  #[error(transparent)]
  Source(#[from] SourceError),
}

pub struct ChunkCache {
  source: Arc<dyn Source>,
  chunk_size: usize,
  max_resident_chunks: u32,
  derived_ceiling_bytes: usize,
  /// Shared with the session's `BitmapStore` so the eviction loop can read
  /// bitmap memory without re-entering the bitmap store's lock.
  bitmap_bytes: Arc<AtomicUsize>,
  inner: Mutex<CacheInner>,
}

impl std::fmt::Debug for ChunkCache {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChunkCache")
      .field("chunk_size", &self.chunk_size)
      .field("max_resident_chunks", &self.max_resident_chunks)
      .field("resident_chunks", &self.resident_chunks())
      .field("resident_bytes", &self.resident_bytes())
      .finish()
  }
}

struct CacheInner {
  chunks: HashMap<u64, Chunk>,
  tick: u64,
  resident_bytes: usize,
  /// Offsets of chunks evicted since the session last drained them.
  /// Drained by [`ChunkCache::drain_evicted`] so the session can in turn
  /// drop the corresponding entries from its bitmap store. Without this,
  /// the bitmaps grow unbounded for documents larger than the cap.
  evicted_since_drain: Vec<u64>,
}

struct Chunk {
  data: Bytes,
  pins: u32,
  last_access: u64,
}

impl ChunkCache {
  pub fn new(source: Arc<dyn Source>, options: CacheOptions) -> Result<Arc<Self>, CacheError> {
    if options.chunk_size == 0 || !options.chunk_size.is_multiple_of(MIN_CHUNK_SIZE) {
      return Err(CacheError::InvalidChunkSize(options.chunk_size));
    }
    if options.max_resident_chunks == 0 {
      return Err(CacheError::ZeroMaxResidentChunks);
    }
    let derived_ceiling_bytes = (options.max_resident_chunks as usize)
      .saturating_mul(options.chunk_size)
      .saturating_mul(CEILING_FACTOR);
    Ok(Arc::new(Self {
      source,
      chunk_size: options.chunk_size,
      max_resident_chunks: options.max_resident_chunks,
      derived_ceiling_bytes,
      bitmap_bytes: Arc::new(AtomicUsize::new(0)),
      inner: Mutex::new(CacheInner {
        chunks: HashMap::new(),
        tick: 0,
        resident_bytes: 0,
        evicted_since_drain: Vec::new(),
      }),
    }))
  }

  pub fn resident_bytes(&self) -> usize {
    self.inner.lock().unwrap().resident_bytes
  }

  pub fn resident_chunks(&self) -> usize {
    self.inner.lock().unwrap().chunks.len()
  }

  pub fn bitmap_bytes_handle(&self) -> Arc<AtomicUsize> {
    self.bitmap_bytes.clone()
  }

  /// Fetch the chunk starting at `chunk_offset` (which must be chunk-aligned).
  /// On a hit, marks the chunk as most-recently-used and pins it. On a miss,
  /// reads from the underlying source while no lock is held, then inserts
  /// and pins; evicts older unpinned chunks if the new resident size exceeds
  /// the budget.
  pub async fn fetch(self: &Arc<Self>, chunk_offset: u64) -> Result<ChunkRef, CacheError> {
    debug_assert!(chunk_offset.is_multiple_of(self.chunk_size as u64));

    {
      let mut inner = self.inner.lock().unwrap();
      if let Some(data) = touch_and_pin(&mut inner, chunk_offset) {
        return Ok(ChunkRef::new(self.clone(), chunk_offset, data));
      }
    }

    let data = self.source.read(chunk_offset, self.chunk_size).await?;

    let mut inner = self.inner.lock().unwrap();
    // Another task may have populated the same chunk while we were reading;
    // prefer the existing entry and let our just-read copy drop.
    if let Some(existing) = touch_and_pin(&mut inner, chunk_offset) {
      return Ok(ChunkRef::new(self.clone(), chunk_offset, existing));
    }

    inner.tick += 1;
    let tick = inner.tick;
    inner.resident_bytes += data.len();
    inner.chunks.insert(
      chunk_offset,
      Chunk {
        data: data.clone(),
        last_access: tick,
        pins: 1,
      },
    );
    self.evict_to_caps(&mut inner);
    Ok(ChunkRef::new(self.clone(), chunk_offset, data))
  }

  /// Take the offsets of chunks evicted since the last drain. The session
  /// uses this to keep its `BitmapStore` in lockstep with the byte cache.
  /// otherwise bitmaps grow unbounded for documents larger than the budget.
  pub fn drain_evicted(&self) -> Vec<u64> {
    let mut inner = self.inner.lock().unwrap();
    std::mem::take(&mut inner.evicted_since_drain)
  }

  fn unpin(&self, chunk_offset: u64) {
    let mut inner = self.inner.lock().unwrap();
    if let Some(chunk) = inner.chunks.get_mut(&chunk_offset) {
      chunk.pins = chunk.pins.saturating_sub(1);
    }

    // Eviction is triggered both on insert and on unpin. A long-running
    // query can pin many chunks at once, pushing the cache above its
    // caps; the over-cap excess is reclaimed as those pins are released
    // (typically on query completion). Without this, freed pins would
    // accumulate indefinitely until the next insert.
    self.evict_to_caps(&mut inner);
  }

  /// Evict LRU unpinned chunks until both caps are satisfied:
  /// 1. `chunks.len() <= max_resident_chunks` (primary slot cap)
  /// 2. `resident_bytes + bitmap_bytes <= derived_ceiling_bytes`
  ///    (defends total RSS against unbounded bitmap growth).
  fn evict_to_caps(&self, inner: &mut CacheInner) {
    loop {
      let over_slots = inner.chunks.len() > self.max_resident_chunks as usize;
      let total = inner
        .resident_bytes
        .saturating_add(self.bitmap_bytes.load(Ordering::Relaxed));
      let over_bytes = total > self.derived_ceiling_bytes;
      if !over_slots && !over_bytes {
        break;
      }
      let victim = inner
        .chunks
        .iter()
        .filter(|(_, c)| c.pins == 0)
        .min_by_key(|(_, c)| c.last_access)
        .map(|(off, _)| *off);
      let Some(off) = victim else {
        break;
      };
      let chunk = inner.chunks.remove(&off).expect("chunk just looked up");
      inner.resident_bytes -= chunk.data.len();
      inner.evicted_since_drain.push(off);
    }
  }

  #[cfg(test)]
  fn try_evict(&self) {
    let mut inner = self.inner.lock().unwrap();
    self.evict_to_caps(&mut inner);
  }

  #[cfg(test)]
  fn is_resident(&self, chunk_offset: u64) -> bool {
    debug_assert!(chunk_offset.is_multiple_of(self.chunk_size as u64));
    self
      .inner
      .lock()
      .unwrap()
      .chunks
      .contains_key(&chunk_offset)
  }

  #[cfg(test)]
  fn derived_ceiling_bytes(&self) -> usize {
    self.derived_ceiling_bytes
  }
}

fn touch_and_pin(inner: &mut CacheInner, chunk_offset: u64) -> Option<Bytes> {
  let chunk = inner.chunks.get_mut(&chunk_offset)?;
  inner.tick += 1;
  chunk.last_access = inner.tick;
  chunk.pins += 1;
  Some(chunk.data.clone())
}

/// RAII pin guarding a single chunk. Drop releases the pin.
pub struct ChunkRef {
  cache: Arc<ChunkCache>,
  pub offset: u64,
  pub data: Bytes,
}

impl ChunkRef {
  fn new(cache: Arc<ChunkCache>, offset: u64, data: Bytes) -> Self {
    Self {
      cache,
      offset,
      data,
    }
  }
}

impl Drop for ChunkRef {
  fn drop(&mut self) {
    self.cache.unpin(self.offset);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::source::InMemorySource;

  fn make_cache(size: usize, max_chunks: u32, chunk: usize) -> Arc<ChunkCache> {
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let source: Arc<dyn Source> = Arc::new(InMemorySource::new(data));
    ChunkCache::new(
      source,
      CacheOptions {
        chunk_size: chunk,
        max_resident_chunks: max_chunks,
      },
    )
    .unwrap()
  }

  #[tokio::test]
  async fn fetch_loads_chunk_from_source() {
    let cache = make_cache(1024, 4, 64);
    assert!(!cache.is_resident(0));
    let chunk = cache.fetch(0).await.unwrap();
    assert_eq!(chunk.data.len(), 64);
    assert_eq!(chunk.data[0], 0);
    assert_eq!(chunk.data[63], 63);
    assert!(cache.is_resident(0));
  }

  #[tokio::test]
  async fn fetch_hit_is_cached() {
    let cache = make_cache(1024, 4, 64);
    let _a = cache.fetch(64).await.unwrap();
    let resident_after_first = cache.resident_bytes();
    let _b = cache.fetch(64).await.unwrap(); // hit
    assert_eq!(cache.resident_bytes(), resident_after_first);
  }

  #[tokio::test]
  async fn evict_respects_slot_cap_when_unpinned() {
    // 4 chunks of 64 bytes each, cap = 2 slots.
    let cache = make_cache(1024, 2, 64);
    drop(cache.fetch(0).await.unwrap());
    drop(cache.fetch(64).await.unwrap());
    drop(cache.fetch(128).await.unwrap());
    drop(cache.fetch(192).await.unwrap());
    assert!(cache.resident_chunks() <= 2);
    // The most-recently-fetched chunks should still be resident; chunk 0
    // should have been evicted first.
    assert!(!cache.is_resident(0));
    assert!(cache.is_resident(192));
  }

  #[tokio::test]
  async fn evict_skips_pinned_chunks() {
    let cache = make_cache(1024, 2, 64);
    let _pinned = cache.fetch(0).await.unwrap(); // held
    drop(cache.fetch(64).await.unwrap());
    drop(cache.fetch(128).await.unwrap());
    drop(cache.fetch(192).await.unwrap());
    // chunk 0 must still be resident because we hold a pin.
    assert!(cache.is_resident(0));
    // Cap is soft when all-but-one chunk is pinned; verify at least one
    // unpinned chunk got evicted.
    assert!(cache.resident_chunks() <= 3);
  }

  #[tokio::test]
  async fn evict_after_pin_dropped() {
    let cache = make_cache(1024, 2, 64);
    let pinned = cache.fetch(0).await.unwrap();
    drop(cache.fetch(64).await.unwrap());
    drop(pinned);
    drop(cache.fetch(128).await.unwrap());
    drop(cache.fetch(192).await.unwrap());
    assert!(!cache.is_resident(0));
  }

  #[tokio::test]
  async fn evict_lru_drops_oldest() {
    let cache = make_cache(1024, 2, 64); // capacity 2
    drop(cache.fetch(0).await.unwrap());
    drop(cache.fetch(64).await.unwrap());
    // Touch chunk 0 so chunk 64 becomes oldest.
    drop(cache.fetch(0).await.unwrap());
    drop(cache.fetch(128).await.unwrap());
    assert!(cache.is_resident(0));
    assert!(!cache.is_resident(64));
    assert!(cache.is_resident(128));
  }

  #[tokio::test]
  async fn evict_holds_slot_cap_under_full_scan() {
    // 100 MiB source, 16-slot cap, 64 KiB chunks. Linearly scan the whole
    // document and assert resident chunk count never exceeds the cap.
    const SIZE: usize = 100 * 1024 * 1024;
    const MAX_CHUNKS: u32 = 16;
    const CHUNK: usize = 64 * 1024;
    let cache = make_cache(SIZE, MAX_CHUNKS, CHUNK);
    let mut offset = 0u64;
    while (offset as usize) < SIZE {
      drop(cache.fetch(offset).await.unwrap());
      assert!(
        cache.resident_chunks() <= MAX_CHUNKS as usize,
        "slot cap breached at offset {offset}: resident {}, max {MAX_CHUNKS}",
        cache.resident_chunks()
      );
      offset += CHUNK as u64;
    }
  }

  #[tokio::test]
  async fn evict_on_bitmap_growth() {
    // Fill the cache to the slot cap, then simulate bitmap growth pushing
    // past the derived ceiling. Cache must evict to bring totals back.
    let cache = make_cache(1024, 4, 64);
    drop(cache.fetch(0).await.unwrap());
    drop(cache.fetch(64).await.unwrap());
    drop(cache.fetch(128).await.unwrap());
    drop(cache.fetch(192).await.unwrap());
    assert_eq!(cache.resident_chunks(), 4);

    // Derived ceiling = 4 slots x 64 bytes x 2 = 512 bytes.
    // Current chunk_bytes = 256. Push bitmap counter past the headroom.
    let handle = cache.bitmap_bytes_handle();
    handle.store(400, std::sync::atomic::Ordering::Relaxed);
    cache.try_evict();
    // 4 x 64 + 400 = 656 > 512; cache must shed chunks until under.
    assert!(
      cache.resident_bytes() + 400 <= cache.derived_ceiling_bytes(),
      "after bitmap growth: chunk bytes {} + bitmaps 400 should be <= ceiling {}",
      cache.resident_bytes(),
      cache.derived_ceiling_bytes(),
    );
  }

  #[test]
  fn config_rejects_invalid_chunk_size() {
    let src: Arc<dyn Source> = Arc::new(InMemorySource::new(vec![]));
    let err = ChunkCache::new(
      src.clone(),
      CacheOptions {
        chunk_size: 63,
        max_resident_chunks: 4,
      },
    )
    .unwrap_err();
    assert!(matches!(err, CacheError::InvalidChunkSize(63)));
    let err = ChunkCache::new(
      src,
      CacheOptions {
        chunk_size: 0,
        max_resident_chunks: 4,
      },
    )
    .unwrap_err();
    assert!(matches!(err, CacheError::InvalidChunkSize(0)));
  }

  #[test]
  fn config_rejects_zero_slot_cap() {
    let src: Arc<dyn Source> = Arc::new(InMemorySource::new(vec![]));
    let err = ChunkCache::new(
      src,
      CacheOptions {
        chunk_size: 64,
        max_resident_chunks: 0,
      },
    )
    .unwrap_err();
    assert!(matches!(err, CacheError::ZeroMaxResidentChunks));
  }
}
