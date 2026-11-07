//! mmd-svg: render a focused Mermaid flowchart subset to self-contained inline SVG.
//!
//! Built for static-site generators: parse a fenced `mermaid` block,
//! lay it out in ranked layers, and emit one `<svg>` string with no
//! scripts, no external references, and deterministic output.
//!
//! Supported syntax (a deliberate subset):
//!
//! ```text
//! flowchart TD
//!     A[Label] --> B(Rounded)
//!     B -->|yes| C{Decision}
//!     C -- no --> D[(Database)]
//!     A --> E((Circle)) --> F
//! ```
//!
//! Directions `TD`/`TB`, `LR`, `RL`, `BT` are accepted; layout supports the
//! vertical (`TD`) and horizontal (`LR`) flows, mapping the reversed
//! directions onto them.

// Diagram geometry lives well below 2^52; usize->f64 casts are exact here.
#![allow(clippy::cast_precision_loss)]
// The SVG emitter is one growing String by design (deterministic, allocation-light);
// write!-through-Fmt would add nothing but indirection.
#![allow(clippy::format_push_string)]
// Layout math reads best with short geometry names (x, y, w, h, t for the theme).
#![allow(clippy::many_single_char_names, clippy::similar_names)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

/// A parse or layout failure, with a short human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mmd-svg: {}", self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}

/// Visual shape of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `A[Square]`
    Rect,
    /// `A(Rounded)`
    Rounded,
    /// `A{Decision}`
    Diamond,
    /// `A((Circle))` — also used for `A[(Cylinder)]`
    Stadium,
}

/// A node from the source, with its label.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub shape: Shape,
}

/// How an edge is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeStyle {
    /// `-->`
    Solid,
    /// `-.->`
    Dotted,
    /// `==>`
    Thick,
}

/// A directed edge between two nodes, by node index.
#[derive(Debug, Clone)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: Option<String>,
    pub style: EdgeStyle,
}

/// Overall flow direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Top to bottom (`flowchart TD`, the default).
    TopDown,
    /// Left to right (`flowchart LR`).
    LeftRight,
}

/// A parsed diagram: nodes, edges, and the declared direction.
#[derive(Debug, Clone)]
pub struct Diagram {
    pub direction: Direction,
    /// Nodes in first-appearance order.
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Theme and spacing for rendering.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Node stroke color.
    pub stroke: String,
    /// Node fill color.
    pub fill: String,
    /// Node label color.
    pub text: String,
    /// Edge line color.
    pub edge: String,
    /// Edge label color.
    pub edge_text: String,
    /// Backdrop behind edge labels (the page background).
    pub label_bg: String,
    /// Font family for all text.
    pub font_family: String,
    /// Font size in px for node labels.
    pub font_size: f64,
    /// Font size in px for edge labels.
    pub edge_font_size: f64,
    /// Gap between layers (rank spacing), px.
    pub rank_gap: f64,
    /// Minimum gap between nodes in a layer, px.
    pub node_gap: f64,
    /// Node padding around the label, px.
    pub node_pad: f64,
    /// Wrap node labels longer than this many characters at word breaks.
    pub wrap_chars: usize,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            stroke: "#dd68ff".into(),
            fill: "#2a1c33".into(),
            text: "#f2e9fa".into(),
            edge: "#9b8fae".into(),
            edge_text: "#cbbfd9".into(),
            label_bg: "#0d0a10".into(),
            font_family: "JetBrains Mono, ui-monospace, monospace".into(),
            font_size: 13.0,
            edge_font_size: 11.0,
            rank_gap: 64.0,
            node_gap: 36.0,
            node_pad: 12.0,
            wrap_chars: 26,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

const EDGE_OPS: &[(&str, EdgeStyle)] = &[
    ("-.->", EdgeStyle::Dotted),
    ("==>", EdgeStyle::Thick),
    ("-->", EdgeStyle::Solid),
    ("---", EdgeStyle::Solid),
];

/// Find the next edge operator outside of double-quoted spans.
fn find_edge_op(line: &str) -> Option<(usize, &'static str, EdgeStyle)> {
    let mut in_quotes = false;
    let mut esc = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if esc {
            esc = false;
        } else if c == '\\' && in_quotes {
            esc = true;
        } else if c == '"' {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            for (op, style) in EDGE_OPS {
                if line[i..].starts_with(op) {
                    return Some((i, op, *style));
                }
            }
        }
        i += 1;
    }
    None
}

/// Split a line on `%%` comments outside quoted spans.
fn strip_comment(line: &str) -> &str {
    match find_op_or_comment(line) {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

fn find_op_or_comment(line: &str) -> Option<usize> {
    let mut in_quotes = false;
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'%' if !in_quotes && line[i..].starts_with("%%") => return Some(i),
            _ => {}
        }
    }
    None
}

