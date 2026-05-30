//! Chunk cache with LRU eviction and RAII pinning.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use thiserror::Error;

use crate::source::{Source, SourceError};

const MIN_CHUNK_SIZE: usize = 64;

/// Multiplier applied to `max_resident_bytes` (the resident chunk-data budget)
/// to derive the total-bytes ceiling. The factor of 2 covers worst-case bitmap
/// growth:
/// one `in_string` bitmap (~chunk/8 bytes) plus up to five structural
/// bitmaps (each ~chunk/8 bytes) =~ chunk x 3/4, comfortably below 1x
/// chunk per slot of bitmap overhead. The 2x total leaves room for
/// padding without ever causing the cache to evict purely on bytes for
/// well-behaved workloads.
const CEILING_FACTOR: usize = 2;

/// Upper bound on a single coalesced `source.read` issued by
/// [`ChunkCache::fetch`]. A burst larger than this is split into
/// multiple reads so the transient read buffer stays bounded (independent of
/// the doubling burst's chunk count) while still collapsing many per-chunk
/// reads into a handful.
const MAX_COALESCED_READ_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct CacheOptions {
  pub chunk_size: usize,
  /// Bytes of source chunk data to keep resident. Must be a non-zero multiple
  /// of `chunk_size`; maps to exactly `max_resident_bytes / chunk_size` slots.
  pub max_resident_bytes: usize,
}

