//! Transient chunk I/O: the only source-data storage in the walk.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use thiserror::Error;

use crate::source::{ByteStream, ReadOutcome, SourceError};

/// Primitive needed a non-resident chunk; the async driver reads the chunk at
/// this offset and retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("chunk at offset {0} not loaded")]
pub struct ChunkMiss(pub u64);

pub struct ChunkWindow {
  #[cfg(test)]
  peak_len: usize,
  chunk_bytes: u64,
  source_size: Option<u64>,
  chunks: HashMap<u64, Bytes>,
}

impl ChunkWindow {
  /// Window over a source of known size.
  pub fn new(chunk_bytes: u64, source_size: u64) -> Self {
    Self::with_size(chunk_bytes, Some(source_size))
  }

  /// Window over a forward source whose size isn't known until [`Self::learn_eof`].
  pub fn new_unsized(chunk_bytes: u64) -> Self {
    Self::with_size(chunk_bytes, None)
  }

  fn with_size(chunk_bytes: u64, source_size: Option<u64>) -> Self {
    Self {
      #[cfg(test)]
      peak_len: 0,
      chunk_bytes,
      source_size,
      chunks: HashMap::new(),
    }
  }

  /// Source size as a scan bound: the known size, or `u64::MAX` while still
  /// unknown so loops run until a chunk fault discovers the end.
  pub fn source_size(&self) -> u64 {
    self.source_size.unwrap_or(u64::MAX)
  }

  /// The known size, or `None` for a forward source whose end isn't found yet.
  pub fn known_size(&self) -> Option<u64> {
    self.source_size
  }

  pub fn chunk_bytes(&self) -> u64 {
    self.chunk_bytes
  }

  /// Record the end discovered from a `read`'s eof. First write wins; a known
  /// size never moves.
  pub fn set_source_size(&mut self, end: u64) {
    self.source_size.get_or_insert(end);
  }

  #[inline]
  fn chunk_start_of(&self, offset: u64) -> u64 {
    (offset / self.chunk_bytes) * self.chunk_bytes
  }

  pub fn clear(&mut self) {
    self.chunks.clear();
  }

  pub fn insert(&mut self, chunk_start: u64, bytes: Bytes) {
    self.chunks.insert(chunk_start, bytes);
    #[cfg(test)]
    {
      self.peak_len = self.peak_len.max(self.chunks.len());
    }
  }

  /// Drop every chunk below the one containing `min_reachable`. Forward-only
  /// traversal never revisits bytes below the committed position, so this keeps
  /// the window bounded as the scan advances.
  pub fn drop_below(&mut self, min_reachable: u64) {
    let fc = self.chunk_start_of(min_reachable);
    self.chunks.retain(|&off, _| off >= fc);
  }

  /// Resident-chunk count; only used to assert the bounded-memory invariant.
  #[cfg(test)]
  pub fn len(&self) -> usize {
    self.chunks.len()
  }

  #[cfg(test)]
  pub fn is_empty(&self) -> bool {
    self.chunks.is_empty()
  }

  #[cfg(test)]
  pub fn contains(&self, chunk_start: u64) -> bool {
    self.chunks.contains_key(&chunk_start)
  }

  /// Bytes of the chunk at chunk-aligned `chunk_start`, or `ChunkMiss` if not
  /// resident. The slice is tied to the window so the [`Walker`]'s per-step
  /// chunk cache can hold it across `&mut self` calls.
  #[inline]
  pub(crate) fn chunk_for(&self, chunk_start: u64) -> Result<&[u8], ChunkMiss> {
    self
      .chunks
      .get(&chunk_start)
      .map(|b| &b[..])
      .ok_or(ChunkMiss(chunk_start))
  }

  /// Highest `len()` ever reached, including the transient peak mid-fault before
  /// a prune. Lets tests catch a burst that balloons the window past one burst.
  #[cfg(test)]
  pub fn peak_len(&self) -> usize {
    self.peak_len
  }
}

const MIN_CHUNK_BYTES: usize = 64;

