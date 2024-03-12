// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

//! Implements memory safety analysis.
//!
//! This is an intra functional, forward-directed data flow analysis over the domain
//! of what we call a *borrow graph*. The borrow graph tracks the creation of references from
//! root memory locations and derivation of other references, by recording an edge for each
//! borrow relation. For example, if `s` is the memory location of a struct, then
//! `&s.f` is represented by a node which is derived from `s`, and those two nodes are
//! connected by an edge labeled with `.f`. In the below example, we have `&s.g` stored
//! in `r1` and `&s.f` stored in `r2` (edges in the graph should be read as arrows pointing
//! downwards):
//!
//! ```ignore
//!              s
//!              | &
//!          .g / \ .f
//!            r1 r2
//! ```
//!
//! Borrow graphs do not come into a normalized form, thus different graphs can represent the
//! same borrow relations. For example, this graph is equivalent to the above. It has what
//! we call an _implicit choice_, that is the choice between alternatives is down
//! after a borrow step:
//!
//! ```ignore
//!              s
//!           & / \ &
//!          .g |  | .f
//!            r1 r2
//! ```
//!
//! In general, the graph is a DAG. Joining of nodes represents branching in the code. For
//! example, the graph below depicts that `r` can either be `&s.f` or `&s.g`:
//!
//! ```ignore
//!              s
//!           & / \ &
//!          .g \ / .f
//!              r
//! ```
//!
//! Together with the borrow graph, pointing from temporaries to graph nodes is maintained at
//! each program point. These represent the currently alive references into the borrowed data.
//! All the _parents_ of those nodes are indirectly borrowed as well. For example, in the
//! graph above, `r` is active and `s` is as well because it is indirectly borrowed by `r`.
//! When the temporaries pointing into the borrow graph are not longer alive, we perform a
//! clean up step and also release any parents not longer needed. For more details of
//! this mechanics, see the comments in the code.
//!
//! The safety analysis essentially evaluates each instruction under the viewpoint of the current
//! active borrow graph at the program point, to detect any conditions of non-safety. This
//! includes specifically the following rules:
//!
//! 1. A local which is borrowed (i.e. points to a node in the graph) cannot be overwritten.
//! 2. A local which is borrowed cannot be moved.
//! 3. References returned from a function call must be derived from parameters
//! 4. Before any call to a user function, or before reading or writing a reference,
//!    the borrow graph must be _safe_ w.r.t. the arguments. Notice specifically, that for
//!    a series of other instructions (loading and moving temporaries around, borrowing fields)
//!    safety is not enforced. This is important to allow construction of more complex
//!    borrow graphs, where the intermediate steps are not safe.
//!
//! To understand the concept of a _safe_ borrow graph consider that edges have a notion of being
//! disjoint. For instance, field selection `s.f` and `s.g` constructs two references into `s` which
//! can safely co-exist because there is no overlap. Consider further a path `p` being a sequence
//! of borrow steps (edges) in the graph from a root to a leaf node. For two paths `p1` and `p2`,
//! _diverging edges_, `(e1, e2) = diverging(p1, p2)`, are a pair of edges where the paths differ
//! after some non-empty common prefix, and do not have a common node where they join again. Here is
//! an example of two paths with diverging edges:
//!
//! ```ignore
//!              s
//!        &mut / \ &mut
//!      call f |  | call g
//!            r1  r2
//! ```
//!
//! Here is another example where, while edges differ, they do not diverge because the paths later
//! join again. Notice that this is a result of different execution paths from code
//! like `let r = if (c) f(&mut s) else g(&mut s)`:
//!
//! ```ignore
//!              s
//!        &mut / \ &mut
//!      call f |  | call g
//!             \  /
//!              r
//! ```
//!
//! Given this definition, a graph is called *safe w.r.t. a set of temporaries `temps`*
//! under the following conditions:
//!
//! a. Any path which does not end in `nodes(temps)` is safe and considered out of scope,
//!    where `nodes(temps)` denotes the nodes which are associated with the given `temps`.
//! b. For any two paths `p` and `q`, `q != p`, and any pair of diverging edges `e1` and `e2`, if
//!    any of those edges is mut, the other needs to be disjoint. This basically states that one
//!    cannot have `&x.f` and `&mut x.f` coexist in a safe graph. However, `&x.f` and `&mut x.g`
//!    is fine.
//! c. For any path `p`, if the last edge is mut, `p` must not be a prefix of any other path. This
//!    basically states that mutable reference in `temps` must be exclusive and cannot
//!    have other borrows.
//! d. For all identical paths in the graph (recall that because of indirect choices, we can
//!    have the same path appearing multiple times in the graph), if the last edge is mut, then
//!    the set of temporaries associated with those paths must be a singleton. This basically
//!    states that the same mutable reference in `temps` cannot be used twice.
//! e. For any path `p`, if a certain edge is mut, then all edges in its prefix must be mut as well.

use crate::{
    pipeline::livevar_analysis_processor::{LiveVarAnnotation, LiveVarInfoAtCodeOffset},
    Experiment, Options,
};
use abstract_domain_derive::AbstractDomain;
use codespan_reporting::diagnostic::Severity;
use itertools::Itertools;
use log::{debug, log_enabled, Level};
use move_binary_format::file_format::CodeOffset;
use move_model::{
    ast::TempIndex,
    model::{FieldId, FunctionEnv, GlobalEnv, Loc, Parameter, QualifiedInstId, StructId},
    ty::Type,
};
use move_stackless_bytecode::{
    dataflow_analysis::{DataflowAnalysis, TransferFunctions},
    dataflow_domains::{AbstractDomain, JoinResult, MapDomain, SetDomain},
    function_target::{FunctionData, FunctionTarget},
    function_target_pipeline::{FunctionTargetProcessor, FunctionTargetsHolder},
    stackless_bytecode::{AssignKind, AttrId, Bytecode, Operation},
    stackless_control_flow_graph::StacklessControlFlowGraph,
};
use std::{
    cmp::Ordering,
    collections::{btree_map, BTreeMap, BTreeSet},
    fmt::{Display, Formatter},
    iter,
};

const DEBUG: bool = false;

// ===============================================================================
// Memory Safety Analysis

// -------------------------------------------------------------------------------------------------
// Program Analysis Domain

/// The program analysis domain used with the data flow analysis framework.
#[derive(Clone, Default, Debug)]
pub struct LifetimeState {
    /// Contains the borrow graph at the current program point, which consists of a set of `LifetimeNode` values
    /// which are labeled by `LifetimeLabel`. This contains exactly those nodes reachable
    /// as parents or children of the node labels used in the below maps and grows and shrinks from
    /// program point to program point.
    graph: MapDomain<LifetimeLabel, LifetimeNode>,
    /// A map from temporaries to labels, for those temporaries which have an associated node in the graph.
    /// If a local is originally borrowed, it will point from `temp` to a node with the `MemoryLocation::Local(temp)`.
    /// If a local is a reference derived from a location, it will point to a node with `MemoryLocation::Derived`.
    temp_to_label_map: BTreeMap<TempIndex, LifetimeLabel>,
    /// A map from globals to labels. Represents root states of the active graph.
    global_to_label_map: BTreeMap<QualifiedInstId<StructId>, LifetimeLabel>,
}

/// Represents a node of the borrow graph.
#[derive(AbstractDomain, Clone, Debug, PartialEq, Eq)]
struct LifetimeNode {
    /// Memory locations associated with this node. This is a set as a result of joins.
    locations: SetDomain<MemoryLocation>,
    /// Outgoing edges to children.
    children: SetDomain<BorrowEdge>,
    /// Backlinks to parents.
    parents: SetDomain<LifetimeLabel>,
}

/// A label for a lifetime node.
#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy, Debug)]
struct LifetimeLabel(u64);

/// A memory location, either a global in storage, a local on the stack, an external from parameter, or
/// a derived portion of it.
#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Debug)]
enum MemoryLocation {
    /// The underlying memory is in global storage.
    Global(QualifiedInstId<StructId>),
    /// The underlying memory is a local on the stack.
    Local(TempIndex),
    /// The underlying memory is some external memory referenced by a function parameter
    External,
    /// Derives from underlying memory as defined by incoming edges. This is used to represent the
    /// result of a field select or function call.
    Derived,
}

