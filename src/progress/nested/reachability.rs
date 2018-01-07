//! Manages pointstamp reachability within a graph.
//!
//! Timely dataflow is concerned with understanding and communicating the potential
//! for capabilites to reach nodes in a directed graph, by following paths through
//! the graph (along edges and through nodes). This module contains one abstraction
//! for managing this information.
//!
//! #Examples
//!
//! ```rust
//! use timely::progress::frontier::Antichain;
//! use timely::progress::nested::subgraph::{Source, Target};
//! use timely::progress::nested::reachability::{Builder, Tracker};
//!
//! // allocate a new empty topology builder.
//! let mut builder = Builder::<usize>::new();
//! 
//! // Each node with one input connected to one output.
//! builder.add_node(0, 1, 1, vec![vec![Antichain::from_elem(0)]]);
//! builder.add_node(1, 1, 1, vec![vec![Antichain::from_elem(0)]]);
//! builder.add_node(2, 1, 1, vec![vec![Antichain::from_elem(1)]]);
//!
//! // Connect nodes in sequence, looping around to the first from the last.
//! builder.add_edge(Source { index: 0, port: 0}, Target { index: 1, port: 0} );
//! builder.add_edge(Source { index: 1, port: 0}, Target { index: 2, port: 0} );
//! builder.add_edge(Source { index: 2, port: 0}, Target { index: 0, port: 0} );
//!
//! // Construct a reachability tracker.
//! let mut tracker = Tracker::allocate_from(builder.summarize());
//!
//! // Introduce a pointstamp at the output of the first node.
//! tracker.update_source(Source { index: 0, port: 0}, 17, 1);
//!
//! // Propagate changes; until this call updates are simply buffered.
//! tracker.propagate();
//!
//! // Propagated changes should have a single element, incremented for node zero.
//! assert_eq!(tracker.pushed_mut(0)[0].drain().collect::<Vec<_>>(), vec![(18, 1)]);
//! assert_eq!(tracker.pushed_mut(1)[0].drain().collect::<Vec<_>>(), vec![(17, 1)]);
//! assert_eq!(tracker.pushed_mut(2)[0].drain().collect::<Vec<_>>(), vec![(17, 1)]);
//! ```

use progress::Timestamp;
use progress::nested::{Source, Target};
use progress::ChangeBatch;

use progress::frontier::Antichain;
use progress::timestamp::PathSummary;
use order::PartialOrder;


/// A topology builder, which can summarize reachability along paths.
///
/// A `Builder` takes descriptions of the nodes and edges in a graph, and compiles
/// a static summary of the minimal actions a timestamp must endure going from any
/// input or output port to a destination input port.
///
/// A graph is provides as (i) several indexed nodes, each with some number of input
/// and output ports, and each with a summary of the internal paths connecting each
/// input to each output, and (ii) a set of edges connecting output ports to input 
/// ports. Edges do not adjust timestamps; only nodes do this.
///
/// The resulting summary describes, for each origin port in the graph and destination
/// input port, a set of incomparable path summaries, each describing what happens to
/// a timestamp as it moves along the path. There may be multiple summaries for each 
/// part of origin and destination due to the fact that the actions on timestamps may
/// not be totally ordered (e.g., "increment the timestamp" and "take the maximum of
/// the timestamp and seven").
///
/// #Examples
///
/// ```rust
/// use timely::progress::frontier::Antichain;
/// use timely::progress::nested::subgraph::{Source, Target};
/// use timely::progress::nested::reachability::Builder;
///
/// // allocate a new empty topology builder.
/// let mut builder = Builder::<usize>::new();
/// 
/// // Each node with one input connected to one output.
/// builder.add_node(0, 1, 1, vec![vec![Antichain::from_elem(0)]]);
/// builder.add_node(1, 1, 1, vec![vec![Antichain::from_elem(0)]]);
/// builder.add_node(2, 1, 1, vec![vec![Antichain::from_elem(1)]]);
///
/// // Connect nodes in sequence, looping around to the first from the last.
/// builder.add_edge(Source { index: 0, port: 0}, Target { index: 1, port: 0} );
/// builder.add_edge(Source { index: 1, port: 0}, Target { index: 2, port: 0} );
/// builder.add_edge(Source { index: 2, port: 0}, Target { index: 0, port: 0} );
///
/// // Summarize reachability information.
/// let summary = builder.summarize();