/// Scan one node reference from the start of `text`, skipping leading
/// whitespace. Returns `(id, Option<(label, shape)>, consumed_bytes)`; the
/// label part is present only when this reference defines it.
fn scan_node_ref(text: &str) -> Result<(String, Option<(String, Shape)>, usize), Error> {
    let t = text.trim_start();
    let skipped = text.len() - t.len();
    if t.is_empty() {
        return err("expected a node reference, found nothing");
    }
    let mut in_quotes = false;
    let bytes = t.as_bytes();
    let mut open = None;
    let mut id_end = t.len();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'[' | b'(' | b'{' if !in_quotes => {
                open = Some(i);
                id_end = i;
                break;
            }
            b' ' | b'\t' if !in_quotes => {
                id_end = i;
                break;
            }
            _ => {}
        }
    }
    let id = t[..id_end].trim().to_string();
    if id.is_empty() {
        return err(format!("node reference without an id near {t:?}"));
    }
    let Some(i) = open else {
        return Ok((id, None, skipped + id_end));
    };
    let rest = &t[i..];
    let (prefix, closer, shape) = if rest.starts_with("((") {
        (2u8, "))", Shape::Stadium)
    } else if rest.starts_with("[(") {
        (2, ")]", Shape::Stadium)
    } else if rest.starts_with('[') {
        (1, "]", Shape::Rect)
    } else if rest.starts_with('(') {
        (1, ")", Shape::Rounded)
    } else if rest.starts_with('{') {
        (1, "}", Shape::Diamond)
    } else {
        return err(format!("unknown shape opener near {rest:?}"));
    };
    // quote-aware scan for the closer
    let body = &rest[prefix as usize..];
    let mut in_quotes = false;
    let mut esc = false;
    let mut end = None;
    for (j, &b) in body.as_bytes().iter().enumerate() {
        if esc {
            esc = false;
        } else if b == b'\\' && in_quotes {
            esc = true;
        } else if b == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && body[j..].starts_with(closer) {
            end = Some(j);
            break;
        }
    }
    let Some(j) = end else {
        return err(format!("unclosed {closer} after node {id:?}"));
    };
    let consumed = skipped + i + prefix as usize + j + closer.len();
    Ok((
        id,
        Some((unquote(&body[..j]).trim().to_string(), shape)),
        consumed,
    ))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    s.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// Parse an edge label: `|text|` right after the operator, or the
/// `-- text -->` / `-. text .->` inline form handled by the caller.
fn parse_pipe_label(rest: &str) -> (Option<String>, &str) {
    let r = rest.trim_start();
    if let Some(r) = r
        .strip_prefix('|')
        .and_then(|r| r.find('|').map(|end| (r, end)))
    {
        let (body, end) = r;
        return (Some(unquote(&body[..end])), &body[end + 1..]);
    }
    (None, r)
}

impl Diagram {
    /// Parse a diagram from mermaid source (with or without the `flowchart`
    /// header line).
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] for unknown directions, malformed node
    /// references or edges, and empty input.
    pub fn parse(source: &str) -> Result<Diagram, Error> {
        let mut dir = Direction::TopDown;
        let mut nodes: Vec<Node> = Vec::new();
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        let mut edges: Vec<Edge> = Vec::new();
        let mut saw_content = false;

        for raw in source.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("flowchart") || lower.starts_with("graph") {
                let d = line.split_whitespace().nth(1).unwrap_or("TD");
                dir = match d.to_ascii_uppercase().as_str() {
                    "TD" | "TB" => Direction::TopDown,
                    "LR" | "RL" | "BT" => Direction::LeftRight, // RL/BT render as LR
                    other => return err(format!("unsupported direction {other:?}")),
                };
                saw_content = true;
                continue;
            }
            saw_content = true;
            // walk the statement: node refs and edge operators alternate:
            // `A -->|x| B -.-> C`. Edge labels use the pipe form.
            let mut rest = line;
            let (first_id, first_def, consumed) = scan_node_ref(rest)?;
            let mut from = ensure(&mut nodes, &mut index, first_id, first_def);
            rest = &rest[consumed..];
            loop {
                if let Some((pos, op, style)) = find_edge_op(rest) {
                    let lead = rest[..pos].trim();
                    if !lead.is_empty() {
                        return err(format!("unexpected text {lead:?} between nodes"));
                    }
                    let (label, after_op) = parse_pipe_label(&rest[pos + op.len()..]);
                    let (id, def, c2) = scan_node_ref(after_op)?;
                    let to = ensure(&mut nodes, &mut index, id, def);
                    edges.push(Edge {
                        from,
                        to,
                        label,
                        style,
                    });
                    from = to;
                    rest = &after_op[c2..];
                } else {
                    if !rest.trim().is_empty() {
                        return err(format!("unexpected text after an edge: {:?}", rest.trim()));
                    }
                    break;
                }
            }
        }
        if !saw_content {
            return err("no diagram content found");
        }
        Ok(Diagram {
            direction: dir,
            nodes,
            edges,
        })
    }
}

