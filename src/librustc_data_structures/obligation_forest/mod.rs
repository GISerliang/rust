//! The `ObligationForest` is a utility data structure used in trait
//! matching to track the set of outstanding obligations (those not yet
//! resolved to success or error). It also tracks the "backtrace" of each
//! pending obligation (why we are trying to figure this out in the first
//! place).
//!
//! ### External view
//!
//! `ObligationForest` supports two main public operations (there are a
//! few others not discussed here):
//!
//! 1. Add a new root obligations (`register_obligation`).
//! 2. Process the pending obligations (`process_obligations`).
//!
//! When a new obligation `N` is added, it becomes the root of an
//! obligation tree. This tree can also carry some per-tree state `T`,
//! which is given at the same time. This tree is a singleton to start, so
//! `N` is both the root and the only leaf. Each time the
//! `process_obligations` method is called, it will invoke its callback
//! with every pending obligation (so that will include `N`, the first
//! time). The callback also receives a (mutable) reference to the
//! per-tree state `T`. The callback should process the obligation `O`
//! that it is given and return a `ProcessResult`:
//!
//! - `Unchanged` -> ambiguous result. Obligation was neither a success
//!   nor a failure. It is assumed that further attempts to process the
//!   obligation will yield the same result unless something in the
//!   surrounding environment changes.
//! - `Changed(C)` - the obligation was *shallowly successful*. The
//!   vector `C` is a list of subobligations. The meaning of this is that
//!   `O` was successful on the assumption that all the obligations in `C`
//!   are also successful. Therefore, `O` is only considered a "true"
//!   success if `C` is empty. Otherwise, `O` is put into a suspended
//!   state and the obligations in `C` become the new pending
//!   obligations. They will be processed the next time you call
//!   `process_obligations`.
//! - `Error(E)` -> obligation failed with error `E`. We will collect this
//!   error and return it from `process_obligations`, along with the
//!   "backtrace" of obligations (that is, the list of obligations up to
//!   and including the root of the failed obligation). No further
//!   obligations from that same tree will be processed, since the tree is
//!   now considered to be in error.
//!
//! When the call to `process_obligations` completes, you get back an `Outcome`,
//! which includes three bits of information:
//!
//! - `completed`: a list of obligations where processing was fully
//!   completed without error (meaning that all transitive subobligations
//!   have also been completed). So, for example, if the callback from
//!   `process_obligations` returns `Changed(C)` for some obligation `O`,
//!   then `O` will be considered completed right away if `C` is the
//!   empty vector. Otherwise it will only be considered completed once
//!   all the obligations in `C` have been found completed.
//! - `errors`: a list of errors that occurred and associated backtraces
//!   at the time of error, which can be used to give context to the user.
//! - `stalled`: if true, then none of the existing obligations were
//!   *shallowly successful* (that is, no callback returned `Changed(_)`).
//!   This implies that all obligations were either errors or returned an
//!   ambiguous result, which means that any further calls to
//!   `process_obligations` would simply yield back further ambiguous
//!   results. This is used by the `FulfillmentContext` to decide when it
//!   has reached a steady state.
//!
//! ### Implementation details
//!
//! For the most part, comments specific to the implementation are in the
//! code. This file only contains a very high-level overview. Basically,
//! the forest is stored in a vector. Each element of the vector is a node
//! in some tree. Each node in the vector has the index of its dependents,
//! including the first dependent which is known as the parent. It also
//! has a current state, described by `NodeState`. After each processing
//! step, we compress the vector to remove completed and error nodes, which
//! aren't needed anymore.

use crate::fx::{FxHashMap, FxHashSet};
use crate::indexed_vec::Idx;
use crate::newtype_index;

use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::hash;
use std::marker::PhantomData;

mod graphviz;

#[cfg(test)]
mod tests;

newtype_index! {
    pub struct NodeIndex { .. }
}

pub trait ForestObligation : Clone + Debug {
    type Predicate : Clone + hash::Hash + Eq + Debug;

    fn as_predicate(&self) -> &Self::Predicate;
}

pub trait ObligationProcessor {
    type Obligation : ForestObligation;
    type Error : Debug;

    fn process_obligation(&mut self,
                          obligation: &mut Self::Obligation)
                          -> ProcessResult<Self::Obligation, Self::Error>;

