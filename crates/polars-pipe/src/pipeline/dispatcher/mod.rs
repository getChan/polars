use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use polars_core::error::PolarsResult;
use polars_core::frame::DataFrame;
use polars_core::utils::accumulate_dataframes_vertical_unchecked;
use polars_core::POOL;
use polars_utils::arena::Node;
use polars_utils::sync::SyncPtr;
use rayon::prelude::*;

use crate::executors::sources::DataFrameSource;
use crate::operators::{
    DataChunk, FinalizedSink, Operator, OperatorResult, PExecutionContext, SExecutionContext, Sink,
    SinkResult, Source, SourceResult,
};
use crate::pipeline::dispatcher::drive_operator::{par_flush, par_process_chunks};
use crate::pipeline::morsels_per_sink;
mod drive_operator;

type PhysOperator = Box<dyn Operator>;
type PhysSink = Box<dyn Sink>;
/// A physical operator/sink per thread.
type ThreadedOperator = Vec<PhysOperator>;
type ThreadedOperatorMut<'a> = &'a mut [PhysOperator];
type ThreadedSinkMut<'a> = &'a mut [PhysSink];

pub(super) struct SinkNode {
    pub sinks: Vec<Box<dyn Sink>>,
    /// when that hits 0, the sink will finalize
    pub shared_count: Rc<RefCell<u32>>,
    initial_shared_count: u32,
    /// - offset in the operators vec
    ///   at that point the sink should be called.
    ///   the pipeline will first call the operators on that point and then
    ///   push the result in the sink.
    pub operator_end: usize,
    pub node: Node,
}

impl SinkNode {
    pub fn new(
        sink: Box<dyn Sink>,
        shared_count: Rc<RefCell<u32>>,
        operator_end: usize,
        node: Node,
    ) -> Self {
        let n_threads = morsels_per_sink();
        let sinks = (0..n_threads).map(|i| sink.split(i)).collect();
        let initial_shared_count = *shared_count.borrow();
        SinkNode {
            sinks,
            initial_shared_count,
            shared_count,
            operator_end,
            node,
        }
    }

    // Only the first node of a shared sink should recurse. The others should return.
    fn allow_recursion(&self) -> bool {
        self.initial_shared_count == *self.shared_count.borrow()
    }
}

/// A pipeline consists of:
///
/// - 1. One or more sources.
///         Sources get pulled and their data is pushed into operators.
/// - 2. Zero or more operators.
///         The operators simply pass through data, modifying it as they need.
///         Operators can work on batches and don't need all data in scope to
///         succeed.
///         Think for example on multiply a few columns, or applying a predicate.
///         Operators can shrink the batches: filter
///         Grow the batches: explode/ melt
///         Keep them the same size: element-wise operations
///         The probe side of join operations is also an operator.
///
///
/// - 3. One or more sinks
///         A sink needs all data in scope to finalize a pipeline branch.
///         Think of sorts, preparing a build phase of a join, group_by + aggregations.
///
/// This struct will have the SOS (source, operators, sinks) of its own pipeline branch, but also
/// the SOS of other branches. The SOS are stored data oriented and the sinks have an offset that
/// indicates the last operator node before that specific sink. We only store the `end offset` and
/// keep track of the starting operator during execution.
///
/// Pipelines branches are shared with other pipeline branches at the join/union nodes.
/// # JOIN
/// Consider this tree:
///         out
///       /
///     /\
///    1  2
///
/// And let's consider that branch 2 runs first. It will run until the join node where it will sink
/// into a build table. Once that is done it will replace the build-phase placeholder operator in
/// branch 1. Branch one can then run completely until out.
pub struct PipeLine {
    /// All the sources of this pipeline
    sources: Vec<Box<dyn Source>>,
    /// All the operators of this pipeline. Some may be placeholders that will be replaced during
    /// execution
    operators: Vec<ThreadedOperator>,
    /// The nodes of operators. These are used to identify operators between pipelines
    operator_nodes: Vec<Node>,
    /// - offset in the operators vec
    ///   at that point the sink should be called.
    ///   the pipeline will first call the operators on that point and then
    ///   push the result in the sink.
    /// - shared_count
    ///     when that hits 0, the sink will finalize
    /// - node of the sink
    sinks: Vec<SinkNode>,
    /// are used to identify the sink shared with other pipeline branches
    sink_nodes: Vec<Node>,
    /// Other branch of the pipeline/tree that must be executed
    /// after this one has executed.
    /// the dispatcher takes care of this.
    other_branches: Rc<RefCell<VecDeque<PipeLine>>>,
    /// this is a correction as there may be more `operators` than nodes
    /// as during construction, source may have inserted operators
    operator_offset: usize,
    /// Log runtime info to stderr
    verbose: bool,
}

