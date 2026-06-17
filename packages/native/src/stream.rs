//! The `iter` streaming operation: a stateful, batch-filling stream over one
//! container's children. Sits above [`Session`] in the operations layer
//! alongside [`crate::eval`].

use crate::chunks::ChunkWindow;
use crate::path::Segment;
use crate::resolve::{ChildKey, ContainerCursor, ContainerKind, PathFault, ValueLocation};
use crate::select::CompiledSelect;
use crate::session::{Session, SessionError};
use crate::walker::TraverseError;

#[derive(Clone, Copy)]
pub(crate) struct BatchLimits {
  pub(crate) max_count: usize,
  pub(crate) max_bytes: usize,
}

impl BatchLimits {
  /// Count-only limits: the byte cap is set out of reach so only the count binds.
  #[cfg(test)]
  pub(crate) fn count(max_count: usize) -> Self {
    Self {
      max_count,
      max_bytes: usize::MAX,
    }
  }
}

/// What one [`IterState::fill_batch`] inner pass produced: a full batch, or the
/// end of the stream (with the final partial batch, if any).
enum BatchFill {
  Batch(String),
  Exhausted(Option<String>),
}

/// State of one `iter` stream: the lazily-opened container cursor and its byte
/// window, plus the projection, batching, and key-wrapping options.
pub(crate) struct IterState {
  pub(crate) initialized: bool,
  pub(crate) path: Vec<Segment>,
  pub(crate) with_key: bool,
  pub(crate) anchor_start: u64,
  /// Document depth of `anchor_start`; children sit at `base_depth + path.len() + 1`.
  pub(crate) base_depth: u32,
  /// `value_start` of the base container, once resolved. Where the stream
  /// records `close`/resume-point array members.
  pub(crate) base_value_start: Option<u64>,
  /// At rest holds at most the chunk covering `next_offset` so the next yield's
  /// first read hits; everything else is pruned per yield, bounding resident
  /// chunks to ~1 between yields.
  pub(crate) window: ChunkWindow,
  /// Serialized projection IR; parse errors are deferred to the first `next()`.
  pub(crate) select_ir: Option<String>,
  /// Compiled once during initialization, after which `select_ir` is dropped.
  /// `None` yields the whole child.
  pub(crate) select: Option<CompiledSelect>,
  /// Set by [`IterState::initialize`]. `None` if the path didn't resolve, and
  /// again once the stream is exhausted or released (iteration yields nothing).
  pub(crate) child_cursor: Option<ContainerCursor>,
  /// Upper bound on items per fetch.
  pub(crate) max_batch_count: usize,
  /// Upper bound on serialized bytes per fetch.
  pub(crate) max_batch_bytes: usize,
}

impl IterState {
  pub(crate) fn new(
    session: &Session,
    path: Vec<Segment>,
    anchor_start: u64,
    base_depth: u32,
    select_ir: Option<String>,
    limits: BatchLimits,
    with_key: bool,
  ) -> Self {
    Self {
      path,
      anchor_start,
      base_depth,
      initialized: false,
      child_cursor: None,
      base_value_start: None,
      window: session.new_window(),
      select_ir,
      select: None,
      max_batch_count: limits.max_count,
      max_batch_bytes: limits.max_bytes,
      with_key,
    }
  }

  pub(crate) async fn initialize(&mut self, session: &Session) -> Result<(), SessionError> {
    if let Some(json) = self.select_ir.as_deref() {
      self.select = Some(CompiledSelect::parse(json)?);
      self.select_ir = None;
    }
    let located = session
      .run_locate(
        &self.path,
        self.anchor_start,
        self.base_depth,
        &mut self.window,
      )
      .await;
    // Locating directly into the stream's window leaves the target's chunk
    // resident for enter_container; clear on the no-open paths so a stream that
    // never opens holds nothing.
    let start = match located {
      Ok(Some(start)) => start,
      Ok(None) => {
        self.window.clear();
        self.initialized = true;
        return Ok(());
      }
      Err(e) => {
        self.window.clear();
        return Err(e);
      }
    };
    self.base_value_start = Some(start);
    let entered = session.enter_container(start, &mut self.window).await?;
    self.initialized = true;
    let Some(cursor) = entered else {
      self.window.clear();
      return Err(SessionError::Path(PathFault::ScalarTarget));
    };
    session.prune_window(&mut self.window, cursor.next_offset);
    self.child_cursor = Some(cursor);
    Ok(())
  }