#[derive(Debug, Error)]
pub enum CacheError {
  #[error("chunk size must be a non-zero multiple of {MIN_CHUNK_SIZE}, got {0}")]
  InvalidChunkSize(usize),
  #[error(
    "max_resident_bytes must be a non-zero multiple of chunk_size {chunk_size}, got {budget}"
  )]
  InvalidMaxResidentBytes { budget: usize, chunk_size: usize },
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
    // A non-zero multiple of chunk_size maps to a whole number of slots (>= 1),
    // so this one rule also guarantees room for a single pinned frontier chunk.
    if options.max_resident_bytes == 0
      || !options
        .max_resident_bytes
        .is_multiple_of(options.chunk_size)
    {
      return Err(CacheError::InvalidMaxResidentBytes {
        budget: options.max_resident_bytes,
        chunk_size: options.chunk_size,
      });
    }
    // A whole number of slots (>= 1) by the multiple-of-chunk_size rule above.
    let max_resident_chunks = (options.max_resident_bytes / options.chunk_size) as u32;
    // Total RSS ceiling = resident chunk bytes + bitmap headroom. Identical to
    // the old `chunks * chunk_size * CEILING_FACTOR`, so eviction is unchanged.
    let derived_ceiling_bytes = options.max_resident_bytes.saturating_mul(CEILING_FACTOR);
    Ok(Arc::new(Self {
      source,
      chunk_size: options.chunk_size,
      max_resident_chunks,
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

  /// Bytes currently attributed to structural/string bitmaps, shared with
  /// the session's `BitmapStore`. `resident_bytes() + bitmap_bytes()` is the
  /// total native memory held for source data, bounded by the ceiling.
  pub fn bitmap_bytes(&self) -> usize {
    self.bitmap_bytes.load(Ordering::Relaxed)
  }

  /// Hard cap on `resident_bytes + bitmap_bytes`; eviction keeps the cache
  /// at or below this regardless of document size.
  pub fn derived_ceiling_bytes(&self) -> usize {
    self.derived_ceiling_bytes
  }

  pub fn bitmap_bytes_handle(&self) -> Arc<AtomicUsize> {
    self.bitmap_bytes.clone()
  }

  /// Fetch the `n` consecutive chunks starting at `chunk_offset` (aligned),
  /// reading runs of not-yet-resident chunks in single coalesced `source.read`
  /// calls instead of one read per chunk. Already-resident chunks are pinned
  /// without a read; each read is split back into per-chunk entries (copied
  /// into independent `Bytes` so each chunk evicts on its own). Returns a
  /// pinned `ChunkRef` for every chunk in
  /// `[chunk_offset, chunk_offset + n*chunk_size)` (clamped to end-of-source),
  /// in ascending offset order.
  pub async fn fetch(
    self: &Arc<Self>,
    chunk_offset: u64,
    n: u64,
  ) -> Result<Vec<ChunkRef>, CacheError> {
    debug_assert!(chunk_offset.is_multiple_of(self.chunk_size as u64));
    let cs = self.chunk_size as u64;
    let span_end = chunk_offset
      .saturating_add(n.saturating_mul(cs))
      .min(self.source.size());
    // Coalesce at most a byte-cap's worth of chunks per read, and never more
    // than the cache is sized to hold - so a single read's transient buffer
    // stays bounded by the resident budget on tight caps.
    let max_chunks_per_read = (MAX_COALESCED_READ_BYTES / self.chunk_size)
      .max(1)
      .min(self.max_resident_chunks as usize) as u64;

    // One ref per chunk in the (clamped) span; size the Vec up front.
    let span_chunks = span_end.saturating_sub(chunk_offset).div_ceil(cs) as usize;
    let mut refs = Vec::with_capacity(span_chunks);
    let mut cur = chunk_offset;
    while cur < span_end {
      // Already resident: pin and continue without a read.
      {
        let mut inner = self.inner.lock().unwrap();
        if let Some(data) = touch_and_pin(&mut inner, cur) {
          refs.push(ChunkRef::new(self.clone(), cur, data));
          cur += cs;
          continue;
        }
      }
      // `cur` is absent. Extend a run of absent chunks - bounded by the
      // per-read byte cap and the burst span - so we read them in one call.
      let run_start = cur;
      let run_cap = run_start
        .saturating_add(max_chunks_per_read.saturating_mul(cs))
        .min(span_end);
      // Scan the run's extent under one lock (cheap key lookups, no read held
      // across it) rather than re-locking per chunk; a resident chunk splits
      // the run, so we read only up to it.
      let mut run_end = (run_start + cs).min(span_end);
      {
        let inner = self.inner.lock().unwrap();
        while run_end < run_cap && !inner.chunks.contains_key(&run_end) {
          run_end = (run_end + cs).min(run_cap); // never overshoot the cap/EOF
        }
      }
      // One coalesced read for the whole absent run, no lock held.
      let buf = self
        .source
        .read(run_start, (run_end - run_start) as usize)
        .await?;
      let mut off = run_start;
      while off < run_end {
        let rel = (off - run_start) as usize;
        if rel >= buf.len() {
          break; // defensive: a short read before EOF violates the contract
        }
        let end = (rel + self.chunk_size).min(buf.len());
        let piece = Bytes::copy_from_slice(&buf[rel..end]);
        let mut inner = self.inner.lock().unwrap();
        // A concurrent task may have populated this chunk while we read.
        if let Some(data) = touch_and_pin(&mut inner, off) {
          refs.push(ChunkRef::new(self.clone(), off, data));
        } else {
          inner.tick += 1;
          let tick = inner.tick;
          inner.resident_bytes += piece.len();
          inner.chunks.insert(
            off,
            Chunk {
              data: piece.clone(),
              last_access: tick,
              pins: 1,
            },
          );
          self.evict_to_caps(&mut inner);
          refs.push(ChunkRef::new(self.clone(), off, piece));
        }
        off += cs;
      }
      cur = run_end;
    }
    Ok(refs)
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
        max_resident_bytes: max_chunks as usize * chunk,
      },
    )
    .unwrap()
  }

  #[test]
  fn config_rejects_invalid_chunk_size() {
    let src: Arc<dyn Source> = Arc::new(InMemorySource::new(vec![]));
    let err = ChunkCache::new(
      src.clone(),
      CacheOptions {
        chunk_size: 63,
        max_resident_bytes: 256,
      },
    )
    .unwrap_err();
    assert!(matches!(err, CacheError::InvalidChunkSize(63)));
    let err = ChunkCache::new(
      src,
      CacheOptions {
        chunk_size: 0,
        max_resident_bytes: 256,
      },
    )
    .unwrap_err();
    assert!(matches!(err, CacheError::InvalidChunkSize(0)));
  }

  #[test]
  fn config_rejects_zero_budget() {
    let src: Arc<dyn Source> = Arc::new(InMemorySource::new(vec![]));
    let err = ChunkCache::new(
      src,
      CacheOptions {
        chunk_size: 64,
        max_resident_bytes: 0,
      },
    )
    .unwrap_err();
    assert!(matches!(
      err,
      CacheError::InvalidMaxResidentBytes {
        budget: 0,
        chunk_size: 64
      }
    ));
  }

  #[test]
  fn config_rejects_budget_not_multiple_of_chunk_size() {
    let src: Arc<dyn Source> = Arc::new(InMemorySource::new(vec![]));
    // 200 is not a multiple of the 64-byte chunk size.
    let err = ChunkCache::new(
      src,
      CacheOptions {
        chunk_size: 64,
        max_resident_bytes: 200,
      },
    )
    .unwrap_err();
    assert!(matches!(
      err,
      CacheError::InvalidMaxResidentBytes {
        budget: 200,
        chunk_size: 64
      }
    ));
  }

  #[tokio::test]
  async fn fetch_loads_chunk_from_source() {
    let cache = make_cache(1024, 4, 64);
    assert!(!cache.is_resident(0));
    let chunks = cache.fetch(0, 1).await.unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].data.len(), 64);
    assert_eq!(chunks[0].data[0], 0);
    assert_eq!(chunks[0].data[63], 63);
    assert!(cache.is_resident(0));
  }

  #[tokio::test]
  async fn fetch_hit_skips_reload() {
    let cache = make_cache(1024, 4, 64);
    let _a = cache.fetch(64, 1).await.unwrap();
    let resident_after_first = cache.resident_bytes();
    let _b = cache.fetch(64, 1).await.unwrap(); // hit: no second read
    assert_eq!(cache.resident_bytes(), resident_after_first);
  }

  #[tokio::test]
  async fn evict_respects_slot_cap_when_unpinned() {
    // 4 chunks of 64 bytes each, cap = 2 slots; each fetched and released in
    // turn so eviction runs between accesses.
    let cache = make_cache(1024, 2, 64);
    for off in [0u64, 64, 128, 192] {
      drop(cache.fetch(off, 1).await.unwrap());
    }
    assert!(cache.resident_chunks() <= 2);
    // The most-recently-fetched chunks should still be resident; chunk 0
    // should have been evicted first.
    assert!(!cache.is_resident(0));
    assert!(cache.is_resident(192));
  }

  #[tokio::test]
  async fn evict_skips_pinned_chunks() {
    let cache = make_cache(1024, 2, 64);
    // Hold the returned Vec to keep chunk 0 pinned across the later fetches.
    let _pinned = cache.fetch(0, 1).await.unwrap();
    for off in [64u64, 128, 192] {
      drop(cache.fetch(off, 1).await.unwrap());
    }
    // chunk 0 must still be resident because we hold a pin.
    assert!(cache.is_resident(0));
    // Cap is soft when all-but-one chunk is pinned; verify at least one
    // unpinned chunk got evicted.
    assert!(cache.resident_chunks() <= 3);
  }

  #[tokio::test]
  async fn evict_after_pin_dropped() {
    let cache = make_cache(1024, 2, 64);
    let pinned = cache.fetch(0, 1).await.unwrap();
    drop(cache.fetch(64, 1).await.unwrap());
    drop(pinned);
    drop(cache.fetch(128, 1).await.unwrap());
    drop(cache.fetch(192, 1).await.unwrap());
    assert!(!cache.is_resident(0));
  }

  #[tokio::test]
  async fn evict_lru_drops_oldest() {
    let cache = make_cache(1024, 2, 64); // capacity 2
    drop(cache.fetch(0, 1).await.unwrap());
    drop(cache.fetch(64, 1).await.unwrap());
    // Touch chunk 0 so chunk 64 becomes oldest.
    drop(cache.fetch(0, 1).await.unwrap());
    drop(cache.fetch(128, 1).await.unwrap());
    assert!(cache.is_resident(0));
    assert!(!cache.is_resident(64));
    assert!(cache.is_resident(128));
  }

  #[tokio::test]
  async fn evict_holds_slot_cap_under_full_scan() {
    // 100 MiB source, 16-slot cap, 64 KiB chunks. Linearly scan the whole
    // document one chunk at a time and assert resident count never exceeds cap.
    const SIZE: usize = 100 * 1024 * 1024;
    const MAX_CHUNKS: u32 = 16;
    const CHUNK: usize = 64 * 1024;
    let cache = make_cache(SIZE, MAX_CHUNKS, CHUNK);
    let mut offset = 0u64;
    while (offset as usize) < SIZE {
      drop(cache.fetch(offset, 1).await.unwrap());
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
    for off in [0u64, 64, 128, 192] {
      drop(cache.fetch(off, 1).await.unwrap());
    }
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

  /// Wraps an [`InMemorySource`] and counts its `read` calls so coalescing is
  /// directly observable.
  struct CountingSource {
    inner: InMemorySource,
    reads: Arc<AtomicUsize>,
  }

  #[async_trait::async_trait]
  impl Source for CountingSource {
    fn size(&self) -> u64 {
      self.inner.size()
    }
    async fn read(&self, offset: u64, length: usize) -> Result<Bytes, SourceError> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      self.inner.read(offset, length).await
    }
  }

  fn counting_cache(
    size: usize,
    chunk: usize,
    max_chunks: u32,
  ) -> (Arc<ChunkCache>, Arc<AtomicUsize>) {
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn Source> = Arc::new(CountingSource {
      inner: InMemorySource::new(data),
      reads: reads.clone(),
    });
    let cache = ChunkCache::new(
      source,
      CacheOptions {
        chunk_size: chunk,
        max_resident_bytes: max_chunks as usize * chunk,
      },
    )
    .unwrap();
    (cache, reads)
  }

  #[tokio::test]
  async fn fetch_coalesces_absent_run_into_one_read() {
    // 8 chunks of 64 B. The coalescing cap (1 MiB) dwarfs the 512 B span, so a
    // cold fetch(0, 8) is a single source.read, not eight.
    let (cache, reads) = counting_cache(512, 64, 16);
    let refs = cache.fetch(0, 8).await.unwrap();
    assert_eq!(refs.len(), 8, "one ref per chunk in the span");
    assert_eq!(
      reads.load(Ordering::Relaxed),
      1,
      "8 contiguous absent chunks must coalesce into one read",
    );
    // Each ref carries the correct chunk-aligned bytes.
    for (i, r) in refs.iter().enumerate() {
      assert_eq!(r.offset, (i * 64) as u64);
      assert_eq!(
        &r.data[..],
        &(0..512).map(|b| (b % 251) as u8).collect::<Vec<_>>()[i * 64..i * 64 + 64]
      );
    }
    assert_eq!(cache.resident_chunks(), 8);
  }

  #[tokio::test]
  async fn fetch_does_not_reread_resident_chunks() {
    // Warm chunk 2 (offset 128) alone, then span [0, 8): the resident chunk
    // splits the run into [0,128) and [192,512) - two reads, chunk 2 reused.
    let (cache, reads) = counting_cache(512, 64, 16);
    drop(cache.fetch(128, 1).await.unwrap());
    assert_eq!(reads.load(Ordering::Relaxed), 1);

    let refs = cache.fetch(0, 8).await.unwrap();
    assert_eq!(refs.len(), 8);
    assert_eq!(
      reads.load(Ordering::Relaxed),
      3,
      "1 warmup + 2 coalesced runs flanking the resident chunk",
    );
  }

  #[tokio::test]
  async fn fetch_caps_reads_to_resident_budget() {
    // The per-read chunk count is clamped to `max_resident_chunks` so a single
    // coalesced read never exceeds what the cache is sized to hold. With a
    // 4-slot cap (and a byte cap far above 4 chunks), a 12-chunk span reads in
    // runs of at most 4 => 3 reads.
    let (cache, reads) = counting_cache(64 * 12, 64, 4);
    let refs = cache.fetch(0, 12).await.unwrap();
    assert_eq!(refs.len(), 12);
    assert_eq!(
      reads.load(Ordering::Relaxed),
      3,
      "12 chunks at 4 chunks/read (clamped to the 4-slot budget) => 3 reads",
    );
  }

  #[tokio::test]
  async fn fetch_clamps_to_source_end() {
    // 5 chunks' worth of data; ask for 8. The span clamps to EOF, the last
    // chunk is the partial tail (not a full chunk_size).
    let (cache, _reads) = counting_cache(64 * 4 + 20, 64, 16);
    let refs = cache.fetch(0, 8).await.unwrap();
    assert_eq!(refs.len(), 5, "4 full chunks + 1 partial tail");
    assert_eq!(refs.last().unwrap().offset, 256);
    assert_eq!(refs.last().unwrap().data.len(), 20, "tail chunk is partial");
  }
}
