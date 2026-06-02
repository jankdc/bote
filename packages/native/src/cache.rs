//! Structural-index cache: a bounded, lazily-built partial skeleton of the
//! document.
//!
//! Caches the *containers* a scan has walked - not whole resolved paths - so a
//! later query that lands in a container we've already entered starts near the
//! target instead of at the container's open. Each [`ContainerNode`] holds an
//! object child-table (`name -> value_start`) plus a resume offset; arrays keep
//! a bounded sorted set of `(index, offset)` landmarks so any later index
//! resumes from the nearest landmark at or before it (forward or backward).
//! Nothing here stores source bytes: the burst-window resident bound is untouched.
//!
//! Pure memoization over an immutable source - entries are never invalidated,
//! only evicted for memory (whole-node LRU under a children slot_budget). Depends
//! only on `path` (keys) and `resolve` (`ContainerKind`/`ValueLocation`); never
//! on `session`/`walker`. All reads/writes are driven from `session`, which
//! sits above it.

use std::collections::HashMap;

use crate::path::Segment;
use crate::resolve::{ContainerKind, ResumePoint, ScanRecord, ValueLocation};

/// Per-container cap on tabled object members. A huge object doesn't get an
/// unbounded table; past the cap it stops tabling and keeps only the resume
/// offset, so one giant container can't exhaust the global slot_budget on its own.
const PER_CONTAINER_MEMBERS: usize = 256;

/// Per-array cap on resume landmarks (mirrors [`PER_CONTAINER_MEMBERS`]). Past
/// the cap the set self-coarsens (drops every other landmark), keeping coverage
/// even and per-array memory fixed regardless of document size.
const PER_ARRAY_LANDMARKS: usize = 256;

/// How a cached hop along one path segment resolves.
enum Hop {
  /// O(1) jump into a tabled object member: continue chaining from `value_start`.
  Into(u64),
  /// Stop chaining here; seed the resolver near the target (or `None` to scan
  /// from the container open).
  Stop(Option<ResumePoint>),
}

/// Kind-specific contents of a walked container. A node holds exactly one
/// variant - no empty other-half - so object-only and array-only state never sit
/// side by side. Objects table their members plus a high-water resume offset;
/// arrays keep a sorted set of resume landmarks.
#[derive(Debug)]
enum ContainerBody {
  Object {
    /// Members `name -> value_start` for every member in `[open, resume]` up to
    /// [`PER_CONTAINER_MEMBERS`]. A flat vec: containers are small or capped, so
    /// linear lookup beats a hash map's per-entry overhead.
    members: Vec<(Box<str>, u64)>,
    /// High-water member offset: a later member scan resumes here. Just past the
    /// open for a fresh node.
    resume: u64,
  },
  Array {
    /// Resume landmarks `(index, offset)`, sorted ascending by index, deduped,
    /// capped at [`PER_ARRAY_LANDMARKS`] by self-coarsening.
    landmarks: Vec<(usize, u64)>,
  },
}

impl ContainerBody {
  fn for_kind(kind: ContainerKind, value_start: u64) -> Self {
    match kind {
      // Fresh resume = just past the open `{`, equivalent to scanning from the
      // container start. A fresh array has no landmarks (it scans from the open).
      ContainerKind::Object => ContainerBody::Object {
        members: Vec::new(),
        resume: value_start + 1,
      },
      ContainerKind::Array => ContainerBody::Array {
        landmarks: Vec::new(),
      },
    }
  }

  /// Slot weight of the kind-specific children: one per tabled member / landmark.
  fn slots(&self) -> usize {
    match self {
      ContainerBody::Object { members, .. } => members.len(),
      ContainerBody::Array { landmarks } => landmarks.len(),
    }
  }

  /// `value_start` of a tabled object member, or `None` (untabled, or array body).
  fn member(&self, name: &str) -> Option<u64> {
    match self {
      ContainerBody::Object { members, .. } => members
        .iter()
        .find(|(n, _)| n.as_ref() == name)
        .map(|(_, vs)| *vs),
      ContainerBody::Array { .. } => None,
    }
  }