    /// As we do the cycle check, we invoke this callback when we
    /// encounter an actual cycle. `cycle` is an iterator that starts
    /// at the start of the cycle in the stack and walks **toward the
    /// top**.
    ///
    /// In other words, if we had O1 which required O2 which required
    /// O3 which required O1, we would give an iterator yielding O1,
    /// O2, O3 (O1 is not yielded twice).
    fn process_backedge<'c, I>(&mut self,
                               cycle: I,
                               _marker: PhantomData<&'c Self::Obligation>)
        where I: Clone + Iterator<Item=&'c Self::Obligation>;
}

/// The result type used by `process_obligation`.
#[derive(Debug)]
pub enum ProcessResult<O, E> {
    Unchanged,
    Changed(Vec<O>),
    Error(E),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ObligationTreeId(usize);

type ObligationTreeIdGenerator =
    ::std::iter::Map<::std::ops::RangeFrom<usize>, fn(usize) -> ObligationTreeId>;

pub struct ObligationForest<O: ForestObligation> {
    /// The list of obligations. In between calls to
    /// `process_obligations`, this list only contains nodes in the
    /// `Pending` or `Success` state (with a non-zero number of
    /// incomplete children). During processing, some of those nodes
    /// may be changed to the error state, or we may find that they
    /// are completed (That is, `num_incomplete_children` drops to 0).
    /// At the end of processing, those nodes will be removed by a
    /// call to `compress`.
    ///
    /// Ideally, this would be an `IndexVec<NodeIndex, Node<O>>`. But that is
    /// slower, because this vector is accessed so often that the
    /// `u32`-to-`usize` conversions required for accesses are significant.
    nodes: Vec<Node<O>>,

    /// A cache of predicates that have been successfully completed.
    done_cache: FxHashSet<O::Predicate>,

    /// A cache of the nodes in `nodes`, indexed by predicate. Unfortunately,
    /// its contents are not guaranteed to match those of `nodes`. See the
    /// comments in `process_obligation` for details.
    waiting_cache: FxHashMap<O::Predicate, NodeIndex>,

    /// A scratch vector reused in various operations, to avoid allocating new
    /// vectors.
    scratch: RefCell<Vec<usize>>,

    obligation_tree_id_generator: ObligationTreeIdGenerator,

    /// Per tree error cache. This is used to deduplicate errors,
    /// which is necessary to avoid trait resolution overflow in
    /// some cases.
    ///
    /// See [this][details] for details.
    ///
    /// [details]: https://github.com/rust-lang/rust/pull/53255#issuecomment-421184780
    error_cache: FxHashMap<ObligationTreeId, FxHashSet<O::Predicate>>,
}

#[derive(Debug)]
struct Node<O> {
    obligation: O,
    state: Cell<NodeState>,

    /// The parent of a node - the original obligation of which it is a
    /// subobligation. Except for error reporting, it is just like any member
    /// of `dependents`.
    ///
    /// Unlike `ObligationForest::nodes`, this uses `NodeIndex` rather than
    /// `usize` for the index, because keeping the size down is more important
    /// than the cost of converting to a `usize` for indexing.
    parent: Option<NodeIndex>,

    /// Obligations that depend on this obligation for their completion. They
    /// must all be in a non-pending state.
    ///
    /// This uses `NodeIndex` for the same reason as `parent`.
    dependents: Vec<NodeIndex>,

    /// Identifier of the obligation tree to which this node belongs.
    obligation_tree_id: ObligationTreeId,
}

/// The state of one node in some tree within the forest. This
/// represents the current state of processing for the obligation (of
/// type `O`) associated with this node.
///
/// Outside of ObligationForest methods, nodes should be either Pending
/// or Waiting.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum NodeState {
    /// Obligations for which selection had not yet returned a
    /// non-ambiguous result.
    Pending,

    /// This obligation was selected successfully, but may or
    /// may not have subobligations.
    Success,

    /// This obligation was selected successfully, but it has
    /// a pending subobligation.
    Waiting,

    /// This obligation, along with its subobligations, are complete,
    /// and will be removed in the next collection.
    Done,

    /// This obligation was resolved to an error. Error nodes are
    /// removed from the vector by the compression step.
    Error,