/// Make sure a node exists (adding or updating it) and return its index.
fn ensure(
    nodes: &mut Vec<Node>,
    index: &mut BTreeMap<String, usize>,
    id: String,
    def: Option<(String, Shape)>,
) -> usize {
    if let Some(&i) = index.get(&id) {
        if let Some((label, shape)) = def {
            nodes[i].label = label;
            nodes[i].shape = shape;
        }
        return i;
    }
    let (label, shape) = def.unwrap_or_else(|| (id.clone(), Shape::Rect));
    let i = nodes.len();
    nodes.push(Node {
        id: id.clone(),
        label,
        shape,
    });
    index.insert(id, i);
    i
}

// ---------------------------------------------------------------------------
// Layout: a small Sugiyama pipeline
//   1. reverse DFS back edges so the graph is acyclic
//   2. longest-path ranking, sources pulled down next to their children
//   3. dummy vertices on edges that span several ranks
//   4. barycenter sweeps to reduce crossings (keeping the best ordering)
//   5. balanced placement across the flow, then layer offsets along it
// ---------------------------------------------------------------------------

/// Monospace advance as a fraction of the font size (600/1000 in `JetBrains Mono`).
const ADVANCE: f64 = 0.6;
/// Line height as a multiple of the font size.
const LINE: f64 = 1.35;

fn text_width(s: &str, size: f64) -> f64 {
    s.chars().count() as f64 * size * ADVANCE
}