  /// The greatest array landmark with `index <= target`, or `None` (object body).
  /// Landmarks are kept sorted by index, so this is a binary search.
  fn nearest_landmark(&self, target: usize) -> Option<(usize, u64)> {
    match self {
      ContainerBody::Array { landmarks } => {
        let i = landmarks.partition_point(|&(idx, _)| idx <= target);
        (i > 0).then(|| landmarks[i - 1])
      }
      ContainerBody::Object { .. } => None,
    }
  }

  /// How a hop along `segment` resolves: an O(1) jump into a tabled object member,
  /// or a stop seeding the resolver near the target.
  fn hop(&self, segment: &Segment) -> Hop {
    match (segment, self) {
      (Segment::Member(name), ContainerBody::Object { resume, .. }) => match self.member(name) {
        Some(vs) => Hop::Into(vs),
        // Seed the resolver with the object's high-water resume offset.
        None => Hop::Stop(Some(ResumePoint::Object { offset: *resume })),
      },
      (Segment::Element(idx), ContainerBody::Array { .. }) => Hop::Stop(
        self
          .nearest_landmark(*idx)
          .map(|(index, offset)| ResumePoint::Array { index, offset }),
      ),
      // Kind/segment mismatch: resolve will return None; no seed.
      _ => Hop::Stop(None),
    }
  }

  /// Merge object members (scan order) up to `cap`, advancing the resume offset to
  /// the high-water boundary (`resume_hint` = the matched member's start, or the
  /// close). If the cap is hit, the resume freezes at the first un-tabled member
  /// so every member before it stays tabled. No-op on an array body.
  fn merge_object(&mut self, new: &[(Box<str>, u64, u64)], resume_hint: Option<u64>, cap: usize) {
    let ContainerBody::Object { members, resume } = self else {
      return;
    };
    let mut resume_off = *resume;
    let mut capped = false;
    for (name, key_start, value_start) in new {
      if members.iter().any(|(n, _)| n.as_ref() == name.as_ref()) {
        continue; // already tabled (a prior scan's prefix)
      }
      if members.len() >= cap {
        resume_off = resume_off.max(*key_start);
        capped = true;
        break;
      }
      members.push((name.clone(), *value_start));
    }
    if !capped {
      if let Some(t) = resume_hint {
        resume_off = resume_off.max(t);
      } else if let Some((_, last_start, _)) = new.last() {
        resume_off = resume_off.max(*last_start);
      }
    }
    *resume = resume_off;
  }

  /// Merge `new` landmarks into the sorted set (dedup by index), then enforce
  /// `cap` by self-coarsening - dropping every other landmark, halving coverage
  /// while staying evenly spaced. No-op on an object body.
  fn merge_array(&mut self, new: &[(usize, u64)], cap: usize) {
    let ContainerBody::Array { landmarks } = self else {
      return;
    };
    if new.is_empty() {
      return;
    }
    landmarks.extend_from_slice(new);
    landmarks.sort_unstable_by_key(|&(i, _)| i);
    landmarks.dedup_by_key(|&mut (i, _)| i);
    while landmarks.len() > cap {
      let mut w = 0;
      for r in 0..landmarks.len() {
        if r % 2 == 0 {
          landmarks[w] = landmarks[r];
          w += 1;
        }
      }
      landmarks.truncate(w);
    }
  }
}

/// A walked container: the offsets/scalars learned about it, plus its
/// kind-specific [`ContainerBody`].
#[derive(Debug)]
pub struct ContainerNode {
  value_start: u64,
  close: Option<u64>,
  child_count: Option<u64>,
  body: ContainerBody,
  last_used: u64,
}

impl ContainerNode {
  fn new(kind: ContainerKind, value_start: u64, tick: u64) -> Self {
    Self {
      value_start,
      close: None,
      child_count: None,
      body: ContainerBody::for_kind(kind, value_start),
      last_used: tick,
    }
  }

  pub fn child_count(&self) -> Option<u64> {
    self.child_count
  }

  fn hop_for(&self, segment: &Segment) -> Hop {
    self.body.hop(segment)
  }

  /// `value_start` of a tabled object member, or `None` if not in the table.
  #[cfg(test)]
  pub fn member(&self, name: &str) -> Option<u64> {
    self.body.member(name)
  }

  /// The greatest array landmark with `index <= target`, if any.
  #[cfg(test)]
  pub fn nearest_landmark(&self, target: usize) -> Option<(usize, u64)> {
    self.body.nearest_landmark(target)
  }