/// Represents an edge in the borrow graph. The source of the edge is implicit in the ownership
/// of the edge by a LifetimeNode through its children field
#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Debug)]
struct BorrowEdge {
    /// The kind of borrow edge.
    kind: BorrowEdgeKind,
    /// A location associated with the borrow edge.
    loc: Loc,
    /// Target of the edge.
    target: LifetimeLabel,
}

/// The different type of edges. Each edge caries a boolean indicating whether it is a mutating borrow.
#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Debug)]
enum BorrowEdgeKind {
    /// Borrows the local at the MemoryLocation in the source node.
    BorrowLocal(bool),
    /// Borrows the global at the MemoryLocation in the source node.
    BorrowGlobal(bool),
    /// Borrows a field from a reference.
    BorrowField(bool, FieldId),
    /// Calls an operation, where the incoming references are used to derive outgoing references. Since every
    /// call outcome can be different, they are distinguished by code offset -- two call edges are never the
    /// same.
    Call(bool, Operation, CodeOffset),
    /// Freezes a mutable reference.
    Freeze,
}

impl BorrowEdgeKind {
    fn is_mut(&self) -> bool {
        use BorrowEdgeKind::*;
        match self {
            BorrowLocal(is_mut)
            | BorrowGlobal(is_mut)
            | BorrowField(is_mut, _)
            | Call(is_mut, _, _) => *is_mut,
            Freeze => false,
        }
    }
}

impl LifetimeLabel {
    /// Creates a new, unique and stable, life time label based on a code offset and
    /// a qualifier to distinguish multiple labels at the same code point.
    /// Since the program analysis could run fixpoint loops, we need to ensure that
    /// these labels are the same in each iteration.
    fn new_from_code_offset(code_offset: CodeOffset, qualifier: u8) -> LifetimeLabel {
        LifetimeLabel(((code_offset as u64) << 8) | (qualifier as u64))
    }

    /// Creates a globally unique label from a counter. These are disjoint from those
    /// from code labels.
    fn new_from_counter(count: u32) -> LifetimeLabel {
        // code offset = 16 bits, qualifier 8 bits
        LifetimeLabel(((count + 1) as u64) << 24)
    }
}

impl BorrowEdge {
    /// Shortcut to create an edge.
    fn new(kind: BorrowEdgeKind, loc: Loc, target: LifetimeLabel) -> Self {
        Self { kind, loc, target }
    }
}

impl BorrowEdgeKind {
    /// Determines whether the region derived from this edge has overlap with the region
    /// of the other edge. Overlap can only be excluded for field edges.
    fn overlaps(&self, other: &BorrowEdgeKind) -> bool {
        use BorrowEdgeKind::*;
        match (self, other) {
            (BorrowField(_, field1), BorrowField(_, field2)) => field1 == field2,
            _ => true,
        }
    }
}

impl AbstractDomain for LifetimeState {
    /// The join operator of the dataflow analysis domain.
    ///
    /// Joining of lifetime states is easy for the borrow graph, as we can simply join the node representations
    /// using the same label. This is consistent because each label is constructed from the program point.
    /// However, if it comes to the mappings of globals/temporaries to labels, we need to unify distinct labels of the
    /// two states. Consider `$t1 -> @1` in one state and `$t1 -> @2` in another state, then we need to unify
    /// the states under labels `@1` and `@2` into one, and renames any occurrence of the one label by the other.
    fn join(&mut self, other: &Self) -> JoinResult {
        // Join the graph
        let mut change = self.graph.join(&other.graph);
        self.check_graph_consistency();

        // A label renaming map resulting from joining lifetime nodes.
        let mut renaming: BTreeMap<LifetimeLabel, LifetimeLabel> = BTreeMap::new();

        let mut new_temp_to_label_map = std::mem::take(&mut self.temp_to_label_map);
        change = change.combine(self.join_label_map(
            &mut new_temp_to_label_map,
            &other.temp_to_label_map,
            &mut renaming,
        ));
        let mut new_global_to_label_map = std::mem::take(&mut self.global_to_label_map);
        change = change.combine(self.join_label_map(
            &mut new_global_to_label_map,
            &other.global_to_label_map,
            &mut renaming,
        ));
        self.temp_to_label_map = new_temp_to_label_map;
        self.global_to_label_map = new_global_to_label_map;

        if !renaming.is_empty() {
            Self::rename_labels_in_graph(&renaming, &mut self.graph);
            Self::rename_labels_in_map(&renaming, &mut self.temp_to_label_map);
            Self::rename_labels_in_map(&renaming, &mut self.global_to_label_map);
            change = JoinResult::Changed;
        }
        self.check_graph_consistency();
        change
    }
}

impl LifetimeState {
    /// Joins two maps with labels in their range. For overlapping keys pointing to different labels,
    /// the nodes behind the labels in the graph are joined, and the label in the `other_map` is
    /// replaced by the given one in `map`. This functions remembers (but does not yet apply)
    /// the replaced labels in the `renaming` map.
    fn join_label_map<A: Clone + Ord>(
        &mut self,
        map: &mut BTreeMap<A, LifetimeLabel>,
        other_map: &BTreeMap<A, LifetimeLabel>,
        renaming: &mut BTreeMap<LifetimeLabel, LifetimeLabel>,
    ) -> JoinResult {
        let mut change = JoinResult::Unchanged;
        for (k, other_label) in other_map {
            match map.entry(k.clone()) {
                btree_map::Entry::Vacant(entry) => {
                    entry.insert(*other_label);
                    change = JoinResult::Changed;
                },
                btree_map::Entry::Occupied(entry) => {
                    let label = entry.get();
                    if label != other_label {
                        // Merge other node into this one, and add renaming of label.
                        let other_copy = self.node(other_label).clone(); // can't mut and read same time
                        self.node_mut(label).join(&other_copy);
                        renaming.insert(*other_label, *label);
                        change = JoinResult::Changed;
                    }
                },
            }
        }
        change
    }

    fn rename_label(renaming: &BTreeMap<LifetimeLabel, LifetimeLabel>, label: &mut LifetimeLabel) {
        // Apply renaming transitively -- it likely cannot occur right now but perhaps in the future.
        let mut visited = BTreeSet::new();
        while let Some(actual) = renaming.get(label) {
            assert!(visited.insert(*label), "renaming must be acyclic");
            *label = *actual
        }
    }

    fn rename_labels_in_map<A: Clone + Ord>(
        renaming: &BTreeMap<LifetimeLabel, LifetimeLabel>,
        map: &mut BTreeMap<A, LifetimeLabel>,
    ) {
        for label in map.values_mut() {
            Self::rename_label(renaming, label)
        }
    }

    fn rename_labels_in_graph(
        renaming: &BTreeMap<LifetimeLabel, LifetimeLabel>,
        graph: &mut MapDomain<LifetimeLabel, LifetimeNode>,
    ) {
        graph.update_values(|node| {
            let mut new_edges = SetDomain::default();
            for mut edge in std::mem::take(&mut node.children).into_iter() {
                Self::rename_label(renaming, &mut edge.target);
                new_edges.insert(edge);
            }
            node.children = new_edges;
            Self::rename_labels_in_set(renaming, &mut node.parents)
        });
        // Delete any nodes which are renamed
        for l in renaming.keys() {
            graph.remove(l);
        }
    }

    fn rename_labels_in_set(
        renaming: &BTreeMap<LifetimeLabel, LifetimeLabel>,
        set: &mut SetDomain<LifetimeLabel>,
    ) {
        *set = set
            .iter()
            .cloned()
            .map(|mut l| {
                Self::rename_label(renaming, &mut l);
                l
            })
            .collect();
    }

    /// Checks, at or above debug level, that
    /// - for any nodes `v, u` in the borrow graph, `v` is has parent/child `u` iff `u` has child/parent `v`
    /// - all labels in the label maps are in the graph
    fn check_graph_consistency(&self) {
        if log_enabled!(Level::Debug) {
            self.debug_print("before check");
            for (l, n) in self.graph.iter() {
                for e in n.children.iter() {
                    assert!(
                        self.graph.contains_key(&e.target),
                        "{} child not in graph",
                        e.target
                    );
                    assert!(
                        self.node(&e.target).parents.contains(l),
                        "{} is not included as a parent in {}",
                        l,
                        e.target
                    )
                }
                for p in n.parents.iter() {
                    assert!(self.graph.contains_key(p), "{} parent not in graph", p);
                    assert!(
                        self.node(p).children.iter().any(|e| &e.target == l),
                        "{} no a child of {}",
                        l,
                        p
                    )
                }
            }
            for l in self
                .temp_to_label_map
                .values()
                .chain(self.global_to_label_map.values())
            {
                assert!(
                    self.graph.contains_key(l),
                    "{} is in label map but not in graph",
                    l
                )
            }
        }
    }

