//! Structural-index cache: a bounded, lazily-built partial skeleton of the
//! document.
//!
//! Caches the *containers* a scan has walked, not whole resolved paths, so a
//! later query landing in an already-entered container starts near the target
//! instead of at the container's open. Each [`ContainerNode`] holds an object
//! child-table (`name -> value_start`) plus a resume offset; arrays keep a sorted
//! set of `(index, offset)` members so any later index resumes from the nearest
//! member at or before it (forward or backward). Nothing here stores source
//! bytes, so the burst-window resident bound is untouched.
//!
//! Memoization over an immutable source: entries are never invalidated, only
//! evicted for memory. Three knobs bound the footprint. Two cap a single node:
//! `object_member_cap` caps an object's tabled members as a dense prefix (past
//! the cap the resume offset freezes at the first un-tabled member, so a later
//! lookup still resumes correctly), and `array_interval` is the element stride
//! between sampled array members (applied upstream in the walker). The third,
//! `slot_budget`, caps the *number* of nodes - without it, a full scan of an
//! array of N objects would mint N nodes and never free them.
//!
//! Eviction is **document-depth-first** (deepest container evicted first, LRU
//! tiebreak): the shallow navigational backbone - a big array's member node, a
//! top-level object's table - is reachable from many future queries; deep nodes
//! are narrowly reachable and shed first. Each node carries its document depth
//! (`base_depth + path.len()`) because `iter`/`walk` re-anchor children at their
//! own byte offset, so the relative `NodeKey.path` length does *not* reflect tree
//! depth.

use std::collections::{hash_map::Entry, HashMap};

use crate::path::Segment;
use crate::resolve::{ContainerKind, ResumePoint, ScanRecord, ValueLocation};

/// How a cached hop along one path segment resolves.
enum Hop {
  /// O(1) jump into a tabled object member: chain on from `value_start`.
  Into(u64),
  /// Stop chaining; seed the resolver near the target (or `None` to scan from the
  /// container open).
  Stop(Option<ResumePoint>),
}

#[derive(Debug)]
enum ContainerBody {
  Object {
    members: Vec<(Box<str>, u64)>,
    // High-water member offset: a later member scan resumes here.
    resume: u64,
  },
  Array {
    /// Resume members `(index, offset)`, sorted ascending by index, deduped.
    members: Vec<(usize, u64)>,
  },
}

impl ContainerBody {
  fn for_kind(kind: ContainerKind, value_start: u64) -> Self {
    match kind {
      // Fresh resume = just past the open `{`, i.e. scan from the container start.
      ContainerKind::Object => ContainerBody::Object {
        members: Vec::new(),
        resume: value_start + 1,
      },
      ContainerKind::Array => ContainerBody::Array {
        members: Vec::new(),
      },
    }
  }

  /// `value_start` of a tabled object member, or `None` (untabled, or array body).
  fn object_member(&self, name: &str) -> Option<u64> {
    match self {
      ContainerBody::Object { members, .. } => members
        .iter()
        .find(|(n, _)| n.as_ref() == name)
        .map(|(_, vs)| *vs),
      ContainerBody::Array { .. } => None,
    }
  }

  /// The greatest array member with `index <= target`, or `None` (object body).
  /// Binary search: array members are kept sorted by index.
  fn nearest_array_member(&self, target: usize) -> Option<(usize, u64)> {
    match self {
      ContainerBody::Array { members } => {
        let i = members.partition_point(|&(idx, _)| idx <= target);
        (i > 0).then(|| members[i - 1])
      }
      ContainerBody::Object { .. } => None,
    }
  }

  /// How a hop along `segment` resolves.
  fn hop(&self, segment: &Segment) -> Hop {
    match (segment, self) {
      (Segment::Member(name), ContainerBody::Object { resume, .. }) => {
        match self.object_member(name) {
          Some(vs) => Hop::Into(vs),
          // The table covers the dense prefix `[open, resume]`, so an un-tabled
          // member is at or after `resume` - resuming there is always correct.
          None => Hop::Stop(Some(ResumePoint::Object { offset: *resume })),
        }
      }
      (Segment::Element(idx), ContainerBody::Array { .. }) => Hop::Stop(
        self
          .nearest_array_member(*idx)
          .map(|(index, offset)| ResumePoint::Array { index, offset }),
      ),
      // Kind/segment mismatch: resolve will return None; no seed.
      _ => Hop::Stop(None),
    }
  }