#[derive(Clone, Debug)]
pub struct Builder<T: Timestamp> {
    /// Internal connections within hosted operators.
    ///
    /// Indexed by operator index, then input port, then output port. This is the
    /// same format returned by `get_internal_summary`, as if we simply appended
    /// all of the summaries for the hosted nodes.
    nodes: Vec<Vec<Vec<Antichain<T::Summary>>>>,
    /// Direct connections from sources to targets. 
    ///
    /// Edges do not affect timestamps, so we only need to know the connectivity.
    /// Indexed by operator index then output port.
    edges: Vec<Vec<Vec<Target>>>,
    /// Numbers of inputs and outputs for each node.
    shape: Vec<(usize, usize)>,
}

impl<T: Timestamp> Builder<T> {

    /// Create a new empty topology builder.
    pub fn new() -> Self {
        Builder {
            nodes: Vec::new(),
            edges: Vec::new(),
            shape: Vec::new(),
        }
    }

    /// Add links internal to operators.
    ///
    /// This method overwrites any existing summary, instead of anything more sophisticated.
    pub fn add_node(&mut self, index: usize, inputs: usize, outputs: usize, summary: Vec<Vec<Antichain<T::Summary>>>) {
        
        // Assert that all summaries exist.
        debug_assert_eq!(inputs, summary.len());
        for x in summary.iter() { debug_assert_eq!(outputs, x.len()); }

        while self.nodes.len() <= index { 
            self.nodes.push(Vec::new()); 
            self.shape.push((0, 0));
        }

        self.nodes[index] = summary;
        self.shape[index] = (inputs, outputs);
    }

    /// Add links between operators.
    ///
    /// This method does not check that the associated nodes and ports exist. References to
    /// missing nodes or ports are discovered in `build`.
    pub fn add_edge(&mut self, source: Source, target: Target) {

        // Assert that the edge is between existing ports.
        debug_assert!(source.port < self.shape[source.index].1);
        debug_assert!(target.port < self.shape[target.index].0);

        while self.edges.len() <= source.index { self.edges.push(Vec::new()); }
        while self.edges[source.index].len() <= source.port { self.edges[source.index].push(Vec::new()); }
        self.edges[source.index][source.port].push(target);
    }

