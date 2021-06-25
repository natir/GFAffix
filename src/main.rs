/* standard use */
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::io;
use std::io::prelude::*;
use std::str::{self, FromStr};

/* crate use */
use clap::Clap;
use gfa::{
    gfa::{orientation::Orientation, GFA},
    parser::GFAParser,
};
use handlegraph::{
    handle::{Direction, Edge, Handle},
    handlegraph::*,
    hashgraph::HashGraph,
    mutablehandlegraph::{AdditiveHandleGraph, MutableHandles},
};
use quick_csv::Csv;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;

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
    // tranform from (node_id, node_len) -> [(node_id, node_orient, node_len), ..]
    //                ^ keys are always forward oriented
    pub transform: FxHashMap<(usize, usize), Vec<(usize, Direction, usize)>>,
    pub overlapping_events: usize,
    pub bubbles: usize,
    pub events: usize,
}

impl CollapseEventTracker {
    fn report(
        &mut self,
        graph : &HashGraph,
        collapsed_prefix_node: Handle,
        splitted_node_pairs: &Vec<(usize, Direction, usize, Handle, Option<Handle>)>,
    ) {
        self.events += 1;
        let is_bubble = splitted_node_pairs.iter().all(|x| x.4.is_none());
        if is_bubble {
            self.bubbles += 1;
        }

        let prefix_id  = collapsed_prefix_node.unpack_number() as usize;
        let prefix_orient = if collapsed_prefix_node.is_reverse() { Direction::Left } else
        {Direction::Right};
        let prefix_len = graph.node_len(collapsed_prefix_node);
        for (node_id, node_orient, node_len, _, suffix) in splitted_node_pairs {

            // record transformation of node, even if none took place (which is the case if node v
            // equals the dedicated shared prefix node
            let mut replacement: Vec<(usize, Direction, usize)> = Vec::new();
            replacement.push((prefix_id, prefix_orient, prefix_len));
            if let Some(v) = suffix {
                replacement.push((v.unpack_number() as usize, if v.is_reverse() { Direction::Left } else
                        {Direction::Right}, graph.node_len(*v)));
            }

            // orient transformation
            // important notice:
            // - handle_graph::split_handle() works only in forward direction
            // - the first node of the split pair an will always be the node itself (again in
            //   forward direction)
            if *node_orient == Direction::Left {
                replacement.reverse();
                for r  in replacement.iter_mut() {
                    r.1 = if r.1 == Direction::Left { Direction::Right } else {Direction::Left};
                }
            }
            log::debug!("new replacement rule for {}:{}", node_id, node_len);
            self.transform.insert((*node_id, *node_len), replacement.clone());
        }
    }

    fn expand(&self, node_id : usize, node_orient : Direction, node_len : usize) -> Vec<(usize, Direction, usize)> {
        let mut res: Vec<(usize, Direction, usize)> = Vec::new();

        if self.transform.contains_key(&(node_id, node_len)) {
            for (rid, rorient, rlen) in self.transform.get(&(node_id, node_len)).unwrap() {
                // if identical node appears in its expansion sequence, don't expand...
                if (*rid, *rlen) != (node_id, node_len) {
                    res.extend(self.expand(*rid, *rorient, *rlen));
                } else {
                    res.push((*rid, *rorient, *rlen));
                }
            }
        } else {
            res.push((node_id, node_orient, node_len));
        }

        if node_orient == Direction::Left {
            res.reverse();
            for x in res.iter_mut() {
                x.1 = match x.1 {Direction::Left => Direction::Right, 
                    Direction::Right => Direction:: Left };
            }
        }
        res
    }

