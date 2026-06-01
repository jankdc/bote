//! Structural-index cache: a bounded, lazily-built partial skeleton of the
//! document.
//!
//! Caches the *containers* a scan has walked - not whole resolved paths - so a
//! later query that lands in a container we've already entered starts near the
//! target instead of at the container's open. Each [`ContainerNode`] holds an
//! object child-table (`name -> value_start`) plus a resume frontier; arrays
//! keep a single `(index, offset)` landmark. Nothing here stores source bytes:
//! the burst-window resident bound is untouched.
//!
//! Pure memoization over an immutable source - entries are never invalidated,
//! only evicted for memory (whole-node LRU under a children budget). Depends
//! only on `path` (keys) and `resolve` (`ContainerKind`/`ValueLocation`); never
//! on `session`/`walker`. All reads/writes are driven from `session`, which
//! sits above it.

use std::collections::HashMap;

use crate::path::Segment;
use crate::resolve::{ContainerKind, ValueLocation};

/// Per-container cap on tabled object members. A huge object doesn't get an
/// unbounded table; past the cap it stops tabling and keeps only the frontier,
/// so one giant container can't exhaust the global budget on its own.
const PER_CONTAINER_MEMBERS: usize = 256;

/// Resume landmark for a container: where a scan that re-enters it should pick
/// up. Object frontiers carry only the high-water member-start offset (member
/// scanning threads no carry); array frontiers carry the `(index, offset)` of
/// the furthest resolved element, to seed the comma popcount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontier {
  Object { offset: u64 },
  Array { index: usize, offset: u64 },
}

/// A walked container: the offsets of the children seen so far, the scalars
/// learned about it, and a resume point. `members` is empty for arrays.
#[derive(Debug)]
pub struct ContainerNode {
  kind: ContainerKind,
  value_start: u64,
  close: Option<u64>,
  child_count: Option<u64>,
  /// Object members `name -> value_start`, for every member in `[open, frontier]`
  /// up to the per-container cap. A flat vec: containers are small or capped, so
  /// linear lookup is cheaper than a hash map's per-entry overhead.
  members: Vec<(Box<str>, u64)>,
  frontier: Frontier,
  last_used: u64,
}

impl ContainerNode {
  pub fn kind(&self) -> ContainerKind {
    self.kind
  }

  pub fn child_count(&self) -> Option<u64> {
    self.child_count
  }

  pub fn frontier(&self) -> Frontier {
    self.frontier
  }

  /// `value_start` of a tabled member, or `None` if not in the table.
  pub fn member(&self, name: &str) -> Option<u64> {
    self
      .members
      .iter()
      .find(|(n, _)| n.as_ref() == name)
      .map(|(_, vs)| *vs)
  }

  /// The container's full `[start, end)` once its close is known.
  pub fn location(&self) -> Option<ValueLocation> {
    self.close.map(|end| ValueLocation {
      start: self.value_start,
      end,
    })
  }

  /// Weight toward the global budget: one slot for the node itself plus one per
  /// tabled child offset.
  fn weight(&self) -> usize {
    1 + self.members.len()
  }
}