impl PipeLine {
    #[allow(clippy::type_complexity)]
    pub(super) fn new(
        sources: Vec<Box<dyn Source>>,
        operators: Vec<PhysOperator>,
        operator_nodes: Vec<Node>,
        sinks: Vec<SinkNode>,
        operator_offset: usize,
        verbose: bool,
    ) -> PipeLine {
        debug_assert_eq!(operators.len(), operator_nodes.len() + operator_offset);
        // we don't use the power of two partition size here
        // we only do that in the sinks itself.
        let n_threads = morsels_per_sink();

        let sink_nodes = sinks.iter().map(|s| s.node).collect();
        // We split so that every thread gets an operator
        // every index maps to a chain of operators than can be pushed as a pipeline for one thread
        let operators = (0..n_threads)
            .map(|i| operators.iter().map(|op| op.split(i)).collect())
            .collect();

        PipeLine {
            sources,
            operators,
            operator_nodes,
            sinks,
            sink_nodes,
            other_branches: Default::default(),
            operator_offset,
            verbose,
        }
    }

    /// Create a pipeline only consisting of a single branch that always finishes with a sink
    pub fn new_simple(
        sources: Vec<Box<dyn Source>>,
        operators: Vec<Box<dyn Operator>>,
        sink: Box<dyn Sink>,
        verbose: bool,
    ) -> Self {
        let operators_len = operators.len();
        Self::new(
            sources,
            operators,
            vec![],
            vec![SinkNode::new(
                sink,
                Rc::new(RefCell::new(1)),
                operators_len,
                Node::default(),
            )],
            0,
            verbose,
        )
    }

    /// Add a parent
    /// This should be in the right order
    pub fn with_other_branch(self, rhs: PipeLine) -> Self {
        self.other_branches.borrow_mut().push_back(rhs);
        self
    }

    // returns if operator was successfully replaced
    fn replace_operator(&mut self, op: &dyn Operator, node: Node) -> bool {
        if let Some(pos) = self.operator_nodes.iter().position(|n| *n == node) {
            let pos = pos + self.operator_offset;
            for (i, operator_pipe) in &mut self.operators.iter_mut().enumerate() {
                operator_pipe[pos] = op.split(i)
            }
            true
        } else {
            false
        }
    }

    /// Replace the current sources with a [`DataFrameSource`].
    fn set_df_as_sources(&mut self, df: DataFrame) {
        let src = Box::new(DataFrameSource::from_df(df)) as Box<dyn Source>;
        self.set_sources(src)
    }

    /// Replace the current sources.
    fn set_sources(&mut self, src: Box<dyn Source>) {
        self.sources.clear();
        self.sources.push(src);
    }