  /// Merge object members (scan order) up to `cap`, advancing the resume offset to
  /// the high-water boundary (`resume_hint` = matched member's start, or the
  /// close). On hitting the cap the resume freezes at the first un-tabled member,
  /// keeping the table a dense prefix. No-op on an array body.
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

  /// Merge `new` array members into the sorted set (dedup by index). Unbounded
  /// here: the bounding stride is applied upstream at sampling time. No-op on an
  /// object body.
  fn merge_array(&mut self, new: &[(usize, u64)]) {
    let ContainerBody::Array { members } = self else {
      return;
    };
    if new.is_empty() {
      return;
    }
    members.extend_from_slice(new);
    members.sort_unstable_by_key(|&(i, _)| i);
    members.dedup_by_key(|&mut (i, _)| i);
  }
}

/// A walked container: the offsets/scalars learned about it, plus its
/// kind-specific [`ContainerBody`].
#[derive(Debug)]
pub struct ContainerNode {
  value_start: u64,
  close: Option<u64>,
  child_count: Option<u64>,
  /// Document-tree depth (`base_depth + path.len()`). Primary eviction key:
  /// deepest evicted first.
  depth: u32,
  /// Recency clock at the last touch/write; LRU tiebreak within a depth.
  last_used: u64,
  body: ContainerBody,
}

impl ContainerNode {
  fn new(kind: ContainerKind, value_start: u64, depth: u32, last_used: u64) -> Self {
    Self {
      value_start,
      close: None,
      child_count: None,
      depth,
      last_used,
      body: ContainerBody::for_kind(kind, value_start),
    }
  }

  pub fn child_count(&self) -> Option<u64> {
    self.child_count
  }

  /// Weight toward `slot_budget`: one slot for the node plus one per tabled object
  /// member. Array members aren't counted - `array_interval` already bounds them.
  fn slot_cost(&self) -> usize {
    1 + match &self.body {
      ContainerBody::Object { members, .. } => members.len(),
      ContainerBody::Array { .. } => 0,
    }
  }

  fn hop_for(&self, segment: &Segment) -> Hop {
    self.body.hop(segment)
  }

  /// `value_start` of a tabled object member, or `None` if not in the table.
  #[cfg(test)]
  pub fn object_member(&self, name: &str) -> Option<u64> {
    self.body.object_member(name)
  }

  /// The greatest array member with `index <= target`, if any.
  #[cfg(test)]
  pub fn nearest_array_member(&self, target: usize) -> Option<(usize, u64)> {
    self.body.nearest_array_member(target)
  }