  /// The container's full `[start, end)` once its close is known.
  pub fn location(&self) -> Option<ValueLocation> {
    self.close.map(|end| ValueLocation {
      start: self.value_start,
      end,
    })
  }

  /// Weight toward the global slot_budget: one slot for the node itself plus its
  /// body's children (tabled members or landmarks).
  fn slot_cost(&self) -> usize {
    1 + self.body.slots()
  }

  /// Object high-water resume offset (a later member scan resumes here).
  #[cfg(test)]
  pub fn object_resume(&self) -> u64 {
    match &self.body {
      ContainerBody::Object { resume, .. } => *resume,
      ContainerBody::Array { .. } => 0,
    }
  }

  #[cfg(test)]
  pub fn array_landmarks(&self) -> &[(usize, u64)] {
    match &self.body {
      ContainerBody::Array { landmarks } => landmarks,
      ContainerBody::Object { .. } => &[],
    }
  }

  #[cfg(test)]
  pub fn member_count(&self) -> usize {
    match &self.body {
      ContainerBody::Object { members, .. } => members.len(),
      ContainerBody::Array { .. } => 0,
    }
  }
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct NodeKey {
  anchor: u64,
  path: Box<[Segment]>,
}

impl NodeKey {
  fn new(anchor: u64, path: &[Segment]) -> Self {
    Self {
      anchor,
      path: path.into(),
    }
  }
}

/// A flat map of container nodes keyed by `(anchor, path)`, so eviction is
/// per-container with no tree-orphaning.
pub struct StructuralIndex {
  /// Max combined slots (`slots_used`); `0` disables the cache entirely.
  slot_budget: usize,
  /// Current combined slots across all nodes (`sum(1 + members.len())`).
  slots_used: usize,
  /// Monotonic recency clock; the largest value is the most-recently-used.
  tick: u64,
  nodes: HashMap<NodeKey, ContainerNode>,
}

impl StructuralIndex {
  pub fn new(slot_budget: usize) -> Self {
    Self {
      slot_budget,
      slots_used: 0,
      tick: 0,
      nodes: HashMap::new(),
    }
  }

  pub fn is_enabled(&self) -> bool {
    self.slot_budget > 0
  }

  pub fn get(&self, anchor: u64, path: &[Segment]) -> Option<&ContainerNode> {
    self.nodes.get(&NodeKey::new(anchor, path))
  }

  /// Bump a node's recency on a cache hit so hot shallow containers persist.
  pub fn touch(&mut self, anchor: u64, path: &[Segment]) {
    let tick = self.next_tick();
    if let Some(node) = self.nodes.get_mut(&NodeKey::new(anchor, path)) {
      node.last_used = tick;
    }
  }

  /// Walk cached container hops from `anchor` along `path`, returning the deepest
  /// `(start, segment_idx, hint)` to seed the resolver with. Each tabled object
  /// member is an O(1) hop; the first uncached level returns the container's start
  /// plus a resume point (object high-water, or the array landmark nearest at or
  /// before the target) so the resolver resumes near the target instead of at the
  /// open.
  pub fn chain_hops(&mut self, anchor: u64, path: &[Segment]) -> (u64, usize, Option<ResumePoint>) {
    let mut start = anchor;
    let mut seg = 0;
    while seg < path.len() {
      let prefix = &path[..seg];
      // Resolve the hop while borrowing the node, then end the borrow before the
      // recency bump.
      let Some(hop) = self.get(anchor, prefix).map(|n| n.hop_for(&path[seg])) else {
        return (start, seg, None);
      };
      self.touch(anchor, prefix);
      match hop {
        Hop::Into(vs) => {
          start = vs;
          seg += 1; // O(1) hop into a tabled member
        }
        Hop::Stop(seed) => return (start, seg, seed),
      }
    }
    (start, seg, None) // whole path hopped: `start` is the resolved value
  }

  /// Drain a scan's collected child offsets into the cache, one node per entered
  /// container.
  pub fn apply_scan_record(&mut self, anchor: u64, path: &[Segment], scan_record: &ScanRecord) {
    for cs in &scan_record.containers {
      let prefix = &path[..cs.seg];
      match cs.kind {
        ContainerKind::Object => self.merge_object_scan(
          anchor,
          prefix,
          cs.value_start,
          &cs.members,
          cs.object_resume,
        ),
        ContainerKind::Array => {
          self.merge_array_scan(anchor, prefix, cs.value_start, &cs.array_landmarks)
        }
      }
    }
  }