/// Greedy word wrap to at most `max` characters per line; long words stay whole.
fn wrap(label: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in label.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > max {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Wrapped label lines and the outer box size of a node.
fn node_box(nd: &Node, t: &Theme) -> (Vec<String>, f64, f64) {
    let max = if nd.shape == Shape::Diamond {
        t.wrap_chars * 2 / 3
    } else {
        t.wrap_chars
    };
    let lines = wrap(&nd.label, max.max(8));
    let tw = lines
        .iter()
        .map(|l| text_width(l, t.font_size))
        .fold(0.0, f64::max);
    let th = lines.len() as f64 * t.font_size * LINE;
    let pad = t.node_pad;
    let (w, h) = match nd.shape {
        Shape::Rect | Shape::Rounded => (tw + 2.0 * pad, th + 1.5 * pad),
        Shape::Stadium => {
            let h = th + 1.5 * pad;
            (tw + 2.0 * pad + h / 2.0, h)
        }
        Shape::Diamond => {
            // the text box's corners must sit inside the rhombus:
            // tw/W + th/H <= 1
            let w = tw.mul_add(1.6, 2.0 * pad);
            (w, th / (1.0 - tw / w) + pad)
        }
    };
    (lines, w, h)
}

/// A vertex of the layered graph: a real node or a dummy on a long edge.
#[derive(Debug, Clone)]
struct Vert {
    /// Index into `Diagram::nodes`, or `None` for a dummy.
    node: Option<usize>,
    rank: usize,
    /// Extent across the flow (width for top-down) and along it.
    cross: f64,
    main: f64,
    /// Center across the flow, then along it.
    pos: f64,
    depth: f64,
    /// Set on the dummy that carries an edge's label.
    label: bool,
}

struct Graph {
    verts: Vec<Vert>,
    /// Links between adjacent ranks: (upper, lower) vertex indices.
    links: Vec<(usize, usize)>,
    /// Per diagram edge: vertex chain in rank order, and whether the edge
    /// was reversed to break a cycle.
    chains: Vec<(Vec<usize>, bool)>,
    layers: Vec<Vec<usize>>,
}

/// Flag edges that close a cycle (DFS back edges), so they can be reversed.
fn back_edges(n: usize, edges: &[Edge]) -> Vec<bool> {
    let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (k, e) in edges.iter().enumerate() {
        adj[e.from].push((e.to, k));
    }
    let mut state = vec![0u8; n]; // 0 unseen, 1 on the stack, 2 done
    let mut back = vec![false; edges.len()];
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        let mut stack = vec![(root, 0usize)];
        while let Some(top) = stack.last_mut() {
            let u = top.0;
            if let Some(&(v, k)) = adj[u].get(top.1) {
                top.1 += 1;
                match state[v] {
                    0 => {
                        state[v] = 1;
                        stack.push((v, 0));
                    }
                    1 => back[k] = true,
                    _ => {}
                }
            } else {
                state[u] = 2;
                stack.pop();
            }
        }
    }
    back
}

/// Longest-path ranks over an acyclic edge list; a node with no
/// predecessors moves down to sit just above its nearest child.
fn rank_nodes(n: usize, dag: &[(usize, usize)]) -> Vec<usize> {
    let mut indeg = vec![0usize; n];
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut has_pred = vec![false; n];
    for &(a, b) in dag {
        if a != b {
            succ[a].push(b);
            indeg[b] += 1;
            has_pred[b] = true;
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
    let mut order = Vec::with_capacity(n);
    let mut rank = vec![0usize; n];
    while let Some(u) = queue.pop_front() {
        order.push(u);
        for &v in &succ[u] {
            rank[v] = rank[v].max(rank[u] + 1);
            indeg[v] -= 1;
            if indeg[v] == 0 {
                queue.push_back(v);
            }
        }
    }
    for &u in order.iter().rev() {
        if !has_pred[u]
            && let Some(m) = succ[u].iter().map(|&v| rank[v]).min()
        {
            rank[u] = m - 1;
        }
    }
    rank
}

/// Size of an edge label's backdrop.
fn label_box(label: &str, t: &Theme) -> (f64, f64) {
    (
        text_width(label, t.edge_font_size) + 10.0,
        t.edge_font_size * LINE + 4.0,
    )
}

fn build(diag: &Diagram, t: &Theme, td: bool) -> (Graph, Vec<Vec<String>>, usize) {
    let n = diag.nodes.len();
    let back = back_edges(n, &diag.edges);
    let dag: Vec<(usize, usize)> = diag
        .edges
        .iter()
        .zip(&back)
        .map(|(e, &b)| if b { (e.to, e.from) } else { (e.from, e.to) })
        .collect();
    // With edge labels, every edge spans at least two ranks so its label
    // can take the middle one as a sized dummy vertex.
    let scale = if diag.edges.iter().any(|e| e.label.is_some()) {
        2
    } else {
        1
    };
    let rank: Vec<usize> = rank_nodes(n, &dag).into_iter().map(|r| r * scale).collect();
    let mut lines = Vec::with_capacity(n);
    let mut verts: Vec<Vert> = Vec::with_capacity(n);
    for (i, nd) in diag.nodes.iter().enumerate() {
        let (l, w, h) = node_box(nd, t);
        lines.push(l);
        let (cross, main) = if td { (w, h) } else { (h, w) };
        verts.push(Vert {
            node: Some(i),
            rank: rank[i],
            cross,
            main,
            pos: 0.0,
            depth: 0.0,
            label: false,
        });
    }
    let mut links = Vec::new();
    let mut chains = Vec::with_capacity(dag.len());
    for (k, &(a, b)) in dag.iter().enumerate() {
        let mut chain = vec![a];
        if a != b {
            let label_rank = diag.edges[k]
                .label
                .as_ref()
                .map(|_| rank[a] + (rank[b] - rank[a]) / 2);
            let mut prev = a;
            for r in rank[a] + 1..rank[b] {
                let d = verts.len();
                let (cross, main, label) = match (&diag.edges[k].label, label_rank) {
                    (Some(l), Some(lr)) if lr == r => {
                        let (w, h) = label_box(l, t);
                        if td { (w, h, true) } else { (h, w, true) }
                    }
                    _ => (0.0, 0.0, false),
                };
                verts.push(Vert {
                    node: None,
                    rank: r,
                    cross,
                    main,
                    pos: 0.0,
                    depth: 0.0,
                    label,
                });
                links.push((prev, d));
                chain.push(d);
                prev = d;
            }
            links.push((prev, b));
            chain.push(b);
        }
        chains.push((chain, back[k]));
    }
    let max_rank = verts.iter().map(|v| v.rank).max().unwrap_or(0);
    let mut layers = vec![Vec::new(); max_rank + 1];
    for (i, v) in verts.iter().enumerate() {
        layers[v.rank].push(i);
    }
    (
        Graph {
            verts,
            links,
            chains,
            layers,
        },
        lines,
        scale,
    )
}

fn index_layers(layers: &[Vec<usize>], idx: &mut [usize]) {
    for l in layers {
        for (i, &v) in l.iter().enumerate() {
            idx[v] = i;
        }
    }
}

fn crossings(layers: &[Vec<usize>], down: &[Vec<usize>], idx: &[usize]) -> usize {
    let mut total = 0;
    for l in layers {
        let segs: Vec<(usize, usize)> = l
            .iter()
            .flat_map(|&a| down[a].iter().map(move |&b| (idx[a], idx[b])))
            .collect();
        for (i, &(a1, b1)) in segs.iter().enumerate() {
            for &(a2, b2) in &segs[i + 1..] {
                if (a1 < a2 && b1 > b2) || (a1 > a2 && b1 < b2) {
                    total += 1;
                }
            }
        }
    }
    total
}

fn adjacency(g: &Graph) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let mut up = vec![Vec::new(); g.verts.len()];
    let mut down = vec![Vec::new(); g.verts.len()];
    for &(a, b) in &g.links {
        down[a].push(b);
        up[b].push(a);
    }
    (up, down)
}

/// Barycenter sweeps, alternating down and up; keeps the ordering with
/// the fewest crossings.
fn order(g: &mut Graph) {
    let (up, down) = adjacency(g);
    let mut idx = vec![0usize; g.verts.len()];
    index_layers(&g.layers, &mut idx);
    let mut best = g.layers.clone();
    let mut best_x = crossings(&g.layers, &down, &idx);
    let nl = g.layers.len();
    for it in 0..16 {
        let downward = it % 2 == 0;
        let seq: Vec<usize> = if downward {
            (1..nl).collect()
        } else {
            (0..nl.saturating_sub(1)).rev().collect()
        };
        for li in seq {
            let nbrs = if downward { &up } else { &down };
            let mut keyed: Vec<(f64, usize, usize)> = g.layers[li]
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let ns = &nbrs[v];
                    let key = if ns.is_empty() {
                        i as f64
                    } else {
                        ns.iter().map(|&u| idx[u] as f64).sum::<f64>() / ns.len() as f64
                    };
                    (key, i, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            g.layers[li] = keyed.into_iter().map(|k| k.2).collect();
            for (i, &v) in g.layers[li].iter().enumerate() {
                idx[v] = i;
            }
        }
        let x = crossings(&g.layers, &down, &idx);
        if x < best_x {
            best_x = x;
            best.clone_from(&g.layers);
        }
    }
    g.layers = best;
}

/// Minimum center-to-center distance between neighbours in a layer.
fn sep(g: &Graph, a: usize, b: usize, gap: f64) -> f64 {
    let (va, vb) = (&g.verts[a], &g.verts[b]);
    let solid = |v: &Vert| v.node.is_some() || v.label;
    let g2 = match (solid(va), solid(vb)) {
        (true, true) => gap * if va.label || vb.label { 0.5 } else { 1.0 },
        (false, false) => 14.0,
        _ => gap * 0.6,
    };
    f64::midpoint(va.cross, vb.cross) + g2
}

/// Place vertices across the flow: each layer moves toward the mean of
/// its neighbours, packed left and right and averaged so spacing holds.
fn place(g: &mut Graph, t: &Theme, scale: usize) {
    for l in &g.layers {
        let mut x = 0.0;
        for (i, &v) in l.iter().enumerate() {
            if i > 0 {
                x += sep(g, l[i - 1], v, t.node_gap);
            }
            g.verts[v].pos = x;
        }
    }
    let (up, down) = adjacency(g);
    let nl = g.layers.len();
    // an odd count ends on a downward sweep, so chains line up under their parents
    for it in 0..25 {
        let downward = it % 2 == 0;
        let seq: Vec<usize> = if downward {
            (1..nl).collect()
        } else {
            (0..nl.saturating_sub(1)).rev().collect()
        };
        for li in seq {
            let l = g.layers[li].clone();
            let k = l.len();
            let desired: Vec<f64> = l
                .iter()
                .map(|&v| {
                    let ns = if downward { &up[v] } else { &down[v] };
                    if ns.is_empty() {
                        g.verts[v].pos
                    } else {
                        ns.iter().map(|&u| g.verts[u].pos).sum::<f64>() / ns.len() as f64
                    }
                })
                .collect();
            let mut left = desired.clone();
            for i in 1..k {
                left[i] = left[i].max(left[i - 1] + sep(g, l[i - 1], l[i], t.node_gap));
            }
            let mut right = desired.clone();
            for i in (0..k.saturating_sub(1)).rev() {
                right[i] = right[i].min(right[i + 1] - sep(g, l[i], l[i + 1], t.node_gap));
            }
            for i in 0..k {
                g.verts[l[i]].pos = f64::midpoint(left[i], right[i]);
            }
        }
    }
    let min = g
        .verts
        .iter()
        .map(|v| v.pos - v.cross / 2.0)
        .fold(f64::INFINITY, f64::min);
    for v in &mut g.verts {
        v.pos -= min;
    }
    // along the flow: each layer as thick as its largest node
    let mut thick = vec![0.0f64; nl];
    for v in &g.verts {
        thick[v.rank] = thick[v.rank].max(v.main);
    }
    let mut off = 0.0;
    let mut start = vec![0.0f64; nl];
    for r in 0..nl {
        start[r] = off;
        off += thick[r] + t.rank_gap / scale as f64;
    }
    for v in &mut g.verts {
        v.depth = start[v.rank] + thick[v.rank] / 2.0;
    }
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Format a float compactly (one decimal, no trailing zeros).
fn f(v: f64) -> String {
    let s = format!("{v:.1}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

#[derive(Clone, Copy)]
struct Pt {
    x: f64,
    y: f64,
}

struct Bounds {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Bounds {
    fn add(&mut self, x: f64, y: f64) {
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x);
        self.y1 = self.y1.max(y);
    }
}

fn shape_svg(shape: Shape, cx: f64, cy: f64, w: f64, h: f64, t: &Theme) -> String {
    let common = format!(
        r#"fill="{}" stroke="{}" stroke-width="1.4""#,
        t.fill, t.stroke
    );
    let (x, y) = (cx - w / 2.0, cy - h / 2.0);
    match shape {
        Shape::Rect => format!(
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="4" {common}/>"#,
            f(x),
            f(y),
            f(w),
            f(h)
        ),
        Shape::Rounded | Shape::Stadium => format!(
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{}" {common}/>"#,
            f(x),
            f(y),
            f(w),
            f(h),
            f(if shape == Shape::Stadium {
                h / 2.0
            } else {
                10.0
            })
        ),
        Shape::Diamond => format!(
            r#"<polygon points="{},{} {},{} {},{} {},{}" {common}/>"#,
            f(cx),
            f(y),
            f(x + w),
            f(cy),
            f(cx),
            f(y + h),
            f(x),
            f(cy)
        ),
    }
}

/// Render mermaid source to a self-contained inline `<svg>...</svg>` string.
///
/// # Errors
///
/// Propagates [`Diagram::parse`] failures.
pub fn render(source: &str) -> Result<String, Error> {
    render_with_theme(source, &Theme::default())
}

/// A laid-out diagram, ready to draw.
struct Scene<'a> {
    diag: &'a Diagram,
    t: &'a Theme,
    g: Graph,
    lines: Vec<Vec<String>>,
    td: bool,
}

type Ports = BTreeMap<(usize, usize), f64>;

impl Scene<'_> {
    /// Screen-space center of a vertex.
    fn at(&self, v: usize) -> Pt {
        let vert = &self.g.verts[v];
        if self.td {
            Pt {
                x: vert.pos,
                y: vert.depth,
            }
        } else {
            Pt {
                x: vert.depth,
                y: vert.pos,
            }
        }
    }

    /// Screen-space (width, height) of a vertex.
    fn size(&self, v: usize) -> (f64, f64) {
        let vert = &self.g.verts[v];
        if self.td {
            (vert.cross, vert.main)
        } else {
            (vert.main, vert.cross)
        }
    }

    /// Offsets that spread the edges leaving (or entering) one node side
    /// by side, ordered by where their other end sits.
    fn port_offsets(&self) -> (Ports, Ports) {
        let mut outs: BTreeMap<usize, Vec<(f64, usize)>> = BTreeMap::new();
        let mut ins: BTreeMap<usize, Vec<(f64, usize)>> = BTreeMap::new();
        for (k, (chain, _)) in self.g.chains.iter().enumerate() {
            if let [a, a2, ..] = chain[..] {
                outs.entry(a).or_default().push((self.g.verts[a2].pos, k));
            }
            if let [.., b2, b] = chain[..] {
                ins.entry(b).or_default().push((self.g.verts[b2].pos, k));
            }
        }
        let spread_out = |ports: BTreeMap<usize, Vec<(f64, usize)>>| -> Ports {
            let mut m = Ports::new();
            for (v, mut list) in ports {
                list.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                let vert = &self.g.verts[v];
                let diamond = vert
                    .node
                    .is_some_and(|i| self.diag.nodes[i].shape == Shape::Diamond);
                let n = list.len();
                let spread = if diamond || n < 2 {
                    0.0
                } else {
                    (vert.cross * 0.5).min(18.0 * (n - 1) as f64)
                };
                for (i, &(_, k)) in list.iter().enumerate() {
                    let o = if n < 2 {
                        0.0
                    } else {
                        -spread / 2.0 + spread * i as f64 / (n - 1) as f64
                    };
                    m.insert((v, k), o);
                }
            }
            m
        };
        (spread_out(outs), spread_out(ins))
    }

    /// SVG path data for edge `k`, and where its label goes.
    fn edge_path(&self, k: usize, ports: &(Ports, Ports), bounds: &mut Bounds) -> (String, Pt) {
        let (chain, reversed) = &self.g.chains[k];
        if let [v] = chain[..] {
            // self-loop on the far side of the node
            let (p, (w, h)) = (self.at(v), self.size(v));
            let (sx, sy) = (p.x + w / 2.0, p.y);
            bounds.add(sx + 40.0, sy - h / 2.0 - 8.0);
            let d = format!(
                "M{},{} C{},{} {},{} {},{}",
                f(sx),
                f(sy - h / 4.0),
                f(sx + 36.0),
                f(sy - h / 2.0 - 8.0),
                f(sx + 36.0),
                f(sy + h / 2.0 + 8.0),
                f(sx),
                f(sy + h / 4.0)
            );
            return (
                d,
                Pt {
                    x: sx + 30.0,
                    y: sy,
                },
            );
        }
        let (first, last) = (chain[0], chain[chain.len() - 1]);
        let o1 = ports.0.get(&(first, k)).copied().unwrap_or(0.0);
        let o2 = ports.1.get(&(last, k)).copied().unwrap_or(0.0);
        let mut pts: Vec<Pt> = chain.iter().map(|&v| self.at(v)).collect();
        let n = pts.len();
        let ((w1, h1), (w2, h2)) = (self.size(first), self.size(last));
        if self.td {
            pts[0] = Pt {
                x: pts[0].x + o1,
                y: pts[0].y + h1 / 2.0,
            };
            pts[n - 1] = Pt {
                x: pts[n - 1].x + o2,
                y: pts[n - 1].y - h2 / 2.0,
            };
        } else {
            pts[0] = Pt {
                x: pts[0].x + w1 / 2.0,
                y: pts[0].y + o1,
            };
            pts[n - 1] = Pt {
                x: pts[n - 1].x - w2 / 2.0,
                y: pts[n - 1].y + o2,
            };
        }
        let mid = chain.iter().find(|&&v| self.g.verts[v].label).map_or(
            Pt {
                x: f64::midpoint(pts[0].x, pts[1].x),
                y: f64::midpoint(pts[0].y, pts[1].y),
            },
            |&v| self.at(v),
        );
        if *reversed {
            pts.reverse();
        }
        let mut d = format!("M{},{}", f(pts[0].x), f(pts[0].y));
        for w in pts.windows(2) {
            let (p, q) = (w[0], w[1]);
            // smooth curve leaving and entering along the flow direction
            let (c1, c2) = if self.td {
                let dy = (q.y - p.y) / 2.0;
                (
                    Pt {
                        x: p.x,
                        y: p.y + dy,
                    },
                    Pt {
                        x: q.x,
                        y: q.y - dy,
                    },
                )
            } else {
                let dx = (q.x - p.x) / 2.0;
                (
                    Pt {
                        x: p.x + dx,
                        y: p.y,
                    },
                    Pt {
                        x: q.x - dx,
                        y: q.y,
                    },
                )
            };
            d.push_str(&format!(
                " C{},{} {},{} {},{}",
                f(c1.x),
                f(c1.y),
                f(c2.x),
                f(c2.y),
                f(q.x),
                f(q.y)
            ));
            bounds.add(q.x, q.y);
        }
        (d, mid)
    }

    /// Shape and centered (possibly multi-line) label of node vertex `v`.
    fn node_svg(&self, v: usize, i: usize) -> String {
        let t = self.t;
        let (p, (w, h)) = (self.at(v), self.size(v));
        let mut s = shape_svg(self.diag.nodes[i].shape, p.x, p.y, w, h, t);
        let ls = &self.lines[i];
        let lh = t.font_size * LINE;
        let y0 = p.y - (ls.len() as f64 - 1.0) * lh / 2.0;
        s.push_str(&format!(
            r#"<text text-anchor="middle" dominant-baseline="central" font-size="{}" fill="{}">"#,
            f(t.font_size),
            t.text
        ));
        for (j, line) in ls.iter().enumerate() {
            s.push_str(&format!(
                r#"<tspan x="{}" y="{}">{}</tspan>"#,
                f(p.x),
                f(y0 + j as f64 * lh),
                esc(line)
            ));
        }
        s.push_str("</text>");
        s
    }

    /// Text alternative listing every edge.
    fn title(&self) -> String {
        let parts: Vec<String> = self
            .diag
            .edges
            .iter()
            .map(|e| {
                let (a, z) = (&self.diag.nodes[e.from].label, &self.diag.nodes[e.to].label);
                e.label
                    .as_ref()
                    .map_or_else(|| format!("{a} → {z}"), |l| format!("{a} → {z} ({l})"))
            })
            .collect();
        format!("Flowchart: {}", parts.join("; "))
    }
}

/// Render with an explicit [`Theme`].
///
/// # Errors
///
/// Propagates [`Diagram::parse`] failures.
pub fn render_with_theme(source: &str, t: &Theme) -> Result<String, Error> {
    let diag = Diagram::parse(source)?;
    let td = diag.direction == Direction::TopDown;
    let (mut g, lines, scale) = build(&diag, t, td);
    order(&mut g);
    place(&mut g, t, scale);
    let scene = Scene {
        diag: &diag,
        t,
        g,
        lines,
        td,
    };

    let mut bounds = Bounds {
        x0: f64::INFINITY,
        y0: f64::INFINITY,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };
    let mut nodes_svg = String::new();
    for (v, vert) in scene.g.verts.iter().enumerate() {
        let Some(i) = vert.node else { continue };
        let (p, (w, h)) = (scene.at(v), scene.size(v));
        bounds.add(p.x - w / 2.0, p.y - h / 2.0);
        bounds.add(p.x + w / 2.0, p.y + h / 2.0);
        nodes_svg.push_str(&scene.node_svg(v, i));
    }

    let ports = scene.port_offsets();
    let mut edges_svg = String::new();
    let mut labels_svg = String::new();
    for (k, e) in diag.edges.iter().enumerate() {
        let (d, mid) = scene.edge_path(k, &ports, &mut bounds);
        let style = match e.style {
            EdgeStyle::Solid => "",
            EdgeStyle::Dotted => r#" stroke-dasharray="3 4""#,
            EdgeStyle::Thick => r#" stroke-width="2.4""#,
        };
        edges_svg.push_str(&format!(
            r#"<path d="{d}" fill="none" stroke="{}" stroke-width="1.3"{style} marker-end="url(#mmd-arrow)"/>"#,
            t.edge
        ));
        if let Some(label) = &e.label {
            let (lw, lh) = label_box(label, t);
            let (x, y) = (mid.x - lw / 2.0, mid.y - lh / 2.0);
            bounds.add(x, y);
            bounds.add(x + lw, y + lh);
            labels_svg.push_str(&format!(
                r#"<rect x="{}" y="{}" width="{}" height="{}" rx="3" fill="{}"/><text x="{}" y="{}" text-anchor="middle" dominant-baseline="central" font-size="{}" fill="{}">{}</text>"#,
                f(x), f(y), f(lw), f(lh), t.label_bg, f(mid.x), f(mid.y), f(t.edge_font_size), t.edge_text, esc(label)
            ));
        }
    }

    let pad = 6.0;
    let (vx, vy) = (bounds.x0 - pad, bounds.y0 - pad);
    let (vw, vh) = (
        bounds.x1 - bounds.x0 + 2.0 * pad,
        bounds.y1 - bounds.y0 + 2.0 * pad,
    );
    let mut out = String::with_capacity(4096);
    out.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{} {} {} {}" width="{}" height="{}" role="img" font-family="{}">"#,
        f(vx), f(vy), f(vw), f(vh), f(vw), f(vh), esc(&t.font_family)
    ));
    out.push_str(&format!("<title>{}</title>", esc(&scene.title())));
    out.push_str(&format!(
        r#"<defs><marker id="mmd-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerUnits="userSpaceOnUse" markerWidth="9" markerHeight="9" orient="auto-start-reverse"><path d="M0,1 L9,5 L0,9 z" fill="{}"/></marker></defs>"#,
        t.edge
    ));
    out.push_str(&edges_svg);
    out.push_str(&nodes_svg);
    out.push_str(&labels_svg);
    out.push_str("</svg>");
    Ok(out)
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    const PIPE: &str = "flowchart TD
        META[IR metadata] --> PAIR{lit + ambient pair}
        IR[IR camera] --> PAIR
        PAIR -->|lit| GATES[gates]
        GATES --> ARC[ArcFace] --> MATCH{match?}
        MATCH -->|yes| LIVE[liveness]
        RGB[RGB camera] --> VL[liveness net] -->|veto| SPOOF[spoof]
        MATCH -->|no| REJ[reject]
        LIVE --> OK[unlock]
        SPOOF --> REJ
        ";

    fn laid(src: &str) -> (Diagram, Graph) {
        let d = Diagram::parse(src).unwrap();
        let t = Theme::default();
        let (mut g, _, scale) = build(&d, &t, true);
        order(&mut g);
        place(&mut g, &t, scale);
        (d, g)
    }

    #[test]
    fn forward_edges_go_down_ranks() {
        let (d, g) = laid(PIPE);
        for e in &d.edges {
            assert!(
                g.verts[e.to].rank > g.verts[e.from].rank,
                "{} -> {}",
                d.nodes[e.from].id,
                d.nodes[e.to].id
            );
        }
    }

    #[test]
    fn no_overlaps_within_a_layer() {
        let (_, g) = laid(PIPE);
        for l in &g.layers {
            for w in l.windows(2) {
                let (a, b) = (&g.verts[w[0]], &g.verts[w[1]]);
                assert!(a.pos + a.cross / 2.0 <= b.pos - b.cross / 2.0 + 0.01);
            }
        }
    }

    #[test]
    fn sources_sit_next_to_their_children() {
        let (d, g) = laid(PIPE);
        let rgb = d.nodes.iter().position(|n| n.id == "RGB").unwrap();
        let vl = d.nodes.iter().position(|n| n.id == "VL").unwrap();
        // ranks are doubled when edges carry labels
        assert_eq!(g.verts[rgb].rank + 2, g.verts[vl].rank);
    }

    #[test]
    fn cycles_are_drawn_with_reversed_edges() {
        let d = Diagram::parse("flowchart TD\nA --> B --> C --> A\n").unwrap();
        assert_eq!(back_edges(3, &d.edges), vec![false, false, true]);
    }

    #[test]
    fn wrapping_keeps_words_whole() {
        assert_eq!(wrap("alpha beta gamma", 10), vec!["alpha beta", "gamma"]);
        assert_eq!(
            wrap("supercalifragilistic", 5),
            vec!["supercalifragilistic"]
        );
    }
}