    fn debug_print(&self, header: &str) {
        if DEBUG && log_enabled!(Level::Debug) {
            let mut header = header.to_owned();
            for (l, n) in self.graph.iter() {
                debug!(
                    "{} {} {:?} -> {}  (<- {})",
                    header,
                    l,
                    n.locations,
                    n.children
                        .iter()
                        .map(|e| format!("{}", e.target))
                        .join(", "),
                    n.parents.iter().map(|l| format!("{}", l)).join(", ")
                );
                header = (0..header.len()).map(|_| ' ').collect();
            }
            debug!(
                "{} {}",
                header,
                self.temp_to_label_map
                    .iter()
                    .map(|(k, v)| format!("$t{} = {}", k, v))
                    .join(", ")
            )
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Working with LifetimeState

impl LifetimeState {
    /// Creates a new node with the given label and location information.
    fn new_node(&mut self, assigned_label: LifetimeLabel, location: MemoryLocation) {
        self.graph.insert(assigned_label, LifetimeNode {
            locations: iter::once(location).collect(),
            children: Default::default(),
            parents: Default::default(),
        });
    }

    /// Returns reference to node.
    fn node(&self, label: &LifetimeLabel) -> &LifetimeNode {
        &self.graph[label]
    }

    /// Returns mutable reference to node.
    fn node_mut(&mut self, label: &LifetimeLabel) -> &mut LifetimeNode {
        &mut self.graph[label]
    }

    /// Returns true if the given label is an ancestor of the other. This is transitive and reflexive.
    fn is_ancestor(&self, label: &LifetimeLabel, descendant: &LifetimeLabel) -> bool {
        label == descendant
            || self
                .children(label)
                .any(|e| self.is_ancestor(&e.target, descendant))
    }

    /// Returns an iteration of child edges of given node.
    fn children(&self, label: &LifetimeLabel) -> impl Iterator<Item = &BorrowEdge> {
        self.node(label).children.iter()
    }

    /// Returns the children grouped by their edge kind.
    fn grouped_children(
        &self,
        labels: &BTreeSet<LifetimeLabel>,
    ) -> BTreeMap<BorrowEdgeKind, Vec<&BorrowEdge>> {
        let mut result: BTreeMap<BorrowEdgeKind, Vec<&BorrowEdge>> = BTreeMap::new();
        for label in labels {
            for edge in self.children(label) {
                result.entry(edge.kind.clone()).or_default().push(edge)
            }
        }
        result
    }

    /// Returns true if given node has no children
    fn is_leaf(&self, label: &LifetimeLabel) -> bool {
        self.node(label).children.is_empty()
    }

    /// Gets the label associated with a local, if it has children.
    fn label_for_temp_with_children(&self, temp: TempIndex) -> Option<&LifetimeLabel> {
        self.label_for_temp(temp).filter(|l| !self.is_leaf(l))
    }

    /// Gets the label associated with a global, if it has children.
    fn label_for_global_with_children(
        &self,
        resource: &QualifiedInstId<StructId>,
    ) -> Option<&LifetimeLabel> {
        self.label_for_global(resource).filter(|l| !self.is_leaf(l))
    }

    /// Returns true if the node has outgoing mut edges.
    fn has_mut_edges(&self, label: &LifetimeLabel) -> bool {
        self.children(label).any(|e| e.kind.is_mut())
    }

    /// Gets the label associated with a local.
    fn label_for_temp(&self, temp: TempIndex) -> Option<&LifetimeLabel> {
        self.temp_to_label_map.get(&temp)
    }

    /// If label for local exists, return it, otherwise create a new node. The code offset and qualifier are
    /// used to create a lifetime label if needed. 'root' indicates whether is a label for an actual memory
    /// root (like a local, external, or global) instead of a reference.
    fn make_temp(
        &mut self,
        temp: TempIndex,
        code_offset: CodeOffset,
        qualifier: u8,
        root: bool,
    ) -> LifetimeLabel {
        self.make_temp_from_label_fun(
            temp,
            || LifetimeLabel::new_from_code_offset(code_offset, qualifier),
            root,
        )
    }

    /// More general version as above where the label to be created, if needed, is specified
    /// by a function.
    fn make_temp_from_label_fun(
        &mut self,
        temp: TempIndex,
        from_label: impl Fn() -> LifetimeLabel,
        root: bool,
    ) -> LifetimeLabel {
        if let Some(label) = self.temp_to_label_map.get(&temp) {
            *label
        } else {
            let label = from_label();
            self.new_node(
                label,
                if root {
                    MemoryLocation::Local(temp)
                } else {
                    MemoryLocation::Derived
                },
            );
            self.temp_to_label_map.insert(temp, label);
            label
        }
    }

    /// Gets the label associated with a global.
    fn label_for_global(&self, global: &QualifiedInstId<StructId>) -> Option<&LifetimeLabel> {
        self.global_to_label_map.get(global)
    }

    /// If label for global exists, return it, otherwise create a new one.
    fn make_global(
        &mut self,
        struct_id: QualifiedInstId<StructId>,
        code_offset: CodeOffset,
        qualifier: u8,
    ) -> LifetimeLabel {
        if let Some(label) = self.global_to_label_map.get(&struct_id) {
            *label
        } else {
            let label = LifetimeLabel::new_from_code_offset(code_offset, qualifier);
            self.new_node(label, MemoryLocation::Global(struct_id.clone()));
            self.global_to_label_map.insert(struct_id, label);
            label
        }
    }

    /// Adds an edge to the graph.
    fn add_edge(&mut self, label: LifetimeLabel, edge: BorrowEdge) {
        let child = edge.target;
        self.node_mut(&label).children.insert(edge);
        self.node_mut(&child).parents.insert(label);
    }

    /// Drops a leaf node. The parents are recursively dropped if their children go down to
    /// zero. Collects the locations of the dropped nodes. Gets passed the set of
    /// labels which are currently in use and pointed to from outside of the graph.
    fn drop_leaf_node(
        &mut self,
        label: &LifetimeLabel,
        in_use: &BTreeSet<LifetimeLabel>,
        removed: &mut BTreeSet<MemoryLocation>,
    ) {
        if in_use.contains(label) {
            return;
        }
        if let Some(node) = self.graph.remove(label) {
            debug_assert!(node.children.is_empty());
            removed.extend(node.locations.iter().cloned());
            for parent in node.parents.iter() {
                let node = self.node_mut(parent);
                // Remove the dropped node from the children list.
                let children = std::mem::take(&mut node.children);
                node.children = children
                    .into_iter()
                    .filter(|e| &e.target != label)
                    .collect();
                // Drop the parent as well if it is now a leaf
                if node.children.is_empty() {
                    self.drop_leaf_node(parent, in_use, removed)
                }
            }
        }
    }

    /// Returns a map from labels which are used by temporaries to the set which are using them.
    fn leaves(&self) -> BTreeMap<LifetimeLabel, BTreeSet<TempIndex>> {
        let mut map: BTreeMap<LifetimeLabel, BTreeSet<TempIndex>> = BTreeMap::new();
        for (temp, label) in &self.temp_to_label_map {
            map.entry(*label).or_default().insert(*temp);
        }
        map
    }

    /// Releases graph resources for a reference in temporary.
    fn release_ref(&mut self, temp: TempIndex) {
        if let Some(label) = self.temp_to_label_map.remove(&temp) {
            if self.is_leaf(&label) {
                // We can drop the underlying node, as there are no borrows out.
                let in_use = self.leaves().keys().cloned().collect();
                let mut indirectly_removed = BTreeSet::new();
                self.drop_leaf_node(&label, &in_use, &mut indirectly_removed);
                // Remove memory locations not longer borrowed.
                for location in indirectly_removed {
                    use MemoryLocation::*;
                    match location {
                        Local(temp) => {
                            self.temp_to_label_map.remove(&temp);
                        },
                        Global(qid) => {
                            self.global_to_label_map.remove(&qid);
                        },
                        External | Derived => {},
                    }
                }
            }
        }
        self.check_graph_consistency()
    }

    /// Replaces a reference in a temporary, as result of an assignment. The current
    /// node associated with the ref is released and then a new node is created and
    /// returned.
    fn replace_ref(
        &mut self,
        temp: TempIndex,
        code_offset: CodeOffset,
        qualifier: u8,
    ) -> LifetimeLabel {
        self.release_ref(temp);
        // Temp might not be released if it is still borrowed, so remove from the map
        self.temp_to_label_map.remove(&temp);
        let label = self.make_temp(temp, code_offset, qualifier, false);
        self.check_graph_consistency();
        label
    }

    /// Move a reference from source to destination. This moves the LifetimeLabel over to the new temp.
    fn move_ref(&mut self, dest: TempIndex, src: TempIndex) {
        let Some(label) = self.temp_to_label_map.remove(&src) else {
            return;
        };
        self.temp_to_label_map.insert(dest, label);
        self.check_graph_consistency()
    }

    /// Copies a reference from source to destination. This create a new lifetime node and clones the edges
    /// leading into the node associated with the source reference.
    fn copy_ref(&mut self, dest: TempIndex, src: TempIndex) {
        if let Some(label) = self.label_for_temp(src) {
            self.temp_to_label_map.insert(dest, *label);
        }
    }

    /// Returns an iterator of the edges which are leading into this node.
    #[allow(unused)]
    fn parent_edges<'a>(
        &'a self,
        label: &'a LifetimeLabel,
    ) -> impl Iterator<Item = (LifetimeLabel, &'a BorrowEdge)> + '_ {
        self.node(label).parents.iter().flat_map(move |parent| {
            self.children(parent)
                .filter(move |edge| &edge.target == label)
                .map(|e| (*parent, e))
        })
    }

    /// Returns the roots of this node, that is those ancestors which have no parents.
    fn roots(&self, label: &LifetimeLabel) -> BTreeSet<LifetimeLabel> {
        let mut roots = BTreeSet::new();
        let mut todo = self.node(label).parents.iter().cloned().collect::<Vec<_>>();
        if todo.is_empty() {
            // Node is already root
            roots.insert(*label);
        } else {
            let mut done = BTreeSet::new();
            while let Some(l) = todo.pop() {
                if !done.insert(l) {
                    continue;
                }
                let node = self.node(&l);
                if node.parents.is_empty() {
                    // Found a root
                    roots.insert(l);
                } else {
                    // Explore parents
                    todo.extend(node.parents.iter().cloned())
                }
            }
        }
        roots
    }

    /// Returns the transitive children of this node.
    fn transitive_children(&self, label: &LifetimeLabel) -> BTreeSet<LifetimeLabel> {
        // Helper function to collect the target nodes of the children.
        let get_children =
            |label: &LifetimeLabel| self.node(label).children.iter().map(|e| e.target);
        let mut result = BTreeSet::new();
        let mut todo = get_children(label).collect::<Vec<_>>();
        if todo.is_empty() {
            result.insert(*label);
        } else {
            while let Some(l) = todo.pop() {
                if !result.insert(l) {
                    continue;
                }
                todo.extend(get_children(&l));
            }
        }
        result
    }

    /// Returns the temporaries borrowed
    pub fn borrowed_locals(&self) -> impl Iterator<Item = TempIndex> + '_ {
        self.temp_to_label_map.keys().cloned()
    }

    /// Checks if the given local is borrowed
    pub fn is_borrowed(&self, temp: TempIndex) -> bool {
        self.label_for_temp_with_children(temp).is_some()
    }
}

// -------------------------------------------------------------------------------------------------
// Lifetime Analysis

/// A structure providing context information for operations during lifetime analysis.
/// This encapsulates the function target which is analyzed, giving also access to
/// the global model. Live var annotations are attached which are evaluated during
/// analysis.
struct LifeTimeAnalysis<'env> {
    /// The function target being analyzed
    target: &'env FunctionTarget<'env>,
    /// The live-var annotation extracted from a previous phase
    live_var_annotation: &'env LiveVarAnnotation,
    // If true, any errors generated by this analysis will be suppressed
    suppress_errors: bool,
}

/// A structure encapsulating, in addition to the analysis context, context
/// about the current instruction step being processed.
struct LifetimeAnalysisStep<'env, 'state> {
    /// The analysis context
    parent: &'env LifeTimeAnalysis<'env>,
    /// The code offset
    code_offset: CodeOffset,
    /// The attribute id at the code offset
    attr_id: AttrId,
    /// Lifetime information at the given code offset
    alive: &'env LiveVarInfoAtCodeOffset,
    /// Mutable reference to the analysis state
    state: &'state mut LifetimeState,
}

/// Used to distinguish how a local is read
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    /// The local is moved
    Move,
    /// The local is copied
    Copy,
    /// The local is transferred as an argument to another function
    Argument,
}