  /// Write back an object scan: merge the members seen (in scan order) up to the
  /// per-container cap, advancing the resume offset to the high-water boundary.
  pub fn merge_object_scan(
    &mut self,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    members: &[(Box<str>, u64, u64)],
    resume: Option<u64>,
  ) {
    let cap = PER_CONTAINER_MEMBERS.min(self.slot_budget);
    self.merge_into(anchor, path, ContainerKind::Object, value_start, |body| {
      body.merge_object(members, resume, cap)
    });
  }

  /// Write back an array scan: merge its `(index, offset)` landmarks into the
  /// node's sorted set (dedup by index), self-coarsening at the per-array cap.
  pub fn merge_array_scan(
    &mut self,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    new_landmarks: &[(usize, u64)],
  ) {
    let cap = PER_ARRAY_LANDMARKS.min(self.slot_budget);
    self.merge_into(anchor, path, ContainerKind::Array, value_start, |body| {
      body.merge_array(new_landmarks, cap)
    });
  }

  /// Get-or-create the `(anchor, path)` node, apply `merge` to its body, and
  /// reconcile `slots_used` for the body's net change in children (the node's own
  /// slot is counted once on creation).
  fn merge_into(
    &mut self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    merge: impl FnOnce(&mut ContainerBody),
  ) {
    if !self.is_enabled() {
      return;
    }
    let tick = self.next_tick();
    {
      let Self {
        nodes, slots_used, ..
      } = self;
      let node = nodes.entry(NodeKey::new(anchor, path)).or_insert_with(|| {
        *slots_used += 1;
        ContainerNode::new(kind, value_start, tick)
      });
      node.last_used = tick;
      let before = node.body.slots();
      merge(&mut node.body);
      *slots_used = *slots_used - before + node.body.slots();
    }
    self.evict_to_slot_budget();
  }

  /// Record a container's matching-close offset (`}`/`]` + 1).
  pub fn store_close(
    &mut self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    close: u64,
  ) {
    self.store_field(anchor, path, kind, value_start, |node| {
      node.close = Some(close)
    });
  }

  pub fn store_child_count(
    &mut self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    count: u64,
  ) {
    self.store_field(anchor, path, kind, value_start, |node| {
      node.child_count = Some(count)
    });
  }

  fn store_field(
    &mut self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    set: impl FnOnce(&mut ContainerNode),
  ) {
    if !self.is_enabled() {
      return;
    }
    let tick = self.next_tick();
    {
      let Self {
        nodes, slots_used, ..
      } = self;
      let node = nodes.entry(NodeKey::new(anchor, path)).or_insert_with(|| {
        *slots_used += 1;
        ContainerNode::new(kind, value_start, tick)
      });
      node.last_used = tick;
      set(node);
    }
    self.evict_to_slot_budget();
  }

  fn next_tick(&mut self) -> u64 {
    self.tick += 1;
    self.tick
  }

  /// Evict least-recently-used whole nodes until the slot_budget holds. Children go
  /// with their node (no orphaning); freshly-written nodes carry the highest
  /// tick and survive over stale ones.
  fn evict_to_slot_budget(&mut self) {
    while self.slots_used > self.slot_budget {
      let Some(victim) = self
        .nodes
        .iter()
        .min_by_key(|(_, n)| n.last_used)
        .map(|(k, _)| k.clone())
      else {
        break;
      };
      if let Some(node) = self.nodes.remove(&victim) {
        self.slots_used -= node.slot_cost();
      }
    }
  }

  #[cfg(test)]
  pub fn slots_used(&self) -> usize {
    self.slots_used
  }

  #[cfg(test)]
  pub fn node_count(&self) -> usize {
    self.nodes.len()
  }