    fn run_pipeline_no_finalize(
        &mut self,
        ec: &PExecutionContext,
        pipeline_q: Rc<RefCell<VecDeque<PipeLine>>>,
    ) -> PolarsResult<(u32, Box<dyn Sink>)> {
        let mut out = None;
        let mut operator_start = 0;
        let last_i = self.sinks.len() - 1;

        // for unions we typically first want to push all pipelines
        // into the union sink before we call `finalize`
        // however if the sink is finished early, (for instance a `head`)
        // we don't want to run the rest of the pipelines and we finalize early
        let mut sink_finished = false;

        for (i, mut sink) in std::mem::take(&mut self.sinks).into_iter().enumerate() {
            for src in &mut std::mem::take(&mut self.sources) {
                let mut next_batches = src.get_batches(ec)?;

                let must_flush: AtomicBool = AtomicBool::new(false);
                while let SourceResult::GotMoreData(chunks) = next_batches {
                    // Every batches iteration we check if we must continue.
                    ec.execution_state.should_stop()?;

                    let (sink_result, next_batches2) = par_process_chunks(
                        chunks,
                        &mut sink.sinks,
                        ec,
                        &mut self.operators,
                        operator_start,
                        sink.operator_end,
                        src,
                        &must_flush,
                    )?;
                    next_batches = next_batches2;

                    if let Some(SinkResult::Finished) = sink_result {
                        sink_finished = true;
                        break;
                    }
                }
                if !sink_finished && must_flush.load(Ordering::Relaxed) {
                    par_flush(
                        &mut sink.sinks,
                        ec,
                        &mut self.operators,
                        operator_start,
                        sink.operator_end,
                    );
                }
            }

            // Before we reduce we also check if we should continue.
            ec.execution_state.should_stop()?;
            let allow_recursion = sink.allow_recursion();

            // The sinks have taken all chunks thread locally, now we reduce them into a single
            // result sink.
            let mut reduced_sink = POOL
                .install(|| {
                    sink.sinks.into_par_iter().reduce_with(|mut a, mut b| {
                        a.combine(&mut *b);
                        a
                    })
                })
                .unwrap();
            operator_start = sink.operator_end;

            let mut shared_sink_count = {
                let mut shared_sink_count = sink.shared_count.borrow_mut();
                *shared_sink_count -= 1;
                *shared_sink_count
            };

            // Prevent very deep recursion. Only the outer callee can pop and run.
            if allow_recursion {
                while shared_sink_count > 0 && !sink_finished {
                    let mut pipeline = pipeline_q.borrow_mut().pop_front().unwrap();
                    let (count, mut sink) =
                        pipeline.run_pipeline_no_finalize(ec, pipeline_q.clone())?;
                    // This branch is hit when we have a Union of joins.
                    // The build side must be converted into an operator and replaced in the next pipeline.

                    // Check either:
                    // 1. There can be a union source that sinks into a single join:
                    //      scan_parquet(*) -> join B
                    // 2. There can be a union of joins
                    //      C - JOIN A, B
                    //      concat (A, B, C)
                    //
                    // So to ensure that we don't finalize we check
                    // - They are not both join builds
                    // - If they are both join builds, check they are note the same build, otherwise
                    //   we must call the `combine` branch.
                    if sink.is_join_build()
                        && (!reduced_sink.is_join_build() || (sink.node() != reduced_sink.node()))
                    {
                        let FinalizedSink::Operator(op) = sink.finalize(ec)? else {
                            unreachable!()
                        };
                        let mut q = pipeline_q.borrow_mut();
                        let Some(node) = pipeline.sink_nodes.pop() else {
                            unreachable!()
                        };

                        for probe_side in q.iter_mut() {
                            let _ = probe_side.replace_operator(op.as_ref(), node);
                        }
                    } else {
                        reduced_sink.combine(sink.as_mut());
                        shared_sink_count = count;
                    }
                }
            }

            if i != last_i {
                let sink_result = reduced_sink.finalize(ec)?;
                match sink_result {
                    // turn this sink an a new source
                    FinalizedSink::Finished(df) => self.set_df_as_sources(df),
                    FinalizedSink::Source(src) => self.set_sources(src),
                    // should not happen
                    FinalizedSink::Operator(_) => {
                        unreachable!()
                    },
                }
            } else {
                out = Some((shared_sink_count, reduced_sink))
            }
        }
        Ok(out.unwrap())
    }