    fn get_expanded_transformation(&self) -> FxHashMap<(usize, usize), Vec<(usize, Direction, usize)>> {
        let mut res: FxHashMap<(usize, usize), Vec<(usize, Direction, usize)>> = FxHashMap::default();
        res.reserve(self.transform.len());

        for (node_id, node_len) in self.transform.keys() {
            log::debug!("expanding node {}:{} ... ", node_id, node_len);
            let expansion = self.expand(*node_id, Direction::Right, *node_len);
            log::debug!(
                "deep-expansion of node {}:{} into {}", node_id, node_len,
                expansion.iter()
                    .map(|(rid, ro, rlen)| format!(
                        "{}{}:{}",
                        if *ro == Direction::Left { '<' } else { '>' },
                        rid, rlen
                    ))
                    .collect::<Vec<String>>()
                    .join("")
            );

            res.insert((*node_id, *node_len), expansion);
        }
        res
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
            // get parents of u
            let mut parents: Vec<Handle> = graph
                .neighbors(u, Direction::Left)
                .filter(|w| !del_subg.node_deleted(&w))
                .collect();
            parents.sort();
            // insert child in variant-preserving data structure
            branch
                .entry((graph.base(u, 0).unwrap(), parents))
                .or_insert(Vec::new())
                .push(u);
        }
    }

    for ((_, parents), children) in branch.iter() {
        if children.len() > 1 {
            log::debug!(
                "identified shared prefix between nodes {} originating from parent(s) {}",
                children
                    .iter()
                    .map(|v| format!(
                        "{}{}",
                        if v.is_reverse() { '<' } else { '>' },
                        v.unpack_number()
                    ))
                    .collect::<Vec<String>>()
                    .join(","),
                parents
                    .iter()
                    .map(|v| format!(
                        "{}{}",
                        if v.is_reverse() { '<' } else { '>' },
                        v.unpack_number()
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

    // Sort handles with shared prefix so that reversed ones come first! This is important for the
    // case that two shared prefixes correspond to the same node, one in forward, and one in
    // backward dirction (e.g. >1209, <1209 share prefix 'C'). In those cases, the code that splits
    // handles only works iff the backward oriented handle is split first (e.g., <1209 is split
    // into <329203 and <1209) and then the forward oriented handle (e.g., truncated handle >1209
    // is split into >1209 and 329204), Subsequently, either >1209 or >329203 is deleted, with the
    // other being advanced as dedicated shared prefix node.

    let mut shared_prefix_nodes = shared_prefix.shared_prefix_nodes.clone();
    shared_prefix_nodes.sort_by(|x, y| match (x.is_reverse(), y.is_reverse()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => Ordering::Equal,
    });

    // update graph in two passes:
    //  1. split handles into shared prefix and distinct suffix and appoint dedicated shared
    //  prefix node
    let mut shared_prefix_node_pos: usize = 0;
    // each element in splitted_node_pairs is of the form:
    //  1. node ID of original handle
    //  2. direction of original handle
    //  3. length of original handle
    //  4-5. splitted handle (IMPORTANT: that's generally not equivalent with the replacement
    //  handles!)
    let mut splitted_node_pairs: Vec<(usize, Direction, usize, Handle, Option<Handle>)> = Vec::new();
    for (i, v) in shared_prefix_nodes.iter().enumerate() {
        let v_len = graph.node_len(*v);
        let node_id = v.unpack_number() as usize;
        let node_orient = if v.is_reverse() { Direction::Left} else {Direction::Right};
        if v_len > prefix_len {
            // x corresponds to the shared prefix,
            let (x, u) = if v.is_reverse() {
                // apparently, rs-handlegraph does not allow splitting nodes in reverse direction
                let (u_rev, x_rev) = graph.split_handle(v.flip(), v_len - prefix_len);
                (x_rev.flip(), u_rev.flip())
            } else {
                graph.split_handle(*v, prefix_len)
            };
            splitted_node_pairs.push((node_id, node_orient, v_len, x, Some(u)));
            // update dedicated shared prefix node if none has been assigned yet
            log::debug!(
                "splitting node {}{} into prefix {}{} and suffix {}{}",
                if v.is_reverse() { '<' } else { '>' },
                v.unpack_number(),
                if x.is_reverse() { '<' } else { '>' },
                x.unpack_number(),
                if u.is_reverse() { '<' } else { '>' },
                u.unpack_number()
            );
        } else {
            // always use a node as dedicated shared prefix node if that node coincides with the
            // prefix
            shared_prefix_node_pos = i;
            splitted_node_pairs.push((node_id, node_orient, v_len, *v, None));
            log::debug!(
                "node {}{} matches prefix {}",
                if v.is_reverse() { '<' } else { '>' },
                v.unpack_number(),
                &shared_prefix.sequence
            );
        }
    }

    //  2. update deleted edge set, reassign outgoing edges of "empty" nodes to dedicated shared
    //     prefix node
    // there will be always a shared prefix node, so this condition is always true
    let shared_prefix_node = splitted_node_pairs[shared_prefix_node_pos].3;
    log::debug!(
        "node {}{} is dedicated shared prefix node",
        if shared_prefix_node.is_reverse() {
            '<'
        } else {
            '>'
        },
        shared_prefix_node.unpack_number()
    );

    for (_, _, _, u, maybe_v) in splitted_node_pairs.iter() {
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
                            shared_prefix_node.unpack_number(),
                            if v.is_reverse() { '<' } else { '>' },
                            v.unpack_number()
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
                                shared_prefix_node.unpack_number(),
                                if w.is_reverse() { '<' } else { '>' },
                                w.unpack_number()
                            );
                        }
                    }
                }
            }
            // mark redundant node as deleted
            log::debug!(
                "flag {}{} as deleted",
                if u.is_reverse() { '<' } else { '>' },
                u.unpack_number()
            );
            del_subg.add(*u);
        }
    }

    event_tracker.report(graph, shared_prefix_node, &splitted_node_pairs);
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
) -> Result<(DeletedSubGraph, CollapseEventTracker), Box<dyn Error>> {
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
                    v.unpack_number()
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
    Ok((del_subg, event_tracker))
}