  /// Recompute `slots_used` from scratch; a test invariant that the incremental
  /// accounting matches the actual node weights.
  #[cfg(test)]
  fn recomputed_slots_used(&self) -> usize {
    self.nodes.values().map(ContainerNode::slot_cost).sum()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn member(name: &str, start: u64, value_start: u64) -> (Box<str>, u64, u64) {
    (name.into(), start, value_start)
  }

  fn obj_path(name: &str) -> Vec<Segment> {
    vec![Segment::Member(name.into())]
  }

  #[test]
  fn disabled_cache_records_nothing() {
    let mut c = StructuralIndex::new(0);
    assert!(!c.is_enabled());
    c.merge_object_scan(0, &[], 0, &[member("a", 1, 4)], Some(1));
    c.store_close(0, &[], ContainerKind::Object, 0, 10);
    assert!(c.get(0, &[]).is_none());
    assert_eq!(c.slots_used(), 0);
  }

  #[test]
  fn object_table_covers_open_to_high_water() {
    let mut c = StructuralIndex::new(64);
    // {"a":1,"b":2,"c":3} - scan matched "c"; a,b skipped, c matched. Terminal is
    // c's member-start (the high-water).
    let members = [member("a", 1, 5), member("b", 7, 11), member("c", 13, 17)];
    c.merge_object_scan(0, &[], 0, &members, Some(13));
    let node = c.get(0, &[]).expect("node");
    assert_eq!(node.member("a"), Some(5));
    assert_eq!(node.member("b"), Some(11));
    assert_eq!(node.member("c"), Some(17));
    assert_eq!(node.member("d"), None);
    // The resume offset sits at the matched member's start, so any un-tabled
    // member is at or after it and a resume from there finds it.
    assert_eq!(node.object_resume(), 13);
    assert_eq!(c.slots_used(), 1 + 3);
  }

  #[test]
  fn object_sibling_scans_extend_contiguously() {
    let mut c = StructuralIndex::new(64);
    // First scan tables a,b and matched b (resume = b's start = 7).
    c.merge_object_scan(0, &[], 0, &[member("a", 1, 5), member("b", 7, 11)], Some(7));
    assert_eq!(c.get(0, &[]).unwrap().object_resume(), 7);
    // Second scan resumes at 7, re-reads b, then c,d; matched d (resume = d's start).
    c.merge_object_scan(
      0,
      &[],
      0,
      &[member("b", 7, 11), member("c", 13, 17), member("d", 19, 23)],
      Some(19),
    );
    let node = c.get(0, &[]).unwrap();
    assert_eq!(node.member("c"), Some(17));
    assert_eq!(node.member("d"), Some(23));
    assert_eq!(node.object_resume(), 19);
    // b not double-counted.
    assert_eq!(c.slots_used(), 1 + 4);
  }

  #[test]
  fn per_container_cap_spills_to_resume_point() {
    let slot_budget = 1024;
    let mut c = StructuralIndex::new(slot_budget);
    let members: Vec<_> = (0..PER_CONTAINER_MEMBERS + 10)
      .map(|i| member(&format!("k{i}"), (i * 10) as u64, (i * 10 + 4) as u64))
      .collect();
    c.merge_object_scan(0, &[], 0, &members, Some(99_999));
    let node = c.get(0, &[]).unwrap();
    assert_eq!(
      node.member_count(),
      PER_CONTAINER_MEMBERS,
      "table capped at the per-container limit"
    );
    // The resume offset froze at the first un-tabled member's start, not the resume.
    let first_untabled_start = (PER_CONTAINER_MEMBERS * 10) as u64;
    assert_eq!(node.object_resume(), first_untabled_start);
  }

  #[test]
  fn array_landmarks_merge_sorted_and_deduped() {
    let mut c = StructuralIndex::new(64);
    c.merge_array_scan(0, &[], 0, &[(5, 50), (2, 20), (9, 90)]);
    assert_eq!(
      c.get(0, &[]).unwrap().array_landmarks(),
      &[(2, 20), (5, 50), (9, 90)]
    );
    assert_eq!(c.slots_used(), 1 + 3);
    // A second scan merges new indices and dedups a repeat (same index).
    c.merge_array_scan(0, &[], 0, &[(2, 20), (7, 70)]);
    assert_eq!(
      c.get(0, &[]).unwrap().array_landmarks(),
      &[(2, 20), (5, 50), (7, 70), (9, 90)]
    );
    assert_eq!(
      c.slots_used(),
      1 + 4,
      "the duplicate index 2 is not double-counted"
    );
  }

  #[test]
  fn chain_hops_array_seeds_nearest_at_or_below_target() {
    let mut c = StructuralIndex::new(64);
    // Root container is the array (prefix `[]`); landmarks at 2, 5, 9.
    c.merge_array_scan(0, &[], 0, &[(2, 20), (5, 50), (9, 90)]);

    // Forward: target 7 resumes from the nearest at or below (index 5).
    let (start, seg, seed) = c.chain_hops(0, &[Segment::Element(7)]);
    assert_eq!((start, seg), (0, 0));
    assert_eq!(
      seed,
      Some(ResumePoint::Array {
        index: 5,
        offset: 50
      })
    );

    // Backward: target 3 resumes from index 2 - impossible under the old
    // forward-only landmark, which would have parked at 9 and rescanned from open.
    let (_, _, back) = c.chain_hops(0, &[Segment::Element(3)]);
    assert_eq!(
      back,
      Some(ResumePoint::Array {
        index: 2,
        offset: 20
      })
    );

    // No landmark at or below 1: scan from the open.
    let (_, _, none) = c.chain_hops(0, &[Segment::Element(1)]);
    assert_eq!(none, None);
  }

  #[test]
  fn per_array_cap_self_coarsens_and_accounts() {
    let slot_budget = 4096;
    let mut c = StructuralIndex::new(slot_budget);
    let lms: Vec<(usize, u64)> = (0..PER_ARRAY_LANDMARKS + 100)
      .map(|i| (i, (i * 10) as u64))
      .collect();
    c.merge_array_scan(0, &[], 0, &lms);
    let got = c.get(0, &[]).unwrap().array_landmarks();
    assert!(
      got.len() <= PER_ARRAY_LANDMARKS,
      "coarsened to the per-array cap, got {}",
      got.len()
    );
    // Coverage stays evenly spaced and sorted, anchored at the first landmark.
    assert_eq!(got[0].0, 0);
    assert!(
      got.windows(2).all(|w| w[0].0 < w[1].0),
      "indices stay sorted/unique"
    );
    assert_eq!(
      c.slots_used(),
      c.recomputed_slots_used(),
      "slot accounting matches the coarsened landmark count"
    );
  }

  #[test]
  fn scalars_close_and_count_and_location() {
    let mut c = StructuralIndex::new(64);
    c.store_close(0, &[], ContainerKind::Array, 0, 42);
    c.store_child_count(0, &[], ContainerKind::Array, 0, 7);
    let node = c.get(0, &[]).unwrap();
    assert_eq!(node.child_count(), Some(7));
    assert_eq!(node.location(), Some(ValueLocation { start: 0, end: 42 }));
  }

  #[test]
  fn whole_node_lru_keeps_held_within_budget() {
    let slot_budget = 50;
    let mut c = StructuralIndex::new(slot_budget);
    // Insert many distinct object nodes, each with a few members.
    for i in 0..200u64 {
      let p = obj_path(&format!("path{i}"));
      let members = [
        member("x", i * 100, i * 100 + 4),
        member("y", i * 100 + 6, i * 100 + 10),
      ];
      c.merge_object_scan(i, &p, i * 100, &members, Some(i * 100));
      assert!(
        c.slots_used() <= slot_budget,
        "slots_used {} exceeded slot_budget {slot_budget} at i={i}",
        c.slots_used()
      );
    }
    assert_eq!(
      c.slots_used(),
      c.recomputed_slots_used(),
      "slots_used accounting drifted"
    );
    // The most-recently-written node survives.
    let last = obj_path("path199");
    assert!(c.get(199, &last).is_some());
  }

  #[test]
  fn cache_stays_bounded_under_many_queries() {
    let slot_budget = 128;
    let mut c = StructuralIndex::new(slot_budget);
    for round in 0..50u64 {
      for i in 0..64u64 {
        let p = obj_path(&format!("c{i}"));
        c.merge_object_scan(
          0,
          &p,
          i * 1000,
          &[member("f", i * 1000, i * 1000 + 4)],
          Some(i * 1000),
        );
        c.merge_array_scan(round, &p, i * 7, &[(round as usize, i * 7)]);
      }
      assert!(
        c.slots_used() <= slot_budget,
        "slots_used {} over slot_budget",
        c.slots_used()
      );
      assert!(c.node_count() <= slot_budget, "node_count over slot_budget");
    }
    assert_eq!(c.slots_used(), c.recomputed_slots_used());
  }
}