impl<'env> LifeTimeAnalysis<'env> {
    fn new_step<'a>(
        &'a self,
        code_offset: CodeOffset,
        attr_id: AttrId,
        state: &'a mut LifetimeState,
    ) -> LifetimeAnalysisStep {
        let alive = self
            .live_var_annotation
            .get_live_var_info_at(code_offset)
            .expect("live var info");
        LifetimeAnalysisStep {
            parent: self,
            code_offset,
            attr_id,
            alive,
            state,
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Analysing and Diagnosing

impl<'env, 'state> LifetimeAnalysisStep<'env, 'state> {
    /// Get the location associated with bytecode attribute.
    fn loc(&self, id: AttrId) -> Loc {
        self.target().get_bytecode_loc(id)
    }

    /// Returns the location of the current instruction
    fn cur_loc(&self) -> Loc {
        self.loc(self.attr_id)
    }

    /// Gets a string for a local to be displayed in error messages
    fn display(&self, local: TempIndex) -> String {
        self.target().get_local_name_for_error_message(local)
    }

    /// Display a non-empty set of temps. This prefers the first printable representative, if any
    fn display_set(&self, set: &BTreeSet<TempIndex>) -> String {
        if let Some(temp) = set
            .iter()
            .find(|t| self.target().get_local_name_opt(**t).is_some())
        {
            self.display(*temp)
        } else {
            self.display(*set.first().expect("non empty"))
        }
    }

    /// Returns "<prefix>`<name>` " if local has name, otherwise empty.
    fn display_name_or_empty(&self, prefix: &str, local: TempIndex) -> String {
        self.target()
            .get_local_name_opt(local)
            .map(|s| format!("{}`{}`", prefix, s))
            .unwrap_or_default()
    }

    /// Get the type associated with local.
    fn ty(&self, local: TempIndex) -> &Type {
        self.target().get_local_type(local)
    }

    /// Returns true if the local is a reference.
    fn is_ref(&self, local: TempIndex) -> bool {
        self.ty(local).is_reference()
    }

    /// Check validness of reading a local.
    fn check_read_local(&self, local: TempIndex, read_mode: ReadMode) -> bool {
        if self.is_ref(local) {
            // Always valid
            return true;
        }
        if let Some(label) = self.state.label_for_temp_with_children(local) {
            let loc = self.cur_loc();
            let usage_info = || self.usage_info(label, |t| t != &local);
            match read_mode {
                ReadMode::Copy => {
                    // Mutable borrow is not allowed
                    if self.state.has_mut_edges(label) {
                        self.error_with_hints(
                            loc,
                            format!(
                                "cannot copy {} which is still mutably borrowed",
                                self.display(local)
                            ),
                            "copied here",
                            self.borrow_info(label, |e| e.kind.is_mut())
                                .into_iter()
                                .chain(usage_info()),
                        );
                        false
                    } else {
                        true
                    }
                },
                ReadMode::Move => {
                    // Any borrow not allowed
                    self.error_with_hints(
                        loc,
                        format!(
                            "cannot move {} which is still borrowed",
                            self.display(local)
                        ),
                        "moved here",
                        self.borrow_info(label, |_| true)
                            .into_iter()
                            .chain(usage_info()),
                    );
                    false
                },
                ReadMode::Argument => {
                    // Mutable borrow not allowed
                    if self.state.has_mut_edges(label) {
                        self.error_with_hints(
                            loc,
                            format!(
                                "cannot pass {} which is still mutably \
                                    borrowed as function argument",
                                self.display(local)
                            ),
                            "passed here",
                            self.borrow_info(label, |_| true)
                                .into_iter()
                                .chain(usage_info()),
                        );
                        false
                    } else {
                        true
                    }
                },
            }
        } else {
            true
        }
    }

    /// Check whether a local can be written. This is only allowed if no borrowed references exist.
    fn check_write_local(&self, local: TempIndex) {
        if self.is_ref(local) {
            // Always valid
            return;
        }
        if let Some(label) = self.state.label_for_temp_with_children(local) {
            // The destination is currently borrowed and cannot be assigned
            self.error_with_hints(
                self.cur_loc(),
                format!("cannot assign to borrowed {}", self.display(local)),
                "attempted to assign here",
                self.borrow_info(label, |_| true)
                    .into_iter()
                    .chain(self.usage_info(label, |t| t != &local)),
            )
        }
    }

    /// Check whether the borrow graph is 'safe' w.r.t a set of `temps`. See the discussion of safety at the
    /// beginning of this file.
    ///
    /// To effectively check the path-oriented conditions of safety here, we need to deal with the fact
    /// that graphs have non-explicit choice nodes, for example:
    ///
    /// ```ignore
    ///                 s
    ///            &mut /\ &mut
    ///             .f /  \ .g
    ///              r1    r2
    /// ```
    ///
    /// The diverging edges `.f` and `.g` are not directly visible. In order to deal with this, we construct a
    /// _hyper graph_ on the fly as follows:
    ///
    /// 1. The root nodes are the singleton sets with all the root nodes for the given temporaries.
    /// 2. The hyper edges are grouped into those of the same edge kind. Hence, two `&mut` edges
    ///    like in the example above become one hyper edge. The successor state of the hyper edge
    ///    is the union of all the targets of the edges grouped together.
    ///
    /// If we walk this graph now from the root to the leaves, we can determine safety by directly comparing
    /// hyper edge siblings.
    fn check_borrow_safety(&mut self, temps_vec: &[TempIndex]) {
        // First check direct duplicates
        for (i, temp) in temps_vec.iter().enumerate() {
            if temps_vec[i + 1..].contains(temp) {
                self.exclusive_access_direct_dup_error(*temp)
            }
        }
        // Now build and analyze the hyper graph
        let temps = &temps_vec.iter().cloned().collect::<BTreeSet<_>>();
        let filtered_leaves = self
            .state
            .leaves()
            .into_iter()
            .filter_map(|(l, mut ts)| {
                ts = ts.intersection(temps).cloned().collect();
                if !ts.is_empty() {
                    Some((l, ts))
                } else {
                    None
                }
            })
            .collect::<BTreeMap<_, _>>();
        // Initialize root hyper nodes
        let mut hyper_nodes: BTreeSet<BTreeSet<LifetimeLabel>> = BTreeSet::new();
        for filtered_leaf in filtered_leaves.keys() {
            for root in self.state.roots(filtered_leaf) {
                hyper_nodes.insert(iter::once(root).collect());
            }
        }
        let mut edges_reported: BTreeSet<BTreeSet<&BorrowEdge>> = BTreeSet::new();
        // Continue to process hyper nodes
        while let Some(hyper) = hyper_nodes.pop_first() {
            let hyper_edges = self.state.grouped_children(&hyper);
            // Check 2-wise combinations of hyper edges for issues. This discovers cases where edges
            // conflict because of mutability.
            for mut perm in hyper_edges.iter().combinations(2) {
                let (kind1, edges1) = perm.pop().unwrap();
                let (kind2, edges2) = perm.pop().unwrap();
                if (kind1.is_mut() || kind2.is_mut()) && kind1.overlaps(kind2) {
                    for (e1, e2) in edges1.iter().cartesian_product(edges2.iter()) {
                        if e1 == e2 || !edges_reported.insert([*e1, *e2].into_iter().collect()) {
                            continue;
                        }
                        // If the diverging edges have common transitive children they result from
                        // joining of conditional branches and are allowed. See also discussion in the file
                        // comment.
                        // NOTE: we may do this more efficiently using a lazy algorithm similar as LCA graph
                        // algorithms as the first common children we find is enough. However, since we
                        // expect the graph of small size, this seems not be too important.
                        // CONJECTURE: it is sufficient here to just check for an intersection. If there is any
                        // common child, then for any later divergences when following the edges, we will do
                        // this check again.
                        if !self
                            .state
                            .transitive_children(&e1.target)
                            .is_disjoint(&self.state.transitive_children(&e2.target))
                        {
                            continue;
                        }
                        self.diverging_edge_error(hyper.first().unwrap(), e1, e2, &filtered_leaves)
                    }
                }
            }
            // Now go over each hyper edge and if they target a leaf node check for conditions
            for (_, edges) in hyper_edges {
                let mut mapped_temps = BTreeSet::new();
                let mut targets = BTreeSet::new();
                for edge in edges {
                    let target = edge.target;
                    targets.insert(target);
                    if edge.kind.is_mut() {
                        if let Some(ts) = filtered_leaves.get(&target) {
                            let mut inter =
                                ts.intersection(temps).cloned().collect::<BTreeSet<_>>();
                            if !inter.is_empty() {
                                if !self.state.is_leaf(&target) {
                                    // A mut leaf node must have exclusive access
                                    self.exclusive_access_borrow_error(&target, &inter)
                                }
                                mapped_temps.append(&mut inter)
                            }
                        }
                    }
                }
                if mapped_temps.len() > 1 {
                    // We cannot associate the same mut node with more than one local
                    self.exclusive_access_indirect_dup_error(&hyper, &mapped_temps)
                }
                hyper_nodes.insert(targets);
            }
        }
    }

    /// Reports an error about a diverging edge. See condition (b) in the file header documentation.
    fn diverging_edge_error<'a>(
        &self,
        label: &LifetimeLabel,
        mut edge: &'a BorrowEdge,
        mut other_edge: &'a BorrowEdge,
        leaves: &BTreeMap<LifetimeLabel, BTreeSet<TempIndex>>,
    ) {
        // Order edges for better error message: the later one in the text should be flagged as error.
        if edge.loc.cmp(&other_edge.loc) == Ordering::Less {
            (other_edge, edge) = (edge, other_edge)
        }
        let (temps, temp_str) = match leaves.get(label) {
            Some(temps) if !temps.is_empty() => (
                temps.clone(),
                format!(
                    "{} ",
                    self.target()
                        .get_local_name_for_error_message(*temps.iter().next().unwrap())
                ),
            ),
            _ => (BTreeSet::new(), "".to_string()),
        };
        let mut info = self.borrow_info(label, |e| e != edge);
        info.push((self.cur_loc(), "requirement enforced here".to_string()));
        self.edge_error(
            edge,
            other_edge,
            temp_str,
            info.into_iter()
                .chain(self.usage_info(&other_edge.target, |t| !temps.contains(t))),
        );
    }

    /// Reports an error about exclusive access requirement for borrows. See
    /// safety condition (c) in the file header documentation.
    fn exclusive_access_borrow_error(&self, label: &LifetimeLabel, temps: &BTreeSet<TempIndex>) {
        self.error_with_hints(
            self.cur_loc(),
            format!(
                "mutable reference in {} requires exclusive access but is borrowed",
                self.display_set(temps)
            ),
            "requirement enforced here",
            self.borrow_info(label, |_| true)
                .into_iter()
                .chain(self.usage_info(label, |t| !temps.contains(t))),
        )
    }

    /// Reports an error about exclusive access requirement for duplicate usage. See safety
    /// condition (d) in the file header documentation. This handles the case were the
    /// same node is used by multiple temps
    fn exclusive_access_indirect_dup_error(
        &self,
        labels: &BTreeSet<LifetimeLabel>,
        temps: &BTreeSet<TempIndex>,
    ) {
        debug_assert!(temps.len() > 1);
        let ts = temps.iter().take(2).collect_vec();
        self.error_with_hints(
            self.cur_loc(),
            format!(
                "same mutable reference in {} is also used in other {} in argument list",
                self.display(*ts[0]),
                self.display(*ts[1])
            ),
            "requirement enforced here",
            labels.iter().flat_map(|l| self.borrow_info(l, |_| true)),
        )
    }

    /// Reports an error about exclusive access requirement for duplicate usage. See safety
    /// condition (d) in the file header documentation. This handles the case were the
    /// same local is used multiple times.
    fn exclusive_access_direct_dup_error(&self, temp: TempIndex) {
        self.error_with_hints(
            self.cur_loc(),
            format!(
                "same mutable reference in {} is used again in argument list",
                self.display(temp),
            ),
            "requirement enforced here",
            iter::empty(),
        )
    }

    /// Reports an error together with hints
    fn error_with_hints(
        &self,
        loc: impl AsRef<Loc>,
        msg: impl AsRef<str>,
        primary: impl AsRef<str>,
        hints: impl Iterator<Item = (Loc, String)>,
    ) {
        if !self.parent.suppress_errors {
            self.global_env().diag_with_primary_and_labels(
                Severity::Error,
                loc.as_ref(),
                msg.as_ref(),
                primary.as_ref(),
                hints.collect(),
            )
        }
    }

    fn edge_error<'a>(
        &self,
        edge: &'a BorrowEdge,
        other_edge: &'a BorrowEdge,
        what: String,
        hints: impl Iterator<Item = (Loc, String)>,
    ) {
        let (action, attempt) = if edge.kind.is_mut() {
            ("mutably", "mutable")
        } else {
            ("immutably", "immutable")
        };
        let reason = if other_edge.kind.is_mut() {
            "mutable references exist"
        } else {
            "immutable references exist"
        };
        self.error_with_hints(
            &edge.loc,
            format!(
                "cannot {action} borrow {what}since {reason}",
                action = action,
                what = what,
                reason = reason
            ),
            format!("{} borrow attempted here", attempt),
            hints,
        );
    }

    #[inline]
    fn global_env(&self) -> &GlobalEnv {
        self.target().global_env()
    }

    #[inline]
    fn target(&self) -> &FunctionTarget {
        self.parent.target
    }

    /// Produces borrow hints for the given node in the graph, for error messages.
    fn borrow_info(
        &self,
        label: &LifetimeLabel,
        filter: impl Fn(&BorrowEdge) -> bool,
    ) -> Vec<(Loc, String)> {
        let leaves = self.state.leaves();
        let primary_edges = self
            .state
            .children(label)
            .filter(|e| filter(e))
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut secondary_edges = BTreeSet::new();
        for edge in primary_edges.iter() {
            // Only include the secondary edge if the primary target is not longer in use. This gives a user an
            // additional hint only if needed.
            if !leaves.contains_key(&edge.target) {
                secondary_edges.extend(self.state.children(&edge.target));
            }
        }
        primary_edges
            .into_iter()
            .map(|e| self.borrow_edge_info("previous ", &e))
            .chain(
                secondary_edges
                    .into_iter()
                    .map(|e| self.borrow_edge_info("used by ", e)),
            )
            .collect::<Vec<_>>()
    }

    fn borrow_edge_info(&self, prefix: &str, e: &BorrowEdge) -> (Loc, String) {
        use BorrowEdgeKind::*;
        let mut_prefix = if e.kind.is_mut() { "mutable " } else { "" };
        (
            e.loc.clone(),
            format!("{}{}{}", prefix, mut_prefix, match &e.kind {
                BorrowLocal(_) => "local borrow",
                BorrowGlobal(_) => "global borrow",
                BorrowField(..) => "field borrow",
                Call(..) => "call result",
                Freeze => "freeze",
            },),
        )
    }

    /// Produces usage information for temporaries involved in the current borrow graph.
    fn usage_info(
        &self,
        label: &LifetimeLabel,
        filter: impl Fn(&TempIndex) -> bool,
    ) -> Vec<(Loc, String)> {
        // Collect the candidates to display. These are temporaries which are alive _after_ this program point
        // and which are in the same path in the graph (i.e. or parent and child of each other).
        let mut cands = vec![];
        for (temp, leaf) in self.state.temp_to_label_map.iter() {
            if self.is_ref(*temp)
                && self.alive.after.contains_key(temp)
                && (self.state.is_ancestor(label, leaf) || self.state.is_ancestor(leaf, label))
                && filter(temp)
            {
                let mut done = false;
                for (ct, cl) in cands.iter_mut() {
                    if self.state.is_ancestor(cl, leaf) {
                        // This leaf is a better proof of the problem as it is derived from the other, replace
                        *ct = *temp;
                        *cl = *leaf;
                        done = true;
                        break;
                    } else if self.state.is_ancestor(leaf, cl) {
                        // The existing one is better than the new one
                        done = true;
                        break;
                    }
                }
                if !done {
                    cands.push((*temp, *leaf))
                }
            }
        }
        // Now compute display
        let mut infos = vec![];
        for (temp, _) in cands {
            if let Some(info) = self.alive.after.get(&temp) {
                for loc in &info.usages {
                    infos.push((
                        loc.clone(),
                        format!(
                            "conflicting reference {}used here",
                            self.display_name_or_empty("", temp)
                        ),
                    ))
                }
            }
        }
        infos
    }
}