fn transform_path(
    path: &Vec<(usize, Direction, usize)>,
    transform: &FxHashMap<(usize, usize), Vec<(usize, Direction, usize)>>,
) -> Vec<(usize, Direction)> {
    let mut res: Vec<(usize, Direction)> = Vec::new();

    for (sid, o, slen) in path.iter() {
        match transform.get(&(*sid, *slen)) {
            Some(us) => res.extend(
                match o {
                    Direction::Right => us.iter().map(|(x, y, _)| (*x, *y)).collect::<Vec<(usize, Direction)>>(),
                    Direction::Left  => us.iter().rev().map(|(x, y, _)| (*x, if *y == Direction::Left {Direction::Right } else {Direction::Left})).collect::<Vec<(usize, Direction)>>(),
                }),
            None => res.push((*sid, *o)),
        }
    }

    res
}

//fn check_path(_graph: &HashGraph, del_subg: &DeletedSubGraph, path: &Vec<(usize, Orientation)>) {
//    for j in 1..path.len() {
//        let i = j - 1;
//        let u = Handle::new(path[i].0, path[i].1);
//        let v = Handle::new(path[j].0, path[j].1);
//
//        //        if !graph.has_edge(u, v) {
//        //            panic!(
//        //                "edge {}{}{}{} is not part of the graph",
//        //                if u.is_reverse() { '<' } else { '>' },
//        //                u.unpack_number(),
//        //                if v.is_reverse() { '<' } else { '>' },
//        //                v.unpack_number()
//        //            );
//        //        } else if del_subg.edge_deleted(&u, &v) {
//        //            panic!(
//        //                "edge {}{}{}{} is deleted",
//        //                if u.is_reverse() { '<' } else { '>' },
//        //                u.unpack_number(),
//        //                if v.is_reverse() { '<' } else { '>' },
//        //                v.unpack_number()
//        //            );
//        //        }
//        if del_subg.node_deleted(&u) || del_subg.node_deleted(&v) {
//            panic!(
//                "either node {}{} or {}{} or both are deleted",
//                if u.is_reverse() { '<' } else { '>' },
//                u.unpack_number(),
//                if v.is_reverse() { '<' } else { '>' },
//                v.unpack_number()
//            );
//        }
//    }
//}

fn print<W: io::Write>(affix: &AffixSubgraph, out: &mut io::BufWriter<W>) -> Result<(), io::Error> {
    let parents: Vec<String> = affix
        .parents
        .iter()
        .map(|&v| {
            format!(
                "{}{}",
                if v.is_reverse() { '<' } else { '>' },
                v.unpack_number(),
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
                v.unpack_number(),
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
                v.unpack_number(),
                String::from_utf8(graph.sequence_vec(v))?
            )?;
        }
    }

    for Edge(mut u, mut v) in graph.edges() {
        if u.is_reverse() && v.is_reverse() {
            let w = u.flip();
            u = v.flip();
            v = w;
        }
        if !del_subg.edge_deleted(&u, &v) {
            writeln!(
                out,
                "L\t{}\t{}\t{}\t{}\t0M",
                u.unpack_number(),
                if u.is_reverse() { '-' } else { '+' },
                v.unpack_number(),
                if v.is_reverse() { '-' } else { '+' }
            )?;
        }
    }
    Ok(())
}

//fn print_transformed_paths<W: io::Write>(
//    gfa: &GFA<usize, ()>,
//    graph: &HashGraph,
//    transform: &FxHashMap<Handle, Vec<Handle>>,
//    del_subg: &DeletedSubGraph,
//    out: &mut io::BufWriter<W>,
//) -> Result<(), Box<dyn Error>> {
//    for path in gfa.paths.iter() {
//        log::debug!("transforming path {}", str::from_utf8(&path.path_name)?);
//        let tpath = transform_path(&path.iter().collect(), &transform);
//        check_path(graph, del_subg, &tpath);
//        writeln!(
//            out,
//            "P\t{}\t{}\t*",
//            str::from_utf8(&path.path_name)?,
//            tpath
//                .iter()
//                .map(|(sid, o)| format!(
//                    "{}{}",
//                    sid,
//                    if *o == Orientation::Forward { '+' } else { '-' }
//                ))
//                .collect::<Vec<String>>()
//                .join(",")
//        )?;
//    }
//
//    Ok(())
//}