    /// This is a temporary state used in DFS loops to detect cycles,
    /// it should not exist outside of these DFSes.
    OnDfsStack,
}

#[derive(Debug)]
pub struct Outcome<O, E> {
    /// Obligations that were completely evaluated, including all
    /// (transitive) subobligations. Only computed if requested.
    pub completed: Option<Vec<O>>,

    /// Backtrace of obligations that were found to be in error.
    pub errors: Vec<Error<O, E>>,

    /// If true, then we saw no successful obligations, which means
    /// there is no point in further iteration. This is based on the
    /// assumption that when trait matching returns `Error` or
    /// `Unchanged`, those results do not affect environmental
    /// inference state. (Note that if we invoke `process_obligations`
    /// with no pending obligations, stalled will be true.)
    pub stalled: bool,
}

/// Should `process_obligations` compute the `Outcome::completed` field of its
/// result?
#[derive(PartialEq)]
pub enum DoCompleted {
    No,
    Yes,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Error<O, E> {
    pub error: E,
    pub backtrace: Vec<O>,
}

impl<O: ForestObligation> ObligationForest<O> {
    pub fn new() -> ObligationForest<O> {
        ObligationForest {
            nodes: vec![],
            done_cache: Default::default(),
            waiting_cache: Default::default(),
            scratch: RefCell::new(vec![]),
            obligation_tree_id_generator: (0..).map(ObligationTreeId),
            error_cache: Default::default(),
        }
    }

    /// Returns the total number of nodes in the forest that have not
    /// yet been fully resolved.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Registers an obligation.
    pub fn register_obligation(&mut self, obligation: O) {
        // Ignore errors here - there is no guarantee of success.
        let _ = self.register_obligation_at(obligation, None);
    }

    // returns Err(()) if we already know this obligation failed.
    fn register_obligation_at(&mut self, obligation: O, parent: Option<NodeIndex>)
                              -> Result<(), ()>
    {
        if self.done_cache.contains(obligation.as_predicate()) {
            return Ok(());
        }

        match self.waiting_cache.entry(obligation.as_predicate().clone()) {
            Entry::Occupied(o) => {
                debug!("register_obligation_at({:?}, {:?}) - duplicate of {:?}!",
                       obligation, parent, o.get());
                let node = &mut self.nodes[o.get().index()];
                if let Some(parent_index) = parent {
                    // If the node is already in `waiting_cache`, it's already
                    // been marked with a parent. (It's possible that parent
                    // has been cleared by `apply_rewrites`, though.) So just
                    // dump `parent` into `node.dependents`... unless it's
                    // already in `node.dependents` or `node.parent`.
                    if !node.dependents.contains(&parent_index) &&
                       Some(parent_index) != node.parent {
                        node.dependents.push(parent_index);
                    }
                }
                if let NodeState::Error = node.state.get() {
                    Err(())
                } else {
                    Ok(())
                }
            }
            Entry::Vacant(v) => {
                debug!("register_obligation_at({:?}, {:?}) - ok, new index is {}",
                       obligation, parent, self.nodes.len());

                let obligation_tree_id = match parent {
                    Some(parent_index) => {
                        self.nodes[parent_index.index()].obligation_tree_id
                    }
                    None => self.obligation_tree_id_generator.next().unwrap()
                };

                let already_failed =
                    parent.is_some()
                        && self.error_cache
                            .get(&obligation_tree_id)
                            .map(|errors| errors.contains(obligation.as_predicate()))
                            .unwrap_or(false);

                if already_failed {
                    Err(())
                } else {
                    v.insert(NodeIndex::new(self.nodes.len()));
                    self.nodes.push(Node::new(parent, obligation, obligation_tree_id));
                    Ok(())
                }
            }
        }
    }

    /// Converts all remaining obligations to the given error.
    pub fn to_errors<E: Clone>(&mut self, error: E) -> Vec<Error<O, E>> {
        let mut errors = vec![];
        for (i, node) in self.nodes.iter().enumerate() {
            if let NodeState::Pending = node.state.get() {
                let backtrace = self.error_at(i);
                errors.push(Error {
                    error: error.clone(),
                    backtrace,
                });
            }
        }
        let successful_obligations = self.compress(DoCompleted::Yes);
        assert!(successful_obligations.unwrap().is_empty());
        errors
    }