// -------------------------------------------------------------------------------------------------
// Program Steps

impl<'env, 'state> LifetimeAnalysisStep<'env, 'state> {
    /// Process an assign instruction. This checks whether the source is currently borrowed and
    /// rejects a move if so.
    fn assign(&mut self, dest: TempIndex, src: TempIndex, kind: AssignKind) {
        // Check validness
        let mode = if kind == AssignKind::Move {
            ReadMode::Move
        } else {
            ReadMode::Copy
        };
        if self.is_ref(src) {
            match kind {
                AssignKind::Move => self.state.move_ref(dest, src),
                AssignKind::Copy => self.state.copy_ref(dest, src),
                AssignKind::Inferred => {
                    if self.state.label_for_temp_with_children(src).is_none()
                        && !self.alive.after.contains_key(&src)
                    {
                        self.state.move_ref(dest, src)
                    } else {
                        self.state.copy_ref(dest, src)
                    }
                },
                AssignKind::Store => panic!("unexpected assign kind"),
            }
        } else {
            self.check_read_local(src, mode);
            self.check_write_local(dest);
        }
    }

    /// Process a borrow local instruction.
    fn borrow_local(&mut self, dest: TempIndex, src: TempIndex) {
        let label = self.state.make_temp(src, self.code_offset, 0, true);
        let child = self.state.replace_ref(dest, self.code_offset, 1);
        let loc = self.cur_loc();
        let is_mut = self.ty(dest).is_mutable_reference();
        self.state.add_edge(
            label,
            BorrowEdge::new(BorrowEdgeKind::BorrowLocal(is_mut), loc, child),
        );
    }