/// Default resume point for a freshly-created node with no scan history: just
/// past the open `{`/`[`. Equivalent to scanning from the container's start
/// (member scans `skip_whitespace` first, comma popcounts start at depth 0).
fn fresh_frontier(kind: ContainerKind, value_start: u64) -> Frontier {
  match kind {
    ContainerKind::Object => Frontier::Object {
      offset: value_start + 1,
    },
    ContainerKind::Array => Frontier::Array {
      index: 0,
      offset: value_start + 1,
    },
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
pub struct IndexCache {
  /// Max combined slots (`held`); `0` disables the cache entirely.
  budget: usize,
  /// Current combined slots across all nodes (`sum(1 + members.len())`).
  held: usize,
  /// Monotonic recency clock; the largest value is the most-recently-used.
  tick: u64,
  nodes: HashMap<NodeKey, ContainerNode>,
}

impl IndexCache {
  pub fn new(budget: usize) -> Self {
    Self {
      budget,
      held: 0,
      tick: 0,
      nodes: HashMap::new(),
    }
  }

  pub fn is_enabled(&self) -> bool {
    self.budget > 0
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

  /// Write back an object scan: merge the members seen (in scan order) up to the
  /// per-container cap, then advance the frontier to the high-water resume point
  /// (`terminal` = the matched member's start, or the close position). If the
  /// cap is hit, the frontier freezes at the first un-tabled member so every
  /// member before it stays tabled - the resume-correctness invariant.
  pub fn record_object_scan(
    &mut self,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    members: &[(Box<str>, u64, u64)],
    terminal: Option<u64>,
  ) {
    if !self.is_enabled() {
      return;
    }
    let tick = self.next_tick();
    let cap = PER_CONTAINER_MEMBERS.min(self.budget);
    {
      let Self { nodes, held, .. } = self;
      let node = nodes.entry(NodeKey::new(anchor, path)).or_insert_with(|| {
        *held += 1;
        ContainerNode::new(ContainerKind::Object, value_start, tick)
      });
      node.last_used = tick;

      let mut frontier_off = match node.frontier {
        Frontier::Object { offset } => offset,
        Frontier::Array { .. } => value_start + 1,
      };
      let mut capped = false;
      for (name, member_start, member_value_start) in members {
        if node.member(name).is_some() {
          continue; // already tabled (a prior scan's prefix)
        }
        if node.members.len() >= cap {
          frontier_off = frontier_off.max(*member_start);
          capped = true;
          break;
        }
        node.members.push((name.clone(), *member_value_start));
        *held += 1;
      }
      if !capped {
        if let Some(t) = terminal {
          frontier_off = frontier_off.max(t);
        } else if let Some((_, last_start, _)) = members.last() {
          frontier_off = frontier_off.max(*last_start);
        }
      }
      node.frontier = Frontier::Object {
        offset: frontier_off,
      };
    }
    self.evict_to_budget();
  }

  /// Write back an array scan: advance the single `(index, offset)` frontier
  /// landmark forward (a query for an earlier index never rewinds it).
  pub fn record_array_scan(
    &mut self,
    anchor: u64,
    path: &[Segment],
    value_start: u64,
    frontier: Option<(usize, u64)>,
  ) {
    if !self.is_enabled() {
      return;
    }
    let tick = self.next_tick();
    {
      let Self { nodes, held, .. } = self;
      let node = nodes.entry(NodeKey::new(anchor, path)).or_insert_with(|| {
        *held += 1;
        ContainerNode::new(ContainerKind::Array, value_start, tick)
      });
      node.last_used = tick;
      if let Some((index, offset)) = frontier {
        let advance = match node.frontier {
          Frontier::Array { index: cur, .. } => index >= cur,
          Frontier::Object { .. } => true,
        };
        if advance {
          node.frontier = Frontier::Array { index, offset };
        }
      }
    }
    self.evict_to_budget();
  }

  /// Record a container's matching-close offset (`}`/`]` + 1).
  pub fn record_close(
    &mut self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    close: u64,
  ) {
    self.scalar(anchor, path, kind, value_start, |node| {
      node.close = Some(close)
    });
  }

  /// Record a container's child count.
  pub fn record_child_count(
    &mut self,
    anchor: u64,
    path: &[Segment],
    kind: ContainerKind,
    value_start: u64,
    count: u64,
  ) {
    self.scalar(anchor, path, kind, value_start, |node| {
      node.child_count = Some(count)
    });
  }

  fn scalar(
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
      let Self { nodes, held, .. } = self;
      let node = nodes.entry(NodeKey::new(anchor, path)).or_insert_with(|| {
        *held += 1;
        ContainerNode::new(kind, value_start, tick)
      });
      node.last_used = tick;
      set(node);
    }
    self.evict_to_budget();
  }

  fn next_tick(&mut self) -> u64 {
    self.tick += 1;
    self.tick
  }

  /// Evict least-recently-used whole nodes until the budget holds. Children go
  /// with their node (no orphaning); freshly-written nodes carry the highest
  /// tick and survive over stale ones.
  fn evict_to_budget(&mut self) {
    while self.held > self.budget {
      let Some(victim) = self
        .nodes
        .iter()
        .min_by_key(|(_, n)| n.last_used)
        .map(|(k, _)| k.clone())
      else {
        break;
      };
      if let Some(node) = self.nodes.remove(&victim) {
        self.held -= node.weight();
      }
    }
  }

  #[cfg(test)]
  pub fn held(&self) -> usize {
    self.held
  }

  #[cfg(test)]
  pub fn node_count(&self) -> usize {
    self.nodes.len()
  }

  /// Recompute `held` from scratch; a test invariant that the incremental
  /// accounting matches the actual node weights.
  #[cfg(test)]
  fn recomputed_held(&self) -> usize {
    self.nodes.values().map(ContainerNode::weight).sum()
  }
}

impl ContainerNode {
  fn new(kind: ContainerKind, value_start: u64, tick: u64) -> Self {
    Self {
      kind,
      value_start,
      close: None,
      child_count: None,
      members: Vec::new(),
      frontier: fresh_frontier(kind, value_start),
      last_used: tick,
    }
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
    let mut c = IndexCache::new(0);
    assert!(!c.is_enabled());
    c.record_object_scan(0, &[], 0, &[member("a", 1, 4)], Some(1));
    c.record_close(0, &[], ContainerKind::Object, 0, 10);
    assert!(c.get(0, &[]).is_none());
    assert_eq!(c.held(), 0);
  }

  #[test]
  fn object_table_covers_open_to_high_water() {
    let mut c = IndexCache::new(64);
    // {"a":1,"b":2,"c":3} - scan matched "c"; a,b skipped, c matched. Terminal is
    // c's member-start (the high-water).
    let members = [member("a", 1, 5), member("b", 7, 11), member("c", 13, 17)];
    c.record_object_scan(0, &[], 0, &members, Some(13));
    let node = c.get(0, &[]).expect("node");
    assert_eq!(node.member("a"), Some(5));
    assert_eq!(node.member("b"), Some(11));
    assert_eq!(node.member("c"), Some(17));
    assert_eq!(node.member("d"), None);
    // Frontier sits at the matched member's start, so any un-tabled member is at
    // or after it and a resume from there finds it.
    assert_eq!(node.frontier(), Frontier::Object { offset: 13 });
    assert_eq!(c.held(), 1 + 3);
  }

  #[test]
  fn object_sibling_scans_extend_contiguously() {
    let mut c = IndexCache::new(64);
    // First scan tables a,b and matched b (terminal = b's start = 7).
    c.record_object_scan(0, &[], 0, &[member("a", 1, 5), member("b", 7, 11)], Some(7));
    assert_eq!(
      c.get(0, &[]).unwrap().frontier(),
      Frontier::Object { offset: 7 }
    );
    // Second scan resumes at 7, re-reads b, then c,d; matched d (terminal = d's start).
    c.record_object_scan(
      0,
      &[],
      0,
      &[member("b", 7, 11), member("c", 13, 17), member("d", 19, 23)],
      Some(19),
    );
    let node = c.get(0, &[]).unwrap();
    assert_eq!(node.member("c"), Some(17));
    assert_eq!(node.member("d"), Some(23));
    assert_eq!(node.frontier(), Frontier::Object { offset: 19 });
    // b not double-counted.
    assert_eq!(c.held(), 1 + 4);
  }

  #[test]
  fn per_container_cap_spills_to_frontier() {
    let budget = 1024;
    let mut c = IndexCache::new(budget);
    let members: Vec<_> = (0..PER_CONTAINER_MEMBERS + 10)
      .map(|i| member(&format!("k{i}"), (i * 10) as u64, (i * 10 + 4) as u64))
      .collect();
    c.record_object_scan(0, &[], 0, &members, Some(99_999));
    let node = c.get(0, &[]).unwrap();
    assert_eq!(
      node.members.len(),
      PER_CONTAINER_MEMBERS,
      "table capped at the per-container limit"
    );
    // Frontier froze at the first un-tabled member's start, not the terminal.
    let first_untabled_start = (PER_CONTAINER_MEMBERS * 10) as u64;
    assert_eq!(
      node.frontier(),
      Frontier::Object {
        offset: first_untabled_start
      }
    );
  }

  #[test]
  fn array_frontier_advances_forward_only() {
    let mut c = IndexCache::new(64);
    c.record_array_scan(0, &[], 0, Some((5, 50)));
    assert_eq!(
      c.get(0, &[]).unwrap().frontier(),
      Frontier::Array {
        index: 5,
        offset: 50
      }
    );
    // A later, further index advances it.
    c.record_array_scan(0, &[], 0, Some((9, 90)));
    assert_eq!(
      c.get(0, &[]).unwrap().frontier(),
      Frontier::Array {
        index: 9,
        offset: 90
      }
    );
    // An earlier index does not rewind it.
    c.record_array_scan(0, &[], 0, Some((3, 30)));
    assert_eq!(
      c.get(0, &[]).unwrap().frontier(),
      Frontier::Array {
        index: 9,
        offset: 90
      }
    );
  }

  #[test]
  fn scalars_close_and_count_and_location() {
    let mut c = IndexCache::new(64);
    c.record_close(0, &[], ContainerKind::Array, 0, 42);
    c.record_child_count(0, &[], ContainerKind::Array, 0, 7);
    let node = c.get(0, &[]).unwrap();
    assert_eq!(node.child_count(), Some(7));
    // `location` exposes the recorded close as a full `[start, end)`.
    assert_eq!(node.location(), Some(ValueLocation { start: 0, end: 42 }));
  }

  #[test]
  fn whole_node_lru_keeps_held_within_budget() {
    let budget = 50;
    let mut c = IndexCache::new(budget);
    // Insert many distinct object nodes, each with a few members.
    for i in 0..200u64 {
      let p = obj_path(&format!("path{i}"));
      let members = [
        member("x", i * 100, i * 100 + 4),
        member("y", i * 100 + 6, i * 100 + 10),
      ];
      c.record_object_scan(i, &p, i * 100, &members, Some(i * 100));
      assert!(
        c.held() <= budget,
        "held {} exceeded budget {budget} at i={i}",
        c.held()
      );
    }
    assert_eq!(c.held(), c.recomputed_held(), "held accounting drifted");
    // The most-recently-written node survives.
    let last = obj_path("path199");
    assert!(c.get(199, &last).is_some());
  }

  #[test]
  fn cache_stays_bounded_under_many_queries() {
    let budget = 128;
    let mut c = IndexCache::new(budget);
    for round in 0..50u64 {
      for i in 0..64u64 {
        let p = obj_path(&format!("c{i}"));
        c.record_object_scan(
          0,
          &p,
          i * 1000,
          &[member("f", i * 1000, i * 1000 + 4)],
          Some(i * 1000),
        );
        c.record_array_scan(round, &p, i * 7, Some((round as usize, i * 7)));
      }
      assert!(c.held() <= budget, "held {} over budget", c.held());
      assert!(c.node_count() <= budget, "node_count over budget");
    }
    assert_eq!(c.held(), c.recomputed_held());
  }
}