    /// Returns the set of obligations that are in a pending state.
    pub fn map_pending_obligations<P, F>(&self, f: F) -> Vec<P>
        where F: Fn(&O) -> P
    {
        self.nodes
            .iter()
            .filter(|n| n.state.get() == NodeState::Pending)
            .map(|n| f(&n.obligation))
            .collect()
    }

    fn insert_into_error_cache(&mut self, node_index: usize) {
        let node = &self.nodes[node_index];
        self.error_cache
            .entry(node.obligation_tree_id)
            .or_default()
            .insert(node.obligation.as_predicate().clone());
    }

    /// Performs a pass through the obligation list. This must
    /// be called in a loop until `outcome.stalled` is false.
    ///
    /// This _cannot_ be unrolled (presently, at least).
    pub fn process_obligations<P>(&mut self, processor: &mut P, do_completed: DoCompleted)
                                  -> Outcome<O, P::Error>
        where P: ObligationProcessor<Obligation=O>
    {
        debug!("process_obligations(len={})", self.nodes.len());

        let mut errors = vec![];
        let mut stalled = true;

        for i in 0..self.nodes.len() {
            let node = &mut self.nodes[i];

            debug!("process_obligations: node {} == {:?}", i, node);

            // `processor.process_obligation` can modify the predicate within
            // `node.obligation`, and that predicate is the key used for
            // `self.waiting_cache`. This means that `self.waiting_cache` can
            // get out of sync with `nodes`. It's not very common, but it does
            // happen, and code in `compress` has to allow for it.
            let result = match node.state.get() {
                NodeState::Pending => processor.process_obligation(&mut node.obligation),
                _ => continue
            };

            debug!("process_obligations: node {} got result {:?}", i, result);

            match result {
                ProcessResult::Unchanged => {
                    // No change in state.
                }
                ProcessResult::Changed(children) => {
                    // We are not (yet) stalled.
                    stalled = false;
                    node.state.set(NodeState::Success);

                    for child in children {
                        let st = self.register_obligation_at(
                            child,
                            Some(NodeIndex::new(i))
                        );
                        if let Err(()) = st {
                            // error already reported - propagate it
                            // to our node.
                            self.error_at(i);
                        }
                    }
                }
                ProcessResult::Error(err) => {
                    stalled = false;
                    let backtrace = self.error_at(i);
                    errors.push(Error {
                        error: err,
                        backtrace,
                    });
                }
            }
        }

        if stalled {
            // There's no need to perform marking, cycle processing and compression when nothing
            // changed.
            return Outcome {
                completed: if do_completed == DoCompleted::Yes { Some(vec![]) } else { None },
                errors,
                stalled,
            };
        }

        self.mark_as_waiting();
        self.process_cycles(processor);

        // Now we have to compress the result
        let completed = self.compress(do_completed);

        debug!("process_obligations: complete");

        Outcome {
            completed,
            errors,
            stalled,
        }
    }

    /// Mark all `NodeState::Success` nodes as `NodeState::Done` and
    /// report all cycles between them. This should be called
    /// after `mark_as_waiting` marks all nodes with pending
    /// subobligations as NodeState::Waiting.
    fn process_cycles<P>(&self, processor: &mut P)
        where P: ObligationProcessor<Obligation=O>
    {
        let mut stack = self.scratch.replace(vec![]);
        debug_assert!(stack.is_empty());

        debug!("process_cycles()");

        for (i, node) in self.nodes.iter().enumerate() {
            // For rustc-benchmarks/inflate-0.1.0 this state test is extremely
            // hot and the state is almost always `Pending` or `Waiting`. It's
            // a win to handle the no-op cases immediately to avoid the cost of
            // the function call.
            match node.state.get() {
                NodeState::Waiting | NodeState::Pending | NodeState::Done | NodeState::Error => {},
                _ => self.find_cycles_from_node(&mut stack, processor, i),
            }
        }

        debug!("process_cycles: complete");

        debug_assert!(stack.is_empty());
        self.scratch.replace(stack);
    }