    /// Process a borrow global instruction.
    fn borrow_global(&mut self, struct_: QualifiedInstId<StructId>, dest: TempIndex) {
        let label = self.state.make_global(struct_.clone(), self.code_offset, 0);
        let child = self.state.replace_ref(dest, self.code_offset, 1);
        let loc = self.cur_loc();
        let is_mut = self.ty(dest).is_mutable_reference();
        self.state.add_edge(
            label,
            BorrowEdge::new(BorrowEdgeKind::BorrowGlobal(is_mut), loc, child),
        );
    }

    /// Process a borrow field instruction.
    fn borrow_field(
        &mut self,
        struct_: QualifiedInstId<StructId>,
        field_offs: &usize,
        dest: TempIndex,
        src: TempIndex,
    ) {
        let label = self.state.make_temp(src, self.code_offset, 0, false);
        let child = self.state.replace_ref(dest, self.code_offset, 1);
        let loc = self.cur_loc();
        let is_mut = self.ty(dest).is_mutable_reference();
        let struct_env = self.global_env().get_struct(struct_.to_qualified_id());
        let field_id = struct_env.get_field_by_offset(*field_offs).get_id();
        let edge = BorrowEdge::new(
            BorrowEdgeKind::BorrowField(is_mut, field_id),
            loc.clone(),
            child,
        );
        // Check condition (e) in the file header documentation.
        let node_opt = self.state.graph.get(&label);
        if node_opt.is_some() && is_mut {
            let mut parent_node = None;
            let mut parent_edge = None;
            for parent in node_opt.unwrap().parents.iter() {
                for ch in self.state.children(parent) {
                    if ch.target.0 == label.0 && !ch.kind.is_mut() {
                        parent_node = Some(parent);
                        parent_edge = Some(ch);
                        break;
                    }
                }
                if parent_node.is_some() {
                    break;
                }
            }
            if parent_node.is_some() {
                let name = format!("{} ", self.target().get_local_name_for_error_message(src));
                self.edge_error(
                    &edge,
                    parent_edge.unwrap(),
                    name,
                    self.borrow_info(parent_node.unwrap(), |_| true).into_iter(),
                );
                return;
            }
        }
        self.state.add_edge(label, edge);
    }

