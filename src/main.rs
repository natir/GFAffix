/* standard use */
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::io;
use std::io::prelude::*;

/* crate use */
use clap::Clap;
use gfa::{gfa::GFA, parser::GFAParser};
use handlegraph::{
    handle::{Direction, Edge, Handle},
    handlegraph::*,
    hashgraph::HashGraph,
    mutablehandlegraph::{AdditiveHandleGraph, MutableHandles},
};

#[derive(clap::Clap, Debug)]
#[clap(
    version = "0.1",
    author = "Daniel Doerr <daniel.doerr@hhu.de>",
    about = "Discover path-preserving shared prefixes in multifurcations of a given graph.\n
    - Do you want log output? Call program with 'RUST_LOG=info gfaffix ...'
    - Log output not informative enough? Try 'RUST_LOG=debug gfaffix ...'"
)]
pub struct Command {
    #[clap(index = 1, about = "graph in GFA1 format", required = true)]
    pub graph: String,

    #[clap(
        short = 'o',
        long = "output_refined",
        about = "write refined graph in GFA1 format to supplied file",
        default_value = " "
    )]
    pub refined_graph_out: String,
}

// structure for storing reported subgraph
pub struct AffixSubgraph {
    pub sequence: String,
    pub parents: Vec<Handle>,
    pub shared_prefix_nodes: Vec<Handle>,
}

#[derive(Clone, Debug)]
pub struct DeletedSubGraph {
    pub nodes: FxHashSet<Handle>,
}

impl DeletedSubGraph {
    fn add(&mut self, v: Handle) -> bool {
        self.nodes.insert(v) | self.nodes.insert(v.flip())
    }

    fn edge_deleted(&self, u: &Handle, v: &Handle) -> bool {
        self.nodes.contains(u) || self.nodes.contains(v)
    }

    fn node_deleted(&self, v: &Handle) -> bool {
        self.nodes.contains(v)
    }