  pub(crate) async fn fill_batch(
    &mut self,
    session: &Session,
  ) -> Result<Option<String>, SessionError> {
    let Self {
      path,
      child_cursor,
      base_value_start,
      window,
      select,
      ..
    } = self;
    let Some(cursor) = child_cursor.as_mut() else {
      return Ok(None);
    };
    let kind = cursor.kind;
    let anchor_start = self.anchor_start;
    let base_depth = self.base_depth;
    let base_value_start = *base_value_start;
    let select = select.as_ref();
    let max_batch_count = self.max_batch_count;
    let max_batch_bytes = self.max_batch_bytes;
    let with_key = self.with_key;

    let sampling_interval = session.array_landmark_sampling_interval();
    let mut landmarks: Vec<(usize, u64)> = Vec::new();

    // The window is pruned after each item, so the buffer (not chunks) is the
    // in-flight batch and needs no cleanup on early termination via `complete`.
    let outcome: Result<BatchFill, SessionError> = async {
      let mut buf: Vec<u8> = vec![b'['];
      let mut count = 0usize;
      loop {
        let Some(child) = session.next_child(cursor, window).await? else {
          // Exhausted: the cursor sits AT the close. Record the close offset on
          // the base node, keyed on the entered container kind.
          if let Some(vs) = base_value_start {
            session.store_close(
              base_depth,
              anchor_start,
              path,
              kind,
              vs,
              cursor.close_offset(),
            );
          }
          if count == 0 {
            return Ok(BatchFill::Exhausted(None));
          }
          buf.push(b']');
          // SAFETY: stitched from valid-UTF-8 JSON source slices and ASCII punctuation.
          return Ok(BatchFill::Exhausted(Some(unsafe {
            String::from_utf8_unchecked(buf)
          })));
        };
        // Render the key before materialize/prune: a member's raw span sits
        // behind the value, so its chunk is only guaranteed resident now.
        let key_json = if with_key {
          Some(render_key(session, child.key, window).await?)
        } else {
          None
        };
        let loc = child.location;
        // Sample array landmarks on the absolute index grid so a later random
        // index resumes from the nearest one. Index 0 is the array's open, so
        // it's implicit; skip it. Objects (member keys) have no index landmark.
        if sampling_interval > 0 {
          if let ChildKey::Index(idx) = child.key {
            if idx != 0 && idx.is_multiple_of(sampling_interval) {
              landmarks.push((idx, loc.start));
            }
          }
        }
        let value = match select {
          Some(sel) => crate::eval::project(session, sel, loc.start, window).await?,
          None => session.materialize(loc, window).await?,
        };
        session.prune_window(window, cursor.next_offset);
        if count > 0 {
          buf.push(b',');
        }
        match &key_json {
          Some(key) => {
            buf.push(b'[');
            buf.extend_from_slice(key);
            buf.push(b',');
            buf.extend_from_slice(&value);
            buf.push(b']');
          }
          None => buf.extend_from_slice(&value),
        }
        count += 1;
        // Flush on whichever bound binds first. The byte check runs after a full
        // item is appended, so a single oversized item still flushes alone (with
        // its trailing `]`), keeping the stream making progress.
        if count >= max_batch_count || buf.len() >= max_batch_bytes {
          buf.push(b']');
          // SAFETY: as above.
          return Ok(BatchFill::Batch(unsafe {
            String::from_utf8_unchecked(buf)
          }));
        }
      }
    }
    .await;

    if let Some(vs) = base_value_start {
      session.store_array_landmarks(base_depth, anchor_start, path, vs, &landmarks);
    }

    let next_offset = cursor.next_offset;
    match outcome {
      // The stream is known done: free the final chunk now and make later
      // next()/complete() calls no-ops.
      Ok(BatchFill::Exhausted(text)) => {
        window.clear();
        *child_cursor = None;
        Ok(text)
      }
      Ok(BatchFill::Batch(text)) => {
        session.prune_window(window, next_offset);
        Ok(Some(text))
      }
      // Defensive prune so an errored, abandoned stream doesn't retain chunks
      // past the scan position.
      Err(e) => {
        session.prune_window(window, next_offset);
        Err(e)
      }
    }
  }

  pub(crate) fn record_early_break(&self, session: &Session) {
    if let (Some(w), Some(vs)) = (self.child_cursor.as_ref(), self.base_value_start) {
      let before_end = session.source_size.is_none_or(|size| w.next_offset < size);
      if w.kind == ContainerKind::Array && w.index > 0 && before_end {
        session.store_array_resume_point(
          self.base_depth,
          self.anchor_start,
          &self.path,
          vs,
          w.index,
          w.next_offset,
        );
      }
    }
  }

  pub(crate) fn release(&mut self) {
    self.window.clear();
    self.child_cursor = None;
  }
}

async fn render_key(
  session: &Session,
  key: ChildKey,
  window: &mut ChunkWindow,
) -> Result<Vec<u8>, SessionError> {
  match key {
    ChildKey::Index(index) => Ok(index.to_string().into_bytes()),
    ChildKey::Member { start, close } => {
      let raw = session
        .materialize(
          ValueLocation {
            start,
            end: close + 1,
          },
          window,
        )
        .await?;
      let name: String =
        serde_json::from_slice(&raw).map_err(|_| TraverseError::Malformed(start))?;
      let mut out = Vec::new();
      serde_json::to_writer(&mut out, &name).expect("serializing a JSON string key is infallible");
      Ok(out)
    }
  }
}