    /// Compiles the current nodes and edges into immutable path summaries.
    ///
    /// This method has the opportunity to perform some error checking that the path summaries
    /// are valid, including references to undefined nodes and ports, as well as self-loops with
    /// default summaries (a serious liveness issue).
    pub fn summarize(&mut self) -> Summary<T> {

        // We maintain a list of new ((source, target), path_summary) entries whose implications 
        // have not yet been fully explored. While such entries exist, we consider the next and 
        // explore its implications by considering all incident target-source' connections (from
        // `self.nodes`) followed by all source'-target' connections (from `self.edges`). This may
        // yield ((source, target'), path_summary) entries, and we enqueue any new ones in our list.
        let mut work = ::std::collections::VecDeque::<((Source, Target), T::Summary)>::new();

        // Initialize `work` with all edges in the graph, each with a `Default::default()` summary.
        for index in 0 .. self.edges.len() {
            for port in 0 .. self.edges[index].len() {
                for &target in &self.edges[index][port] {
                    work.push_back(((Source { index: index, port: port}, target), Default::default()));
                }
            }
        }

        // Prepare space for path summaries.
        let mut source_target: Vec<Vec<Vec<(Target, Antichain<T::Summary>)>>> = Vec::new();
        let mut target_target: Vec<Vec<Vec<(Target, Antichain<T::Summary>)>>> = Vec::new();

        for &(inputs, outputs) in self.shape.iter() {
            source_target.push(vec![Vec::new(); outputs]);
            target_target.push(vec![Vec::new(); inputs]);
        }

        // Establish all source-target path summaries by fixed-point computation.
        while let Some(((source, target), summary)) = work.pop_front() {
            // try to add the summary, and if it comes back as "novel" we should explore its two-hop connections.
            if add_summary(&mut source_target[source.index][source.port], target, summary.clone()) {
                for (new_source_port, internal_summaries) in self.nodes[target.index][target.port].iter().enumerate() {
                    for internal_summary in internal_summaries.elements() {
                        if let Some(new_summary) = summary.followed_by(internal_summary) {
                            for &new_target in self.edges[target.index][new_source_port].iter() {
                                work.push_back(((source, new_target), new_summary.clone()));
                            }
                        }
                    }
                }
            }
        }

        // Extend source-target path summaries by one target'-source connection, to yield all 
        // target'-target path summaries. This computes summaries along non-empty paths, so that
        // we can test for trivial summaries along non-trivial paths.
        for index in 0 .. self.nodes.len() {
            for input_port in 0 .. self.nodes[index].len() {
                // for each output port, consider source-target summaries.
                for (output_port, internal_summaries) in self.nodes[index][input_port].iter().enumerate() {
                    for internal_summary in internal_summaries.elements() {
                        for &(target, ref new_summaries) in source_target[index][output_port].iter() {
                            for new_summary in new_summaries.elements() {
                                if let Some(summary) = internal_summary.followed_by(new_summary) {
                                    add_summary(&mut target_target[index][input_port], target, summary);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Test for trivial summaries along self-loops.
        #[cfg(debug_assertions)]
        {
            for node in 0 .. target_target.len() {
                for port in 0 .. target_target[node].len() {
                    let this_target = Target { index: node, port: port };
                    for &(ref target, ref summary) in target_target[node][port].iter() {
                        if target == &this_target && summary.less_equal(&Default::default()) {
                            panic!("Default summary found along self-loop: {:?}", target);
                        }
                    }
                }
            }
        }

        // Incorporate trivial self-loops, as changes at a target do apply to the target.
        for index in 0 .. self.nodes.len() {
            for input_port in 0 .. self.nodes[index].len() {
                add_summary(
                    &mut target_target[index][input_port], 
                    Target { index: index, port: input_port }, 
                    Default::default(),
                );
            }
        }

        Summary {
            source_target,
            target_target,
        }
    }
}

/// A summary of minimal path summaries in a timely dataflow graph.
///
/// A `Summary` instance records a compiled representation of path summaries along paths
/// in a timely dataflow graph, mostly commonly constructed by a `reachability::Builder`.
pub struct Summary<T: Timestamp> {
    /// Compiled source-to-target reachability.
    pub source_target: Vec<Vec<Vec<(Target, Antichain<T::Summary>)>>>,
    /// Compiled target-to-target reachability.
    pub target_target: Vec<Vec<Vec<(Target, Antichain<T::Summary>)>>>,
}

/// Interactive tracking of propagated reachability information.
///
/// A `Tracker` tracks, for a fixed graph topology, the consequences of
/// pointstamp changes at various node input and output ports. These changes may
/// alter the potential pointstamps that could arrive at downstream input ports.
///
/// A `Tracker` instance is constructed from a reachability summary, by
/// way of its `allocate_from` method. With a fixed topology, users can interactively
/// call `update_target` and `update_source` to change observed pointstamp counts
/// at node inputs and outputs, respectively. These changes are buffered until a
/// user invokes either `propagate` or `propagate_node`, which consume buffered 
/// changes propagate their consequences along the graph to any other port that 
/// can be reached. These changes can be read for each node using `pushed_mut`.
///
/// #Examples
///
/// ```rust
/// use timely::progress::frontier::Antichain;
/// use timely::progress::nested::subgraph::{Source, Target};
/// use timely::progress::nested::reachability::{Builder, Tracker};
///
/// // allocate a new empty topology builder.
/// let mut builder = Builder::<usize>::new();
/// 
/// // Each node with one input connected to one output.
/// builder.add_node(0, 1, 1, vec![vec![Antichain::from_elem(0)]]);
/// builder.add_node(1, 1, 1, vec![vec![Antichain::from_elem(0)]]);
/// builder.add_node(2, 1, 1, vec![vec![Antichain::from_elem(1)]]);
///
/// // Connect nodes in sequence, looping around to the first from the last.
/// builder.add_edge(Source { index: 0, port: 0}, Target { index: 1, port: 0} );
/// builder.add_edge(Source { index: 1, port: 0}, Target { index: 2, port: 0} );
/// builder.add_edge(Source { index: 2, port: 0}, Target { index: 0, port: 0} );
///
/// // Construct a reachability tracker.
/// let mut tracker = Tracker::allocate_from(builder.summarize());
///
/// // Introduce a pointstamp at the output of the first node.
/// tracker.update_source(Source { index: 0, port: 0}, 17, 1);
///
/// // Propagate changes; until this call updates are simply buffered.
/// tracker.propagate();
///
/// // Propagated changes should have a single element, incremented for node zero.
/// assert_eq!(tracker.pushed_mut(0)[0].drain().collect::<Vec<_>>(), vec![(18, 1)]);
/// assert_eq!(tracker.pushed_mut(1)[0].drain().collect::<Vec<_>>(), vec![(17, 1)]);
/// assert_eq!(tracker.pushed_mut(2)[0].drain().collect::<Vec<_>>(), vec![(17, 1)]);
/// ```

#[derive(Default)]
pub struct Tracker<T:Timestamp> {
    /// Buffers of observed changes.
    source:  Vec<Vec<ChangeBatch<T>>>,
    target:  Vec<Vec<ChangeBatch<T>>>,
    /// Buffers of consequent propagated changes.
    pushed:  Vec<Vec<ChangeBatch<T>>>,
    /// Compiled reachability along edges and through internal connections.
    source_target: Vec<Vec<Vec<(Target, Antichain<T::Summary>)>>>,
    target_target: Vec<Vec<Vec<(Target, Antichain<T::Summary>)>>>,
}

impl<T:Timestamp> Tracker<T> {

    /// Updates the count for a time at a target.
    pub fn update_target(&mut self, target: Target, time: T, value: i64) {
        self.target[target.index][target.port].update(time, value);
    }
    /// Updates the count for a time at a source.
    pub fn update_source(&mut self, source: Source, time: T, value: i64) {
        self.source[source.index][source.port].update(time, value);
    }

    /// Clears the pointstamp counter.
    pub fn clear(&mut self) {
        for vec in &mut self.source { for map in vec.iter_mut() { map.clear(); } }
        for vec in &mut self.target { for map in vec.iter_mut() { map.clear(); } }
        for vec in &mut self.pushed { for map in vec.iter_mut() { map.clear(); } }
    }

    /// Allocate a new `Tracker` using the shape from `summaries`.
    pub fn allocate_from(summary: Summary<T>) -> Self {

        let source_target = summary.source_target;
        let target_target = summary.target_target;

        let mut sources = Vec::with_capacity(source_target.len());
        let mut targets = Vec::with_capacity(target_target.len());
        let mut pushed = Vec::with_capacity(source_target.len());

        // Allocate buffer space for each input and input port.
        for source in 0 .. source_target.len() {
            sources.push(vec![ChangeBatch::new(); source_target[source].len()]);
            pushed.push(vec![ChangeBatch::new(); source_target[source].len()]);
        }

        // Allocate buffer space for each output and output port.
        for target in 0 .. target_target.len() {
            targets.push(vec![ChangeBatch::new(); target_target[target].len()]);
        }

        Tracker {
            source: sources,
            target: targets,
            pushed,
            source_target,
            target_target,
        }
    }

    /// Propagates updates from an indicated node.
    ///
    /// This method is potentially useful for propagating the consequences of a single
    /// node invocation, to make the results available immediately.
    pub fn propagate_node(&mut self, index: usize) {

        // Propagate changes at each input (target).
        for input in 0..self.target[index].len() {
            for (time, value) in self.target[index][input].drain() {
                for &(target, ref antichain) in &self.target_target[index][input] {
                    for summary in antichain.elements().iter() {
                        if let Some(new_time) = summary.results_in(&time) {
                            self.pushed[target.index][target.port].update(new_time, value);
                        }
                    }
                }
            }
        }

        // Propagate changes at each output (source).
        for output in 0..self.source[index].len() {
            for (time, value) in self.source[index][output].drain() {
                for &(target, ref antichain) in &self.source_target[index][output] {
                    for summary in antichain.elements().iter() {
                        if let Some(new_time) = summary.results_in(&time) {
                            self.pushed[target.index][target.port].update(new_time, value);
                        }
                    }
                }
            }
        }
    }

    /// Propagates all updates made to sources and targets.
    pub fn propagate(&mut self) {
        debug_assert_eq!(self.source.len(), self.target.len());
        for index in 0..self.target.len() {
            self.propagate_node(index);
        }
    }

    /// Provides access to pushed changes for a node.
    ///
    /// The caller may read the results or consume the results, as appropriate. The method
    /// itself does not clear the buffer, so pushed values will stay in place until they are
    /// consumed by some caller.
    pub fn pushed_mut(&mut self, node: usize) -> &mut [ChangeBatch<T>] {
        &mut self.pushed[node][..]
    }
}


fn add_summary<S: PartialOrder+Eq>(vector: &mut Vec<(Target, Antichain<S>)>, target: Target, summary: S) -> bool {
    for &mut (ref t, ref mut antichain) in vector.iter_mut() {
        // TODO : Do we need to clone here, or should `insert` be smarter?
        if target.eq(t) { return antichain.insert(summary); }
    }
    vector.push((target, Antichain::from_elem(summary)));
    true
}