    fn new() -> Self {
        DeletedSubGraph {
            nodes: FxHashSet::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CollapseEventTracker {
    pub transform: FxHashMap<Handle, Vec<Handle>>,
    pub overlapping_events: usize,
    pub bubbles: usize,
    pub events: usize,
}

impl CollapseEventTracker {
    fn report(
        &mut self,
        collapsed_prefix_node: Handle,
        shared_prefix_nodes: &Vec<Handle>,
        splitted_node_pairs: &Vec<(Handle, Option<Handle>)>,
    ) {
        self.events += 1;
        let is_bubble = splitted_node_pairs.iter().all(|(_, x)| x.is_none());
        if is_bubble {
            self.bubbles += 1;
        }
        for i in 0..shared_prefix_nodes.len() {
            let v = shared_prefix_nodes[i];
            if self.transform.contains_key(&v)
                || (is_bubble && self.transform.contains_key(&v.flip()))
            {
                self.overlapping_events += 1
            }

            // record transformation of node, even if none took place (which is the case if node v
            // equals the dedicated shared prefix node
            let mut replacement: Vec<Handle> = Vec::new();
            replacement.push(collapsed_prefix_node);
            if let Some(u) = splitted_node_pairs[i].1 {
                replacement.push(u)
            }
            self.transform.insert(v, replacement.clone());
            if is_bubble {
                // if shared prefix is a bubble than also record the reverse complementary
                // transformation
                self.transform
                    .insert(v.flip(), replacement.iter().map(|u| u.flip()).collect());
            }
        }
    }

    fn new() -> Self {
        CollapseEventTracker {
            transform: FxHashMap::default(),
            overlapping_events: 0,
            bubbles: 0,
            events: 0,
        }
    }
}

fn enumerate_variant_preserving_shared_affixes(
    graph: &HashGraph,
    del_subg: &DeletedSubGraph,
    v: Handle,
) -> Result<Vec<AffixSubgraph>, Box<dyn Error>> {
    let mut res: Vec<AffixSubgraph> = Vec::new();

    let mut branch: FxHashMap<(u8, Vec<Handle>), Vec<Handle>> = FxHashMap::default();
    // traverse multifurcation in the forward direction of the handle
    for u in graph.neighbors(v, Direction::Right) {
        if !del_subg.node_deleted(&u) {
            let seq = graph.sequence_vec(u);
            // get parents of u
            let mut parents: Vec<Handle> = graph
                .neighbors(u, Direction::Left)
                .filter(|w| !del_subg.node_deleted(&w))
                .collect();
            parents.sort();
            // insert child in variant-preserving data structure
            branch
                .entry((seq[0], parents))
                .or_insert(Vec::new())
                .push(u);
        }
    }

    for ((_, parents), children) in branch.iter() {
        if children.len() > 1 {
            log::debug!(
                "identified shared prefix between nodes {} originating from parents {}",
                children
                    .iter()
                    .map(|v| format!(
                        "{}{}",
                        if v.is_reverse() { '<' } else { '>' },
                        usize::from(v.id())
                    ))
                    .collect::<Vec<String>>()
                    .join(","),
                parents
                    .iter()
                    .map(|v| format!(
                        "{}{}",
                        if v.is_reverse() { '<' } else { '>' },
                        usize::from(v.id())
                    ))
                    .collect::<Vec<String>>()
                    .join(",")
            );
            res.push(AffixSubgraph {
                sequence: get_shared_prefix(children, graph)?,
                parents: parents.clone(),
                shared_prefix_nodes: children.clone(),
            });
        }
    }

    Ok(res)
}

fn collapse(
    graph: &mut HashGraph,
    shared_prefix: &AffixSubgraph,
    del_subg: &mut DeletedSubGraph,
    event_tracker: &mut CollapseEventTracker,
) -> Handle {
    let prefix_len = shared_prefix.sequence.len();

    // update graph in two passes:
    //  1. split nodes into shared prefix and distinct suffix and appoint dedicated shared
    //  prefix node
    let mut shared_prefix_node_pos: usize = 0;
    let mut splitted_node_pairs: Vec<(Handle, Option<Handle>)> = Vec::new();
    for (i, v) in shared_prefix.shared_prefix_nodes.iter().enumerate() {
        let v_len = graph.sequence_vec(*v).len();
        if v_len > prefix_len {
            // x corresponds to the shared prefix,
            let (x, u) = if v.is_reverse() {
                // apparently, there's a bug in rs-handlegraph that prevents splitting nodes in
                // reverse direction
                let (u_rev, x_rev) = graph.split_handle(v.flip(), v_len - prefix_len);
                (x_rev.flip(), u_rev.flip())
            } else {
                graph.split_handle(*v, prefix_len)
            };
            splitted_node_pairs.push((x, Some(u)));
            // update dedicated shared prefix node if none has been assigned yet
            log::debug!(
                "splitting node {}{} into prefix {}{} and suffix {}{}",
                if v.is_reverse() { '<' } else { '>' },
                usize::from(v.id()),
                if x.is_reverse() { '<' } else { '>' },
                usize::from(x.id()),
                if u.is_reverse() { '<' } else { '>' },
                usize::from(u.id())
            );
        } else {
            // always use a node as dedicated shared prefix node if that node coincides with the
            // prefix
            shared_prefix_node_pos = i;
            splitted_node_pairs.push((*v, None));
            log::debug!(
                "node {}{} matches prefix {}",
                if v.is_reverse() { '<' } else { '>' },
                usize::from(v.id()),
                &shared_prefix.sequence
            );
        }
    }

    //  2. update deleted edge set, reassign outgoing edges of "empty" nodes to dedicated shared
    //     prefix node
    // there will be always a shared prefix node, so this condition is always true
    let shared_prefix_node = shared_prefix.shared_prefix_nodes[shared_prefix_node_pos];
    log::debug!(
        "node {}{} is dedicated shared prefix node",
        if shared_prefix_node.is_reverse() {
            '<'
        } else {
            '>'
        },
        usize::from(shared_prefix_node.id())
    );

    for (u, maybe_v) in splitted_node_pairs.iter() {
        if *u != shared_prefix_node {
            // rewrire outgoing edges
            match maybe_v {
                Some(v) => {
                    // make all suffixes spring from shared suffix node
                    if !graph.has_edge(shared_prefix_node, *v) {
                        graph.create_edge(Edge(shared_prefix_node, *v));
                        log::debug!(
                            "create edge {}{}{}{}",
                            if shared_prefix_node.is_reverse() {
                                '<'
                            } else {
                                '>'
                            },
                            usize::from(shared_prefix_node.id()),
                            if v.is_reverse() { '<' } else { '>' },
                            usize::from(v.id())
                        );
                    }
                }
                None => {
                    // if node coincides with shared prefix (but is not the dedicated shared prefix
                    // node), then all outgoing edges of this node must be transferred to dedicated
                    // node
                    let outgoing_edges: Vec<Handle> = graph
                        .neighbors(*u, Direction::Right)
                        .filter(|v| !del_subg.edge_deleted(&u, v))
                        .collect();
                    for w in outgoing_edges {
                        if !graph.has_edge(shared_prefix_node, w) {
                            graph.create_edge(Edge(shared_prefix_node, w));
                            log::debug!(
                                "create edge {}{}{}{}",
                                if shared_prefix_node.is_reverse() {
                                    '<'
                                } else {
                                    '>'
                                },
                                usize::from(shared_prefix_node.id()),
                                if w.is_reverse() { '<' } else { '>' },
                                usize::from(w.id())
                            );
                        }
                    }
                }
            }
            // mark redundant node as deleted
            log::debug!(
                "flag {}{} as deleted",
                if u.is_reverse() { '<' } else { '>' },
                usize::from(u.id())
            );
            del_subg.add(*u);
        }
    }

    event_tracker.report(
        shared_prefix_node,
        &shared_prefix.shared_prefix_nodes,
        &splitted_node_pairs,
    );

    shared_prefix_node
}

fn get_shared_prefix(
    nodes: &Vec<Handle>,
    graph: &HashGraph,
) -> Result<String, std::string::FromUtf8Error> {
    let mut seq: Vec<u8> = Vec::new();

    let sequences: Vec<Vec<u8>> = nodes.iter().map(|v| graph.sequence_vec(*v)).collect();

    let mut i = 0;
    while sequences[0].len() > i {
        let c: u8 = sequences[0][i];
        if sequences
            .iter()
            .any(|other| other.len() <= i || other[i] != c)
        {
            break;
        }
        seq.push(c);
        i += 1;
    }

    String::from_utf8(seq)
}

fn find_and_report_variant_preserving_shared_affixes<W: Write>(
    graph: &mut HashGraph,
    out: &mut io::BufWriter<W>,
) -> Result<DeletedSubGraph, Box<dyn Error>> {
    let mut del_subg = DeletedSubGraph::new();

    let mut event_tracker = CollapseEventTracker::new();

    let mut has_changed = true;
    while has_changed {
        has_changed = false;
        let mut queue: VecDeque<Handle> = VecDeque::new();
        queue.extend(graph.handles().chain(graph.handles().map(|v| v.flip())));
        while let Some(v) = queue.pop_front() {
            if !del_subg.node_deleted(&v) {
                log::debug!(
                    "processing oriented node {}{}",
                    if v.is_reverse() { '<' } else { '>' },
                    usize::from(v.id())
                );

                // process node in forward direction
                let affixes = enumerate_variant_preserving_shared_affixes(graph, &del_subg, v)?;
                for affix in affixes.iter() {
                    has_changed |= true;
                    // in each iteration, the set of deleted nodes can change and affect the
                    // subsequent iteration, so we need to check the status the node every time
                    if affix
                        .shared_prefix_nodes
                        .iter()
                        .chain(affix.parents.iter())
                        .any(|u| del_subg.node_deleted(u))
                    {
                        // push non-deleted parents back on queue
                        queue.extend(affix.parents.iter().filter(|u| !del_subg.node_deleted(u)));
                    } else {
                        print(affix, out)?;
                        let shared_prefix_node =
                            collapse(graph, affix, &mut del_subg, &mut event_tracker);
                        queue.push_back(shared_prefix_node);
                        queue.push_back(shared_prefix_node.flip());
                    }
                }
            }
        }
    }

    log::info!(
        "identified {} shared prefixes, {} of which are overlapping, and {} of which are bubbles",
        event_tracker.events,
        event_tracker.overlapping_events,
        event_tracker.bubbles
    );
    Ok(del_subg)
}

fn print<W: io::Write>(affix: &AffixSubgraph, out: &mut io::BufWriter<W>) -> Result<(), io::Error> {
    let parents: Vec<String> = affix
        .parents
        .iter()
        .map(|&v| {
            format!(
                "{}{}",
                if v.is_reverse() { '<' } else { '>' },
                usize::from(v.id()),
            )
        })
        .collect();
    let children: Vec<String> = affix
        .shared_prefix_nodes
        .iter()
        .map(|&v| {
            format!(
                "{}{}",
                if v.is_reverse() { '<' } else { '>' },
                usize::from(v.id()),
            )
        })
        .collect();
    writeln!(
        out,
        "{}\t{}\t{}\t{}",
        parents.join(","),
        children.join(","),
        affix.sequence.len(),
        &affix.sequence,
    )?;
    Ok(())
}

fn print_active_subgraph<W: io::Write>(
    graph: &HashGraph,
    del_subg: &DeletedSubGraph,
    out: &mut io::BufWriter<W>,
) -> Result<(), Box<dyn Error>> {
    for v in graph.handles() {
        if !del_subg.node_deleted(&v) {
            writeln!(
                out,
                "S\t{}\t{}",
                usize::from(v.id()),
                String::from_utf8(graph.sequence_vec(v))?
            )?;
        }
    }
    
    let mut visited : FxHashSet<Handle> = FxHashSet::default();
    for x in graph.handles() {
        for mut v in vec![x, x.flip()] {
            for mut u in graph.neighbors(v, Direction::Right) {
                if !visited.contains(&u) {
                    if u.is_reverse() && v.is_reverse() { 
                        let w = u.flip();
                        u = v.flip();
                        v = w;
                    }
                    if !del_subg.edge_deleted(&u, &v) {
                        writeln!(
                            out,
                            "L\t{}\t{}\t{}\t{}\t0M",
                            usize::from(u.id()),
                            if u.is_reverse() { '-' } else { '+' },
                            usize::from(v.id()),
                            if v.is_reverse() { '-' } else { '+' }
                        )?;
                    } else {
                        log::debug!("edge {}{}{}{} is flagged as deleted", 
                            if u.is_reverse() { '<' } else { '>' },
                            usize::from(u.id()),
                            if v.is_reverse() { '<' } else { '>' },
                            usize::from(v.id()));
                    }
                }
            }
            visited.insert(v);
        }
    }
    Ok(())
}

fn main() -> Result<(), io::Error> {
    env_logger::init();

    // print output to stdout
    let mut out = io::BufWriter::new(std::io::stdout());

    // initialize command line parser & parse command line arguments
    let params = Command::parse();

    log::info!("loading graph {}", &params.graph);
    let parser = GFAParser::new();
    let gfa: GFA<usize, ()> = parser.parse_file(&params.graph).unwrap();

    log::info!("constructing handle graph");
    let mut graph = HashGraph::from_gfa(&gfa);

    if graph.has_edge(Handle::from_integer(184099).flip(), Handle::from_integer(184100)) {
        log::info!("graph has edge <184099>184100");
    }

    log::info!("identifying variant-preserving shared prefixes");
    writeln!(
        out,
        "{}",
        [
            "oriented_parent_nodes",
            "oriented_child_nodes",
            "prefix_length",
            "prefix",
        ]
        .join("\t")
    )?;
    let res = find_and_report_variant_preserving_shared_affixes(&mut graph, &mut out);

    if graph.has_edge(Handle::from_integer(184099).flip(), Handle::from_integer(184100)) {
        log::info!("graph still has edge <184099>184100");
    }
    match res {
        Err(e) => panic!("gfaffix failed: {}", e),
        Ok(del_subg) => {
            if !params.refined_graph_out.trim().is_empty() {
                let mut graph_out =
                    io::BufWriter::new(fs::File::create(params.refined_graph_out.clone())?);
                if let Err(e) = print_active_subgraph(&graph, &del_subg, &mut graph_out) {
                    panic!(
                        "unable to write refined graph to {}: {}",
                        params.refined_graph_out.clone(),
                        e
                    );
                }
            }
        }
    }
    out.flush()?;
    log::info!("done");
    Ok(())
}