    /// Run a single pipeline branch.
    /// This pulls data from the sources and pushes it into the operators which run on a different
    /// thread and finalize in a sink.
    ///
    /// The sink can be finished, but can also become a new source and then rinse and repeat.
    pub fn run_pipeline(
        &mut self,
        ec: &PExecutionContext,
        pipeline_q: Rc<RefCell<VecDeque<PipeLine>>>,
    ) -> PolarsResult<Option<FinalizedSink>> {
        let (sink_shared_count, mut reduced_sink) =
            self.run_pipeline_no_finalize(ec, pipeline_q)?;
        assert_eq!(sink_shared_count, 0);
        Ok(reduced_sink.finalize(ec).ok())
    }

    /// Executes all branches and replaces operators and sinks during execution to ensure
    /// we materialize.
    pub fn execute(&mut self, state: Box<dyn SExecutionContext>) -> PolarsResult<DataFrame> {
        let ec = PExecutionContext::new(state, self.verbose);

        if self.verbose {
            eprintln!("{self:?}");
            eprintln!("{:?}", &self.other_branches);
        }
        let mut sink_out = self.run_pipeline(&ec, self.other_branches.clone())?;
        let mut sink_nodes = std::mem::take(&mut self.sink_nodes);
        loop {
            match &mut sink_out {
                None => {
                    let mut pipeline = self.other_branches.borrow_mut().pop_front().unwrap();
                    sink_out = pipeline.run_pipeline(&ec, self.other_branches.clone())?;
                    sink_nodes = std::mem::take(&mut pipeline.sink_nodes);
                },
                Some(FinalizedSink::Finished(df)) => return Ok(std::mem::take(df)),
                Some(FinalizedSink::Source(src)) => return consume_source(&mut **src, &ec),

                //
                //  1/\
                //   2/\
                //     3\
                // the left hand side of the join has finished and now is an operator
                // we replace the dummy node in the right hand side pipeline with this
                // operator and then we run the pipeline rinse and repeat
                // until the final right hand side pipeline ran
                Some(FinalizedSink::Operator(op)) => {
                    // we unwrap, because the latest pipeline should not return an Operator
                    let mut pipeline = self.other_branches.borrow_mut().pop_front().unwrap();

                    // latest sink_node will be the operator, as the left side of the join
                    // always finishes that branch.
                    if let Some(sink_node) = sink_nodes.pop() {
                        // we traverse all pipeline
                        pipeline.replace_operator(op.as_ref(), sink_node);
                        // if there are unions, there can be more
                        for pl in self.other_branches.borrow_mut().iter_mut() {
                            pl.replace_operator(op.as_ref(), sink_node);
                        }
                    }
                    sink_out = pipeline.run_pipeline(&ec, self.other_branches.clone())?;
                    sink_nodes = std::mem::take(&mut pipeline.sink_nodes);
                },
            }
        }
    }
}

impl Debug for PipeLine {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut fmt = String::new();
        let mut start = 0usize;
        fmt.push_str(self.sources[0].fmt());
        for sink in &self.sinks {
            fmt.push_str(" -> ");
            // take operators of a single thread
            let ops = &self.operators[0];
            // slice the pipeline
            let ops = &ops[start..sink.operator_end];
            for op in ops {
                fmt.push_str(op.fmt());
                fmt.push_str(" -> ")
            }
            start = sink.operator_end;
            fmt.push_str(sink.sinks[0].fmt())
        }
        write!(f, "{fmt}")
    }
}

/// Take a source and materialize it into a [`DataFrame`].
fn consume_source(src: &mut dyn Source, context: &PExecutionContext) -> PolarsResult<DataFrame> {
    let mut frames = Vec::with_capacity(32);

    while let SourceResult::GotMoreData(batch) = src.get_batches(context)? {
        frames.extend(batch.into_iter().map(|chunk| chunk.data))
    }
    Ok(accumulate_dataframes_vertical_unchecked(frames))
}

unsafe impl Send for PipeLine {}
unsafe impl Sync for PipeLine {}