fn parse_and_transform_walks<W: io::Write, R: io::Read>(
    mut data: io::BufReader<R>,
//    graph: &HashGraph,
    transform: &FxHashMap<(usize, usize), Vec<(usize, Direction, usize)>>,
    orig_node_lens : &FxHashMap<usize,usize>,
//    del_subg: &DeletedSubGraph,
    out: &mut io::BufWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let reader = Csv::from_reader(&mut data)
        .delimiter(b'\t')
        .flexible(true)
        .has_header(false);

    for row in reader.into_iter() {
        let row = row.unwrap();
        let mut row_it = row.bytes_columns();

        if &[b'W'] == row_it.next().unwrap() {
            let sample_id = str::from_utf8(row_it.next().unwrap())?;
            let hap_idx = str::from_utf8(row_it.next().unwrap())?;
            let seq_id = str::from_utf8(row_it.next().unwrap())?;
            let seq_start = str::from_utf8(row_it.next().unwrap())?;
            let seq_end = str::from_utf8(row_it.next().unwrap())?;
            let walk_ident = format!(
                "{}#{}#{}:{}-{}",
                sample_id, hap_idx, seq_id, seq_start, seq_end
            );
            log::debug!("transforming walk {}", walk_ident);

            let walk_data = row_it.next().unwrap();
            let mut walk: Vec<(usize, Direction, usize)> = Vec::new();

            let mut cur_el: Vec<u8> = Vec::new();
            for c in walk_data {
                if (c == &b'>' || c == &b'<') && !cur_el.is_empty() {
                    let sid = usize::from_str(str::from_utf8(&cur_el[1..])?)?;
                    let o = match cur_el[0] {
                        b'>' => Direction::Right,
                        b'<' => Direction::Left,
                        _ => panic!(
                            "unknown orientation '{}' of segment {} in walk {}",
                            cur_el[0], sid, walk_ident
                        ),
                    };
                    walk.push((sid, o, *orig_node_lens.get(&sid).unwrap()));
                    cur_el.clear();
                }
                cur_el.push(*c);
            }

            if !cur_el.is_empty() {
                let sid = usize::from_str(str::from_utf8(&cur_el[1..])?)?;
                let o = match cur_el[0] {
                    b'>' => Direction::Right,
                    b'<' => Direction::Left,
                    _ => panic!(
                        "unknown orientation '{}' of segment {} in walk {}",
                        cur_el[0], sid, walk_ident
                    ),
                };
                walk.push((sid, o, *orig_node_lens.get(&sid).unwrap()));
            }

            let tpath = transform_path(&walk, &transform);
//            check_path(graph, del_subg, &tpath);
            writeln!(
                out,
                "W\t{}\t{}\t{}\t{}\t{}\t{}",
                sample_id,
                hap_idx,
                seq_id,
                seq_start,
                seq_end,
                tpath
                    .iter()
                    .map(|(sid, o)| format!(
                        "{}{}",
                        if *o == Direction::Right { '>' } else { '<' },
                        sid
                    ))
                    .collect::<Vec<String>>()
                    .join("")
            )?;
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

    log::info!("storing length of original nodes for bookkeeping");
    let mut node_lens : FxHashMap<usize, usize> = FxHashMap::default();
    node_lens.reserve(graph.node_count());
    for v in graph.handles() {
        node_lens.insert(v.unpack_number() as usize, graph.node_len(v));
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

    match res {
        Err(e) => panic!("gfaffix failed: {}", e),
        Ok((del_subg, event_tracker)) => {
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
                let transform = event_tracker.get_expanded_transformation();
//                log::info!("transforming paths..");
//                if let Err(e) =
//                    print_transformed_paths(&gfa, &graph, &transform, &del_subg, &mut graph_out)
//                {
//                    panic!(
//                        "unable to write refined graph paths to {}: {}",
//                        params.refined_graph_out.clone(),
//                        e
//                    );
//                };
                log::info!("transforming walks..");
                let data = io::BufReader::new(fs::File::open(&params.graph)?);
                if let Err(e) =
                    parse_and_transform_walks(data, &transform, &node_lens, &mut graph_out)
                {
                    panic!(
                        "unable to parse or write refined graph walks to {}: {}",
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