    fn find_cycles_from_node<P>(&self, stack: &mut Vec<usize>, processor: &mut P, i: usize)
        where P: ObligationProcessor<Obligation=O>
    {
        let node = &self.nodes[i];
        match node.state.get() {
            NodeState::OnDfsStack => {
                let i = stack.iter().rposition(|n| *n == i).unwrap();
                processor.process_backedge(stack[i..].iter().map(GetObligation(&self.nodes)),
                                           PhantomData);
            }
            NodeState::Success => {
                node.state.set(NodeState::OnDfsStack);
                stack.push(i);
                for index in node.parent.iter().chain(node.dependents.iter()) {
                    self.find_cycles_from_node(stack, processor, index.index());
                }
                stack.pop();
                node.state.set(NodeState::Done);
            },
            NodeState::Waiting | NodeState::Pending => {
                // this node is still reachable from some pending node. We
                // will get to it when they are all processed.
            }
            NodeState::Done | NodeState::Error => {
                // already processed that node
            }
        };
    }

    /// Returns a vector of obligations for `p` and all of its
    /// ancestors, putting them into the error state in the process.
    fn error_at(&self, mut i: usize) -> Vec<O> {
        let mut error_stack = self.scratch.replace(vec![]);
        let mut trace = vec![];

        loop {
            let node = &self.nodes[i];
            node.state.set(NodeState::Error);
            trace.push(node.obligation.clone());
            error_stack.extend(node.dependents.iter().map(|index| index.index()));

            // Loop to the parent.
            match node.parent {
                Some(parent_index) => i = parent_index.index(),
                None => break
            }
        }

        while let Some(i) = error_stack.pop() {
            let node = &self.nodes[i];
            match node.state.get() {
                NodeState::Error => continue,
                _ => node.state.set(NodeState::Error),
            }

            error_stack.extend(
                node.parent.iter().chain(node.dependents.iter()).map(|index| index.index())
            );
        }

        self.scratch.replace(error_stack);
        trace
    }

    // This always-inlined function is for the hot call site.
    #[inline(always)]
    fn inlined_mark_neighbors_as_waiting_from(&self, node: &Node<O>) {
        for dependent in node.parent.iter().chain(node.dependents.iter()) {
            self.mark_as_waiting_from(&self.nodes[dependent.index()]);
        }
    }

    // This never-inlined function is for the cold call site.
    #[inline(never)]
    fn uninlined_mark_neighbors_as_waiting_from(&self, node: &Node<O>) {
        self.inlined_mark_neighbors_as_waiting_from(node)
    }

    /// Marks all nodes that depend on a pending node as `NodeState::Waiting`.
    fn mark_as_waiting(&self) {
        for node in &self.nodes {
            if node.state.get() == NodeState::Waiting {
                node.state.set(NodeState::Success);
            }
        }

        for node in &self.nodes {
            if node.state.get() == NodeState::Pending {
                // This call site is hot.
                self.inlined_mark_neighbors_as_waiting_from(node);
            }
        }
    }

    fn mark_as_waiting_from(&self, node: &Node<O>) {
        match node.state.get() {
            NodeState::Waiting | NodeState::Error | NodeState::OnDfsStack => return,
            NodeState::Success => node.state.set(NodeState::Waiting),
            NodeState::Pending | NodeState::Done => {},
        }

        // This call site is cold.
        self.uninlined_mark_neighbors_as_waiting_from(node);
    }