  /// The container's full `[start, end)` once its close is known.
  pub fn location(&self) -> Option<ValueLocation> {
    self.close.map(|end| ValueLocation {
      start: self.value_start,
      end,
    })
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
  pub fn array_members(&self) -> &[(usize, u64)] {
    match &self.body {
      ContainerBody::Array { members } => members,
      ContainerBody::Object { .. } => &[],
    }
  }

  #[cfg(test)]
  pub fn object_member_count(&self) -> usize {
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

/// A flat map of container nodes keyed by `(anchor, path)`, bounded by a global
/// `slot_budget` with depth-first eviction. No invalidation (the source is
/// immutable); nodes only leave via eviction.
pub struct StructuralIndex {
  /// Max combined slots before eviction kicks in; `0` disables the cache.
  slot_budget: usize,
  /// `sum(slot_cost)` across all nodes, tracked incrementally on every write.
  slots_used: usize,
  /// Monotonic recency clock; the largest value is the most-recently-used.
  tick: u64,
  /// Max tabled members per object (`usize::MAX` = unbounded; `0` tables none,
  /// resume parks at the open).
  object_member_cap: usize,
  /// Element stride between sampled array members (`0` = none). Held for
  /// `is_enabled`; the stride itself is applied at sampling time.
  array_interval: usize,
  nodes: HashMap<NodeKey, ContainerNode>,
}

impl StructuralIndex {
  pub fn new(slot_budget: usize, object_member_cap: usize, array_interval: usize) -> Self {
    Self {
      slot_budget,
      slots_used: 0,
      tick: 0,
      object_member_cap,
      array_interval,
      nodes: HashMap::new(),
    }
  }

  pub fn is_enabled(&self) -> bool {
    self.slot_budget > 0 && (self.object_member_cap > 0 || self.array_interval > 0)
  }

  fn next_tick(&mut self) -> u64 {
    self.tick += 1;
    self.tick
  }

  pub fn get(&self, anchor: u64, path: &[Segment]) -> Option<&ContainerNode> {
    self.nodes.get(&NodeKey::new(anchor, path))
  }

  /// Walk cached container hops from `anchor` along `path`, returning the deepest
  /// `(start, segment_idx, hint)` to seed the resolver with. Each tabled object
  /// member is an O(1) hop; the first uncached level returns its container's start
  /// plus a resume point so the resolver picks up near the target, not at the open.
  pub fn chain_hops(&mut self, anchor: u64, path: &[Segment]) -> (u64, usize, Option<ResumePoint>) {
    let mut start = anchor;
    let mut seg = 0;
    while seg < path.len() {
      let key = NodeKey::new(anchor, &path[..seg]);
      let Some(hop) = self.nodes.get(&key).map(|n| n.hop_for(&path[seg])) else {
        return (start, seg, None);
      };
      // Bump recency on the hit so hot containers outlive stale ones in their tier.
      let tick = self.next_tick();
      if let Some(node) = self.nodes.get_mut(&key) {
        node.last_used = tick;
      }
      match hop {
        Hop::Into(vs) => {
          start = vs;
          seg += 1;
        }
        Hop::Stop(seed) => return (start, seg, seed),
      }
    }
    (start, seg, None) // whole path hopped: `start` is the resolved value
  }

  /// Drain a scan's collected child offsets into the cache, one node per entered
  /// container.
  pub fn apply_scan_record(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    scan_record: &ScanRecord,
  ) {
    for cs in &scan_record.containers {
      let prefix = &path[..cs.seg];
      match cs.kind {
        ContainerKind::Object => self.merge_object_scan(
          base_depth,
          anchor,
          prefix,
          cs.value_start,
          &cs.members,
          cs.object_resume,
        ),
        ContainerKind::Array => self.merge_array_scan(
          base_depth,
          anchor,
          prefix,
          cs.value_start,
          &cs.array_members,
        ),
      }
    }
  }

  /// Write back an object scan: merge the members seen (scan order) up to the cap,
  /// advancing the resume offset to the high-water boundary.
  pub fn merge_object_scan(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    members: &[(Box<str>, u64, u64)],
    resume: Option<u64>,
  ) {
    let cap = self.object_member_cap;
    self.merge_into(
      base_depth,
      anchor,
      path,
      ContainerKind::Object,
      value_start,
      |body| body.merge_object(members, resume, cap),
    );
  }

  /// Write back an array scan: merge its `(index, offset)` array members into the
  /// node's sorted set.
  pub fn merge_array_scan(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    new_members: &[(usize, u64)],
  ) {
    self.merge_into(
      base_depth,
      anchor,
      path,
      ContainerKind::Array,
      value_start,
      |body| body.merge_array(new_members),
    );
  }

  /// Get-or-create the `(anchor, path)` node and apply `merge` to its body,
  /// keeping `slots_used` in step and evicting if the write tips over budget.
  fn merge_into(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    merge: impl FnOnce(&mut ContainerBody),
  ) {
    if !self.is_enabled() {
      return;
    }
    let depth = base_depth + path.len() as u32;
    let tick = self.next_tick();
    let delta = {
      let (before, node) = match self.nodes.entry(NodeKey::new(anchor, path)) {
        Entry::Occupied(e) => {
          let n = e.into_mut();
          (n.slot_cost(), n)
        }
        Entry::Vacant(e) => (
          0,
          e.insert(ContainerNode::new(kind, value_start, depth, tick)),
        ),
      };
      merge(&mut node.body);
      node.last_used = tick;
      node.slot_cost() - before
    };
    self.slots_used += delta;
    self.evict_if_over_budget();
  }

  /// Depth-first batch eviction: when over `slot_budget`, drop nodes deepest-first
  /// (LRU tiebreak) down to a low-water mark (~7/8 budget) in one sorted pass.
  /// Batching keeps the amortized cost ~O(log n) per insert instead of an O(n)
  /// victim scan per evicted node; shallow backbone nodes are shed last.
  fn evict_if_over_budget(&mut self) {
    if self.slot_budget == 0 || self.slots_used <= self.slot_budget {
      return;
    }
    let low_water = self.slot_budget - self.slot_budget / 8;
    // Best victim first: greatest depth, then least recently used within it.
    let mut victims: Vec<(u32, u64, NodeKey)> = self
      .nodes
      .iter()
      .map(|(k, n)| (n.depth, n.last_used, k.clone()))
      .collect();
    victims.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, _, key) in victims {
      if self.slots_used <= low_water {
        break;
      }
      if let Some(node) = self.nodes.remove(&key) {
        self.slots_used -= node.slot_cost();
      }
    }
  }

  /// Record a container's matching-close offset (`}`/`]` + 1).
  pub fn store_close(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    close: u64,
  ) {
    self.store_field(base_depth, anchor, path, kind, value_start, |node| {
      node.close = Some(close)
    });
  }

  pub fn store_child_count(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    count: u64,
  ) {
    self.store_field(base_depth, anchor, path, kind, value_start, |node| {
      node.child_count = Some(count)
    });
  }

  fn store_field(
    &mut self,
    base_depth: u32,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    set: impl FnOnce(&mut ContainerNode),
  ) {
    if !self.is_enabled() {
      return;
    }
    let depth = base_depth + path.len() as u32;
    let tick = self.next_tick();
    let delta = {
      let (before, node) = match self.nodes.entry(NodeKey::new(anchor, path)) {
        Entry::Occupied(e) => {
          let n = e.into_mut();
          (n.slot_cost(), n)
        }
        Entry::Vacant(e) => (
          0,
          e.insert(ContainerNode::new(kind, value_start, depth, tick)),
        ),
      };
      set(node);
      node.last_used = tick;
      node.slot_cost() - before
    };
    self.slots_used += delta;
    self.evict_if_over_budget();
  }

  #[cfg(test)]
  pub fn node_count(&self) -> usize {
    self.nodes.len()
  }

  #[cfg(test)]
  pub fn slots_used(&self) -> usize {
    self.slots_used
  }

  /// Recompute `slots_used` from scratch, to check the incremental accounting.
  #[cfg(test)]
  fn recomputed_slots_used(&self) -> usize {
    self.nodes.values().map(ContainerNode::slot_cost).sum()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Big enough that the table/array-member tests never trip eviction.
  const NO_EVICT: usize = 1 << 20;

  fn member(name: &str, start: u64, value_start: u64) -> (Box<str>, u64, u64) {
    (name.into(), start, value_start)
  }

  fn obj_path(name: &str) -> Vec<Segment> {
    vec![Segment::Member(name.into())]
  }

  #[test]
  fn object_table_covers_open_to_high_water() {
    let mut c = StructuralIndex::new(NO_EVICT, 64, 16);
    // {"a":1,"b":2,"c":3} - scan matched "c"; a,b skipped, c matched. Terminal is
    // c's member-start (the high-water).
    let members = [member("a", 1, 5), member("b", 7, 11), member("c", 13, 17)];
    c.merge_object_scan(0, 0, &[], 0, &members, Some(13));
    let node = c.get(0, &[]).expect("node");
    assert_eq!(node.object_member("a"), Some(5));
    assert_eq!(node.object_member("b"), Some(11));
    assert_eq!(node.object_member("c"), Some(17));
    assert_eq!(node.object_member("d"), None);
    // Resume sits at the matched member's start, so any un-tabled member is at or
    // after it - a resume from there finds it.
    assert_eq!(node.object_resume(), 13);
    assert_eq!(node.object_member_count(), 3);
  }

  #[test]
  fn object_sibling_scans_extend_contiguously() {
    let mut c = StructuralIndex::new(NO_EVICT, 64, 16);
    // First scan tables a,b and matched b (resume = b's start = 7).
    c.merge_object_scan(
      0,
      0,
      &[],
      0,
      &[member("a", 1, 5), member("b", 7, 11)],
      Some(7),
    );
    assert_eq!(c.get(0, &[]).unwrap().object_resume(), 7);
    // Second scan resumes at 7, re-reads b, then c,d; matched d (resume = d's start).
    c.merge_object_scan(
      0,
      0,
      &[],
      0,
      &[member("b", 7, 11), member("c", 13, 17), member("d", 19, 23)],
      Some(19),
    );
    let node = c.get(0, &[]).unwrap();
    assert_eq!(node.object_member("c"), Some(17));
    assert_eq!(node.object_member("d"), Some(23));
    assert_eq!(node.object_resume(), 19);
    // b not double-counted.
    assert_eq!(node.object_member_count(), 4);
    // Slot accounting tracks the growing table (node + 4 members).
    assert_eq!(c.slots_used(), 5);
    assert_eq!(c.slots_used(), c.recomputed_slots_used());
  }

  #[test]
  fn object_cap_spills_to_resume_point() {
    let cap = 8;
    let mut c = StructuralIndex::new(NO_EVICT, cap, 16);
    let members: Vec<_> = (0..cap + 10)
      .map(|i| member(&format!("k{i}"), (i * 10) as u64, (i * 10 + 4) as u64))
      .collect();
    c.merge_object_scan(0, 0, &[], 0, &members, Some(99_999));
    let node = c.get(0, &[]).unwrap();
    assert_eq!(
      node.object_member_count(),
      cap,
      "table capped at object_member_cap"
    );
    // The resume offset froze at the first un-tabled member's start, not the resume.
    let first_untabled_start = (cap * 10) as u64;
    assert_eq!(node.object_resume(), first_untabled_start);
  }

  #[test]
  fn object_cap_unbounded_tables_all() {
    let mut c = StructuralIndex::new(NO_EVICT, usize::MAX, 16);
    let members: Vec<_> = (0..500)
      .map(|i| member(&format!("k{i}"), (i * 10) as u64, (i * 10 + 4) as u64))
      .collect();
    c.merge_object_scan(0, 0, &[], 0, &members, Some(99_999));
    assert_eq!(
      c.get(0, &[]).unwrap().object_member_count(),
      500,
      "the unbounded default tables every member"
    );
  }

  #[test]
  fn array_members_merge_sorted_and_deduped() {
    let mut c = StructuralIndex::new(NO_EVICT, 64, 16);
    c.merge_array_scan(0, 0, &[], 0, &[(5, 50), (2, 20), (9, 90)]);
    assert_eq!(
      c.get(0, &[]).unwrap().array_members(),
      &[(2, 20), (5, 50), (9, 90)]
    );
    // A second scan merges new indices and dedups a repeat (same index).
    c.merge_array_scan(0, 0, &[], 0, &[(2, 20), (7, 70)]);
    assert_eq!(
      c.get(0, &[]).unwrap().array_members(),
      &[(2, 20), (5, 50), (7, 70), (9, 90)]
    );
    // Array members don't count toward slots: one array node is one slot.
    assert_eq!(c.slots_used(), 1);
  }

  #[test]
  fn array_members_unbounded() {
    let mut c = StructuralIndex::new(NO_EVICT, 64, 16);
    let lms: Vec<(usize, u64)> = (0..1000).map(|i| (i, (i * 10) as u64)).collect();
    c.merge_array_scan(0, 0, &[], 0, &lms);
    let got = c.get(0, &[]).unwrap().array_members();
    assert_eq!(
      got.len(),
      1000,
      "no cap/coarsen: every array member is kept"
    );
    assert!(
      got.windows(2).all(|w| w[0].0 < w[1].0),
      "indices stay sorted/unique"
    );
  }

  #[test]
  fn chain_hops_array_seeds_nearest_at_or_below_target() {
    let mut c = StructuralIndex::new(NO_EVICT, 64, 16);
    // Root container is the array (prefix `[]`); array members at 2, 5, 9.
    c.merge_array_scan(0, 0, &[], 0, &[(2, 20), (5, 50), (9, 90)]);

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

    // Backward: target 3 resumes from index 2 - impossible under a single
    // forward-only member, which would have parked at 9 and rescanned from open.
    let (_, _, back) = c.chain_hops(0, &[Segment::Element(3)]);
    assert_eq!(
      back,
      Some(ResumePoint::Array {
        index: 2,
        offset: 20
      })
    );

    // No array member at or below 1: scan from the open.
    let (_, _, none) = c.chain_hops(0, &[Segment::Element(1)]);
    assert_eq!(none, None);
  }

  #[test]
  fn scalars_close_and_count_and_location() {
    let mut c = StructuralIndex::new(NO_EVICT, 64, 16);
    c.store_close(0, 0, &[], ContainerKind::Array, 0, 42);
    c.store_child_count(0, 0, &[], ContainerKind::Array, 0, 7);
    let node = c.get(0, &[]).unwrap();
    assert_eq!(node.child_count(), Some(7));
    assert_eq!(node.location(), Some(ValueLocation { start: 0, end: 42 }));
  }

  #[test]
  fn disabled_when_budget_zero_or_caps_zero() {
    // Budget 0 disables regardless of the per-container caps.
    let mut c = StructuralIndex::new(0, 64, 16);
    assert!(!c.is_enabled());
    c.merge_object_scan(0, 0, &[], 0, &[member("a", 1, 4)], Some(1));
    c.store_close(0, 0, &[], ContainerKind::Object, 0, 10);
    assert!(c.get(0, &[]).is_none());
    // Both per-container caps 0 disables even with a budget (the off switch).
    let mut c = StructuralIndex::new(NO_EVICT, 0, 0);
    assert!(!c.is_enabled());
    c.merge_object_scan(0, 0, &[], 0, &[member("a", 1, 4)], Some(1));
    assert!(c.get(0, &[]).is_none());
  }

  #[test]
  fn budget_bounds_slots_used() {
    let budget = 64;
    let mut c = StructuralIndex::new(budget, usize::MAX, 16);
    // Distinct single-member nodes, all at the same depth so eviction is purely
    // by recency.
    for i in 0..200u64 {
      let p = obj_path(&format!("path{i}"));
      c.merge_object_scan(
        0,
        i,
        &p,
        i * 100,
        &[member("x", i * 100, i * 100 + 4)],
        Some(i * 100),
      );
      assert!(
        c.slots_used() <= budget,
        "slots_used {} exceeded budget {budget} at i={i}",
        c.slots_used()
      );
    }
    assert_eq!(c.slots_used(), c.recomputed_slots_used());
    assert!(c.node_count() <= budget);
    assert!(
      c.get(199, &obj_path("path199")).is_some(),
      "recent node kept"
    );
    assert!(c.get(0, &obj_path("path0")).is_none(), "stale node evicted");
  }

  #[test]
  fn eviction_is_depth_first_keeping_the_shallow_backbone() {
    let budget = 8;
    let mut c = StructuralIndex::new(budget, usize::MAX, 16);
    // One shallow backbone node (depth 0): a root array with array members.
    c.merge_array_scan(0, 0, &[], 0, &[(0, 0), (16, 160)]);
    // Flood deep nodes (depth 2): re-anchored element objects, tagged base_depth 2
    // the way iter/walk would.
    for i in 0..100u64 {
      c.merge_object_scan(
        2,
        i + 1,
        &[],
        i * 100,
        &[member("name", i * 100, i * 100 + 7)],
        Some(i * 100),
      );
      assert!(c.slots_used() <= budget);
    }
    // Never the victim while deeper nodes exist, so it survives the flood.
    let backbone = c.get(0, &[]).expect("shallow backbone node must survive");
    assert_eq!(backbone.array_members(), &[(0, 0), (16, 160)]);
    assert!(c.node_count() <= budget);
  }
}