    /// Process a function call. For now we implement standard Move semantics, where every
    /// output reference is a child of all input references. Here would be the point where to
    // evaluate lifetime modifiers in future language versions.
    fn call_operation(&mut self, oper: Operation, dests: &[TempIndex], srcs: &[TempIndex]) {
        // Check validness of arguments
        for src in srcs {
            self.check_read_local(*src, ReadMode::Argument);
        }
        // Next check whether we can assign to the destinations.
        for dest in dests {
            self.check_write_local(*dest)
        }
        // Now draw edges from all reference sources to all reference destinations.
        let dest_labels = dests
            .iter()
            .filter(|d| self.ty(**d).is_reference())
            .collect::<Vec<_>>()
            .into_iter()
            .enumerate()
            .map(|(i, t)| (*t, self.state.replace_ref(*t, self.code_offset, i as u8)))
            .collect::<BTreeMap<_, _>>();
        let src_qualifier_offset = dest_labels.len();
        let loc = self.cur_loc();
        for dest in dests {
            let dest_ty = self.ty(*dest).clone();
            if dest_ty.is_reference() {
                for (i, src) in srcs.iter().enumerate() {
                    let src_ty = self.ty(*src);
                    if src_ty.is_reference() {
                        let label = self.state.make_temp(
                            *src,
                            self.code_offset,
                            (src_qualifier_offset + i) as u8,
                            false,
                        );
                        let child = &dest_labels[dest];
                        self.state.add_edge(
                            label,
                            BorrowEdge::new(
                                BorrowEdgeKind::Call(
                                    dest_ty.is_mutable_reference(),
                                    oper.clone(),
                                    self.code_offset,
                                ),
                                loc.clone(),
                                *child,
                            ),
                        )
                    }
                }
            }
        }
    }

    /// Process a FreezeRef instruction.
    fn freeze_ref(&mut self, code_offset: CodeOffset, dest: TempIndex, src: TempIndex) {
        let label = *self.state.label_for_temp(src).expect("label for reference");
        let target = self.state.replace_ref(dest, code_offset, 0);
        self.state.add_edge(label, BorrowEdge {
            kind: BorrowEdgeKind::Freeze,
            loc: self.cur_loc(),
            target,
        })
    }

    /// Process a MoveFrom instruction.
    fn move_from(&mut self, dest: TempIndex, resource: &QualifiedInstId<StructId>, src: TempIndex) {
        self.check_read_local(src, ReadMode::Argument);
        self.check_write_local(dest);
        if let Some(label) = self.state.label_for_global_with_children(resource) {
            self.error_with_hints(
                self.cur_loc(),
                format!(
                    "cannot extract resource `{}` which is still borrowed",
                    self.global_env().display(resource)
                ),
                "extracted here",
                self.borrow_info(label, |_| true).into_iter(),
            )
        }
    }

    /// Process a return instruction.
    fn return_(&mut self, srcs: &[TempIndex]) {
        for src in srcs {
            if self.ty(*src).is_reference() {
                // Need to check whether this reference is derived from a local which is not a
                // a parameter
                if let Some(label) = self.state.label_for_temp(*src) {
                    for root in self.state.roots(label) {
                        for location in self.state.node(&root).locations.iter() {
                            match location {
                                MemoryLocation::Global(resource) => self.error_with_hints(
                                    self.cur_loc(),
                                    format!(
                                        "cannot return a reference derived from global `{}`",
                                        self.global_env().display(resource)
                                    ),
                                    "returned here",
                                    self.borrow_info(&root, |_| true).into_iter(),
                                ),
                                MemoryLocation::Local(local) => {
                                    if *local >= self.target().get_parameter_count() {
                                        self.error_with_hints(
                                            self.cur_loc(),
                                            format!(
                                                "cannot return a reference derived from {} since it is not a parameter",
                                                self.display(*local)
                                            ),
                                            "returned here",
                                            self.borrow_info(&root, |_| true).into_iter(),
                                        )
                                    }
                                },
                                MemoryLocation::External | MemoryLocation::Derived => {},
                            }
                        }
                    }
                }
            }
        }
    }

    /// Process a ReadRef instruction.
    fn read_ref(&mut self, dest: TempIndex, src: TempIndex) {
        debug_assert!(self.is_ref(src));
        self.check_write_local(dest);
        self.check_read_local(src, ReadMode::Argument);
    }