    /// Compresses the vector, removing all popped nodes. This adjusts
    /// the indices and hence invalidates any outstanding
    /// indices. Cannot be used during a transaction.
    ///
    /// Beforehand, all nodes must be marked as `Done` and no cycles
    /// on these nodes may be present. This is done by e.g., `process_cycles`.
    #[inline(never)]
    fn compress(&mut self, do_completed: DoCompleted) -> Option<Vec<O>> {
        let nodes_len = self.nodes.len();
        let mut node_rewrites: Vec<_> = self.scratch.replace(vec![]);
        node_rewrites.extend(0..nodes_len);
        let mut dead_nodes = 0;

        // Now move all popped nodes to the end. Try to keep the order.
        //
        // LOOP INVARIANT:
        //     self.nodes[0..i - dead_nodes] are the first remaining nodes
        //     self.nodes[i - dead_nodes..i] are all dead
        //     self.nodes[i..] are unchanged
        for i in 0..self.nodes.len() {
            let node = &self.nodes[i];
            match node.state.get() {
                NodeState::Pending | NodeState::Waiting => {
                    if dead_nodes > 0 {
                        self.nodes.swap(i, i - dead_nodes);
                        node_rewrites[i] -= dead_nodes;
                    }
                }
                NodeState::Done => {
                    // This lookup can fail because the contents of
                    // `self.waiting_cache` is not guaranteed to match those of
                    // `self.nodes`. See the comment in `process_obligation`
                    // for more details.
                    if let Some((predicate, _)) = self.waiting_cache
                        .remove_entry(node.obligation.as_predicate())
                    {
                        self.done_cache.insert(predicate);
                    } else {
                        self.done_cache.insert(node.obligation.as_predicate().clone());
                    }
                    node_rewrites[i] = nodes_len;
                    dead_nodes += 1;
                }
                NodeState::Error => {
                    // We *intentionally* remove the node from the cache at this point. Otherwise
                    // tests must come up with a different type on every type error they
                    // check against.
                    self.waiting_cache.remove(node.obligation.as_predicate());
                    node_rewrites[i] = nodes_len;
                    dead_nodes += 1;
                    self.insert_into_error_cache(i);
                }
                NodeState::OnDfsStack | NodeState::Success => unreachable!()
            }
        }

        // No compression needed.
        if dead_nodes == 0 {
            node_rewrites.truncate(0);
            self.scratch.replace(node_rewrites);
            return if do_completed == DoCompleted::Yes { Some(vec![]) } else { None };
        }

        // Pop off all the nodes we killed and extract the success
        // stories.
        let successful = if do_completed == DoCompleted::Yes {
            Some((0..dead_nodes)
                .map(|_| self.nodes.pop().unwrap())
                .flat_map(|node| {
                    match node.state.get() {
                        NodeState::Error => None,
                        NodeState::Done => Some(node.obligation),
                        _ => unreachable!()
                    }
                })
                .collect())
        } else {
            self.nodes.truncate(self.nodes.len() - dead_nodes);
            None
        };
        self.apply_rewrites(&node_rewrites);

        node_rewrites.truncate(0);
        self.scratch.replace(node_rewrites);

        successful
    }

    fn apply_rewrites(&mut self, node_rewrites: &[usize]) {
        let nodes_len = node_rewrites.len();

        for node in &mut self.nodes {
            if let Some(index) = node.parent {
                let new_i = node_rewrites[index.index()];
                if new_i >= nodes_len {
                    // parent dead due to error
                    node.parent = None;
                } else {
                    node.parent = Some(NodeIndex::new(new_i));
                }
            }

            let mut i = 0;
            while i < node.dependents.len() {
                let new_i = node_rewrites[node.dependents[i].index()];
                if new_i >= nodes_len {
                    node.dependents.swap_remove(i);
                } else {
                    node.dependents[i] = NodeIndex::new(new_i);
                    i += 1;
                }
            }
        }

        // This updating of `self.waiting_cache` is necessary because the
        // removal of nodes within `compress` can fail. See above.
        let mut kill_list = vec![];
        for (predicate, index) in &mut self.waiting_cache {
            let new_i = node_rewrites[index.index()];
            if new_i >= nodes_len {
                kill_list.push(predicate.clone());
            } else {
                *index = NodeIndex::new(new_i);
            }
        }

        for predicate in kill_list { self.waiting_cache.remove(&predicate); }
    }
}

impl<O> Node<O> {
    fn new(
        parent: Option<NodeIndex>,
        obligation: O,
        obligation_tree_id: ObligationTreeId
    ) -> Node<O> {
        Node {
            obligation,
            state: Cell::new(NodeState::Pending),
            parent,
            dependents: vec![],
            obligation_tree_id,
        }
    }
}

// I need a Clone closure
#[derive(Clone)]
struct GetObligation<'a, O>(&'a [Node<O>]);

impl<'a, 'b, O> FnOnce<(&'b usize,)> for GetObligation<'a, O> {
    type Output = &'a O;
    extern "rust-call" fn call_once(self, args: (&'b usize,)) -> &'a O {
        &self.0[*args.0].obligation
    }
}

impl<'a, 'b, O> FnMut<(&'b usize,)> for GetObligation<'a, O> {
    extern "rust-call" fn call_mut(&mut self, args: (&'b usize,)) -> &'a O {
        &self.0[*args.0].obligation
    }
}