/// Upper bound on a single coalesced `source.read`: a larger burst splits into
/// several reads so the transient buffer stays bounded, independent of the
/// burst's chunk count.
const MAX_COALESCED_READ_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ReaderError {
  #[error(transparent)]
  ByteStream(#[from] SourceError),
  #[error("chunk size must be a non-zero multiple of {MIN_CHUNK_BYTES}, got {0}")]
  InvalidChunkSize(usize),
}

pub struct ChunkReader {
  source: Arc<dyn ByteStream>,
  chunk_bytes: u64,
}

impl ChunkReader {
  pub fn new(source: Arc<dyn ByteStream>, chunk_bytes: usize) -> Result<Arc<Self>, ReaderError> {
    if chunk_bytes == 0 || !chunk_bytes.is_multiple_of(MIN_CHUNK_BYTES) {
      return Err(ReaderError::InvalidChunkSize(chunk_bytes));
    }
    Ok(Arc::new(Self {
      source,
      chunk_bytes: chunk_bytes as u64,
    }))
  }

  pub fn chunk_bytes(&self) -> u64 {
    self.chunk_bytes
  }

  /// Read `n` chunks from chunk-aligned `start` (ascending), coalescing
  /// contiguous chunks into reads of at most [`MAX_COALESCED_READ_BYTES`] and
  /// splitting each back into per-chunk `Bytes` so dropping one frees its bytes
  /// without retaining the whole coalesced buffer. The final chunk may be a
  /// partial tail.
  ///
  /// `n` bounds the request, the source's `eof` bounds reality - so no size
  /// clamp is needed (a sized source clamps its own reads). When a read reports
  /// eof, the returned `Option` is the discovered end-of-source offset, which
  /// the caller folds back into the window via [`ChunkWindow::learn_eof`].
  pub async fn read_chunks(
    &self,
    start: u64,
    n: u64,
  ) -> Result<(Vec<(u64, Bytes)>, Option<u64>), ReaderError> {
    debug_assert!(start.is_multiple_of(self.chunk_bytes));
    let cs = self.chunk_bytes;
    let span_end = start.saturating_add(n.saturating_mul(cs));
    let max_per_read = (MAX_COALESCED_READ_BYTES / self.chunk_bytes as usize).max(1) as u64;
    let mut out = Vec::with_capacity(n as usize);
    let mut discovered_end = None;

    let mut cur = start;
    while cur < span_end {
      let run_end = cur
        .saturating_add(max_per_read.saturating_mul(cs))
        .min(span_end);
      let ReadOutcome { bytes: buf, eof } = self.source.read(cur, (run_end - cur) as usize).await?;
      let got = buf.len() as u64;
      let mut off = cur;
      while off < run_end {
        let rel = (off - cur) as usize;
        if rel >= buf.len() {
          break; // short read: the rest of this run is past end-of-source
        }
        let end = (rel + self.chunk_bytes as usize).min(buf.len());
        out.push((off, buf.slice(rel..end)));
        off += cs;
      }
      if eof {
        discovered_end = Some(cur + got);
        break;
      }
      cur = run_end;
    }
    Ok((out, discovered_end))
  }
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};

  use async_trait::async_trait;

  use super::*;
  use crate::source::{ForwardStream, InMemoryStream};

  fn window(chunk_bytes: u64, source: &[u8]) -> ChunkWindow {
    let mut w = ChunkWindow::new(chunk_bytes, source.len() as u64);
    let mut off = 0u64;
    while (off as usize) < source.len() {
      let end = (off as usize + chunk_bytes as usize).min(source.len());
      w.insert(off, Bytes::copy_from_slice(&source[off as usize..end]));
      off += chunk_bytes;
    }
    w
  }

  #[test]
  fn window_chunk_for_returns_bytes_or_miss() {
    let source: Vec<u8> = (0..128).map(|i| (i % 251) as u8).collect();
    let w = window(64, &source);
    assert_eq!(w.chunk_for(0).unwrap()[0], 0);
    assert_eq!(w.chunk_for(64).unwrap()[0], 64);
    assert_eq!(w.chunk_for(128).unwrap_err(), ChunkMiss(128));
  }

  #[test]
  fn window_drop_below_evicts_lower_chunks() {
    let source: Vec<u8> = (0..256).map(|i| (i % 251) as u8).collect();
    let mut w = window(64, &source);
    assert_eq!(w.len(), 4);
    w.drop_below(130); // chunk containing 130 is 128; keep >= 128
    assert!(!w.contains(0));
    assert!(!w.contains(64));
    assert!(w.contains(128));
    assert!(w.contains(192));
    assert_eq!(w.len(), 2);
  }

  /// Counts `read` calls so coalescing is directly observable.
  struct CountingSource {
    inner: InMemoryStream,
    reads: Arc<AtomicUsize>,
  }

  #[async_trait]
  impl ByteStream for CountingSource {
    fn size(&self) -> Option<u64> {
      self.inner.size()
    }
    async fn read(&self, offset: u64, length: usize) -> Result<ReadOutcome, SourceError> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      self.inner.read(offset, length).await
    }
  }

  fn reader(size: usize, chunk: usize) -> (Arc<ChunkReader>, Arc<AtomicUsize>) {
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn ByteStream> = Arc::new(CountingSource {
      inner: InMemoryStream::new(data),
      reads: reads.clone(),
    });
    (ChunkReader::new(source, chunk).unwrap(), reads)
  }

  #[tokio::test]
  async fn reader_coalesces_contiguous_run_into_one_read() {
    // 4 MiB cap dwarfs the 512 B span, so 8 chunks fold into one read
    let (reader, reads) = reader(512, 64);
    let (chunks, discovered) = reader.read_chunks(0, 8).await.unwrap();
    assert_eq!(chunks.len(), 8, "one entry per chunk in the span");
    assert_eq!(discovered, Some(512), "the full read reaches the end");
    assert_eq!(
      reads.load(Ordering::Relaxed),
      1,
      "8 chunks coalesce into one read"
    );
    for (i, (off, data)) in chunks.iter().enumerate() {
      assert_eq!(*off, (i * 64) as u64);
      assert_eq!(data.len(), 64);
      assert_eq!(
        &data[..],
        &(0..512).map(|b| (b % 251) as u8).collect::<Vec<_>>()[i * 64..i * 64 + 64]
      );
    }
  }

  #[tokio::test]
  async fn reader_clamps_span_to_source_end() {
    // 4 full chunks + a 20-byte partial tail; ask for 8.
    let (reader, _reads) = reader(64 * 4 + 20, 64);
    let (chunks, discovered) = reader.read_chunks(0, 8).await.unwrap();
    assert_eq!(chunks.len(), 5, "4 full chunks + 1 partial tail");
    assert_eq!(chunks.last().unwrap().0, 256);
    assert_eq!(chunks.last().unwrap().1.len(), 20, "tail chunk is partial");
    assert_eq!(
      discovered,
      Some(64 * 4 + 20),
      "end discovered from the short read"
    );
  }

  #[tokio::test]
  async fn reader_discovers_eof_for_unsized_source() {
    // Same shape behind a forward (unknown-size) source: the reader learns the
    // end from the source's eof, not from a size clamp.
    let data: Vec<u8> = (0..64 * 4 + 20).map(|i| (i % 251) as u8).collect();
    let source: Arc<dyn ByteStream> = Arc::new(ForwardStream::new(data));
    let reader = ChunkReader::new(source, 64).unwrap();
    let (chunks, discovered) = reader.read_chunks(0, 8).await.unwrap();
    assert_eq!(chunks.len(), 5, "4 full chunks + 1 partial tail");
    assert_eq!(chunks.last().unwrap().1.len(), 20, "tail chunk is partial");
    assert_eq!(discovered, Some(64 * 4 + 20), "end discovered from eof");
  }

  #[tokio::test]
  async fn reader_per_chunk_bytes_are_independent_allocations() {
    // can't observe the allocator; copy_from_slice gives each chunk its own
    // Bytes, so dropping one can't keep the others alive.
    let (reader, _reads) = reader(512, 64);
    let (chunks, _discovered) = reader.read_chunks(0, 8).await.unwrap();
    for (_, data) in &chunks {
      assert_eq!(data.len(), 64);
    }
  }

  #[test]
  fn reader_rejects_invalid_chunk_size() {
    let src: Arc<dyn ByteStream> = Arc::new(InMemoryStream::new(vec![]));
    assert!(matches!(
      ChunkReader::new(src.clone(), 63).err().expect("rejects 63"),
      ReaderError::InvalidChunkSize(63)
    ));
    assert!(matches!(
      ChunkReader::new(src, 0).err().expect("rejects 0"),
      ReaderError::InvalidChunkSize(0)
    ));
  }
}