    /// Process a WriteRef instruction.
    fn write_ref(&mut self, dest: TempIndex, src: TempIndex) {
        self.check_read_local(src, ReadMode::Argument);
        if let Some(label) = self.state.label_for_temp_with_children(dest) {
            self.error_with_hints(
                self.cur_loc(),
                format!(
                    "cannot write to reference in {} which is still borrowed",
                    self.display(dest)
                ),
                "written here",
                self.borrow_info(label, |_| true).into_iter(),
            )
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Transfer Function

impl<'env> TransferFunctions for LifeTimeAnalysis<'env> {
    type State = LifetimeState;

    const BACKWARD: bool = false;

    /// Transfer function for given bytecode.
    fn execute(&self, state: &mut Self::State, instr: &Bytecode, code_offset: CodeOffset) {
        use Bytecode::*;

        // Construct step context
        let mut step = self.new_step(code_offset, instr.get_attr_id(), state);

        // Preprocessing: release all temps in the label map which are not longer alive at this point.
        step.state.debug_print("before enter release");
        let alive_temps = step.alive.before_set();
        for temp in step
            .state
            .temp_to_label_map
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            if !alive_temps.contains(&temp) && step.is_ref(temp) {
                step.state.release_ref(temp)
            }
        }

        // Preprocessing: check borrow safety of the currently active borrow graph for read ref,
        // write ref, and function calls.
        #[allow(clippy::single_match)]
        match instr {
            // Only handle operations which can take references
            Call(_, _, oper, srcs, ..) => match oper {
                Operation::ReadRef
                | Operation::WriteRef
                | Operation::Function(..)
                | Operation::Eq
                | Operation::Neq => {
                    let exclusive_refs = srcs
                        .iter()
                        .filter(|t| step.is_ref(**t))
                        .cloned()
                        .collect_vec();
                    step.check_borrow_safety(&exclusive_refs)
                },
                _ => {},
            },
            Ret(_, srcs) => {
                let exclusive_refs = srcs
                    .iter()
                    .filter(|t| step.is_ref(**t))
                    .cloned()
                    .collect_vec();
                step.check_borrow_safety(&exclusive_refs)
            },
            _ => {},
        }

        // Process the instruction
        match instr {
            Assign(_, dest, src, kind) => {
                step.assign(*dest, *src, *kind);
            },
            Ret(_, srcs) => step.return_(srcs),
            Call(_, dests, oper, srcs, _) => {
                use Operation::*;
                match oper {
                    BorrowLoc => {
                        step.borrow_local(dests[0], srcs[0]);
                    },
                    BorrowGlobal(mid, sid, inst) => {
                        step.borrow_global(mid.qualified_inst(*sid, inst.clone()), dests[0]);
                    },
                    BorrowField(mid, sid, inst, field_offs) => {
                        let (dest, src) = (dests[0], srcs[0]);
                        step.borrow_field(
                            mid.qualified_inst(*sid, inst.clone()),
                            field_offs,
                            dest,
                            src,
                        );
                    },
                    ReadRef => step.read_ref(dests[0], srcs[0]),
                    WriteRef => step.write_ref(srcs[0], srcs[1]),
                    FreezeRef => step.freeze_ref(code_offset, dests[0], srcs[0]),
                    MoveFrom(mid, sid, inst) => {
                        step.move_from(dests[0], &mid.qualified_inst(*sid, inst.clone()), srcs[0])
                    },
                    _ => step.call_operation(oper.clone(), dests, srcs),
                }
            },
            _ => {},
        }
        // After processing, release any temporaries which are dying at this program point.
        // Variables which are introduced in this step but not alive after need to be released as well, as they
        // are not in the before set.
        step.state.debug_print("before exit release");
        let after_set = step.alive.after_set();
        for released in step.alive.before.keys().chain(
            instr
                .dests()
                .iter()
                .filter(|t| !step.alive.before.contains_key(t)),
        ) {
            if !after_set.contains(released) && step.is_ref(*released) {
                step.state.release_ref(*released)
            }
        }
    }
}

/// Instantiate the data flow analysis framework based on the transfer function
impl<'env> DataflowAnalysis for LifeTimeAnalysis<'env> {}

// ===============================================================================
// Processor

pub struct ReferenceSafetyProcessor {}

/// Annotation produced by this processor
#[derive(Clone, Debug)]
pub struct LifetimeAnnotation(BTreeMap<CodeOffset, LifetimeInfoAtCodeOffset>);

impl LifetimeAnnotation {
    /// Returns information for code offset.
    pub fn get_info_at(&self, code_offset: CodeOffset) -> &LifetimeInfoAtCodeOffset {
        self.0.get(&code_offset).expect("lifetime info")
    }
}

/// Annotation present at each code offset
#[derive(Debug, Clone, Default)]
pub struct LifetimeInfoAtCodeOffset {
    pub before: LifetimeState,
    pub after: LifetimeState,
}

/// Public functions on lifetime info
impl LifetimeInfoAtCodeOffset {
    /// Returns the temporaries which are released at the give code offset since they are not borrowed
    /// any longer. Notice that this is only for temporaries which are actually borrowed, other
    /// temporaries being released need to be determined from livevar analysis results.
    pub fn released_temps(&self) -> impl Iterator<Item = TempIndex> + '_ {
        self.before
            .temp_to_label_map
            .keys()
            .filter(|t| !self.after.temp_to_label_map.contains_key(t))
            .cloned()
    }
}

impl FunctionTargetProcessor for ReferenceSafetyProcessor {
    fn process(
        &self,
        _targets: &mut FunctionTargetsHolder,
        fun_env: &FunctionEnv,
        mut data: FunctionData,
        _scc_opt: Option<&[FunctionEnv]>,
    ) -> FunctionData {
        if fun_env.is_native() {
            return data;
        }
        let target = FunctionTarget::new(fun_env, &data);
        let live_var_annotation = target
            .get_annotations()
            .get::<LiveVarAnnotation>()
            .expect("livevar annotation");
        let suppress_errors = fun_env
            .module_env
            .env
            .get_extension::<Options>()
            .unwrap_or_default()
            .experiment_on(Experiment::NO_SAFETY);
        let analyzer = LifeTimeAnalysis {
            target: &target,
            live_var_annotation,
            suppress_errors,
        };
        let code = target.get_bytecode();
        let cfg = StacklessControlFlowGraph::new_forward(code);
        let mut state = LifetimeState::default();
        let mut label_counter: u32 = 0;
        for (i, Parameter(_, ty, loc)) in fun_env.get_parameters().into_iter().enumerate() {
            if ty.is_reference() {
                let label = LifetimeLabel::new_from_counter(label_counter);
                label_counter += 1;
                state.new_node(label, MemoryLocation::External);
                let target = state.make_temp_from_label_fun(
                    i,
                    || LifetimeLabel::new_from_counter(label_counter),
                    false,
                );
                label_counter += 1;
                state.add_edge(label, BorrowEdge {
                    kind: BorrowEdgeKind::BorrowLocal(ty.is_mutable_reference()),
                    loc,
                    target,
                })
            }
        }
        let state_map = analyzer.analyze_function(state, target.get_bytecode(), &cfg);
        let state_map_per_instr = analyzer.state_per_instruction_with_default(
            state_map,
            target.get_bytecode(),
            &cfg,
            |before, after| LifetimeInfoAtCodeOffset {
                before: before.clone(),
                after: after.clone(),
            },
        );
        let annotation = LifetimeAnnotation(state_map_per_instr);
        data.annotations.set(annotation, true);
        data
    }

    fn name(&self) -> String {
        "ReferenceSafetyProcessor".to_owned()
    }
}

// ===============================================================================================
// Display

impl ReferenceSafetyProcessor {
    /// Registers annotation formatter at the given function target. This is for debugging and
    /// testing.
    pub fn register_formatters(target: &FunctionTarget) {
        target.register_annotation_formatter(Box::new(format_lifetime_annotation))
    }
}

struct BorrowEdgeDisplay<'a>(&'a FunctionTarget<'a>, &'a BorrowEdge, bool);
impl<'a> Display for BorrowEdgeDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let edge = &self.1;
        let display_child = self.2;
        use BorrowEdgeKind::*;
        (match &edge.kind {
            BorrowLocal(is_mut) => write!(f, "borrow({})", is_mut),
            BorrowGlobal(is_mut) => write!(f, "borrow_global({})", is_mut),
            BorrowField(is_mut, _) => write!(f, "borrow_field({})", is_mut),
            Call(is_mut, _, _) => write!(f, "call({})", is_mut),
            Freeze => write!(f, "freeze"),
        })?;
        if display_child {
            write!(f, " -> {}", edge.target)
        } else {
            Ok(())
        }
    }
}

impl BorrowEdge {
    fn display<'a>(
        &'a self,
        target: &'a FunctionTarget,
        display_child: bool,
    ) -> BorrowEdgeDisplay<'a> {
        BorrowEdgeDisplay(target, self, display_child)
    }
}

impl Display for LifetimeLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{:X}", self.0)
    }
}

struct MemoryLocationDisplay<'a>(&'a FunctionTarget<'a>, &'a MemoryLocation);
impl<'a> Display for MemoryLocationDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        use MemoryLocation::*;
        let env = self.0.global_env();
        match self.1 {
            Global(qid) => write!(f, "global<{}>", env.display(qid)),
            Local(temp) => write!(
                f,
                "local({})",
                env.display(&self.0.get_local_raw_name(*temp))
            ),
            External => write!(f, "external"),
            Derived => write!(f, "derived"),
        }
    }
}
impl MemoryLocation {
    fn display<'a>(&'a self, fun: &'a FunctionTarget) -> MemoryLocationDisplay<'a> {
        MemoryLocationDisplay(fun, self)
    }
}

struct LifetimeNodeDisplay<'a>(&'a FunctionTarget<'a>, &'a LifetimeNode);
impl<'a> Display for LifetimeNodeDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}[",
            self.1.locations.iter().map(|l| l.display(self.0)).join(",")
        )?;
        f.write_str(
            &self
                .1
                .children
                .iter()
                .map(|e| e.display(self.0, true).to_string())
                .join(","),
        )?;
        f.write_str("]")
    }
}
impl LifetimeNode {
    fn display<'a>(&'a self, fun: &'a FunctionTarget) -> LifetimeNodeDisplay<'a> {
        LifetimeNodeDisplay(fun, self)
    }
}

struct LifetimeStateDisplay<'a>(&'a FunctionTarget<'a>, &'a LifetimeState);
impl<'a> Display for LifetimeStateDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let LifetimeState {
            graph,
            temp_to_label_map,
            global_to_label_map,
        } = &self.1;
        let pool = self.0.global_env().symbol_pool();
        writeln!(
            f,
            "graph: {}",
            graph.to_string(|k| k.to_string(), |v| v.display(self.0).to_string())
        )?;
        writeln!(
            f,
            "locals: {{{}}}",
            temp_to_label_map
                .iter()
                .map(|(temp, label)| format!(
                    "{}={}",
                    self.0.get_local_raw_name(*temp).display(pool),
                    label
                ))
                .join(",")
        )?;
        writeln!(
            f,
            "globals: {{{}}}",
            global_to_label_map
                .iter()
                .map(|(str, label)| format!("{}={}", self.0.global_env().display(str), label))
                .join(",")
        )
    }
}

impl LifetimeState {
    fn display<'a>(&'a self, fun: &'a FunctionTarget) -> LifetimeStateDisplay<'a> {
        LifetimeStateDisplay(fun, self)
    }
}

fn format_lifetime_annotation(
    target: &FunctionTarget<'_>,
    code_offset: CodeOffset,
) -> Option<String> {
    if let Some(LifetimeAnnotation(map)) = target.get_annotations().get::<LifetimeAnnotation>() {
        if let Some(at) = map.get(&code_offset) {
            return Some(at.before.display(target).to_string());
        }
    }
    None
}
