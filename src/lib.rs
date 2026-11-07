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

use std::collections::BTreeMap;
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
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            stroke: "#dd68ff".into(),
            fill: "#2a1c33".into(),
            text: "#f2e9fa".into(),
            edge: "#9b8fae".into(),
            edge_text: "#cbbfd9".into(),
            font_family: "JetBrains Mono, ui-monospace, monospace".into(),
            font_size: 13.0,
            edge_font_size: 11.0,
            rank_gap: 64.0,
            node_gap: 36.0,
            node_pad: 12.0,
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
    Ok((id, Some((unquote(&body[..j]).trim().to_string(), shape)), consumed))
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
    if let Some(r) = r.strip_prefix('|').and_then(|r| r.find('|').map(|end| (r, end))) {
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
                        edges.push(Edge { from, to, label, style });
                        from = to;
                        rest = &after_op[c2..];
                } else {
                    if !rest.trim().is_empty() {
                        return err(format!(
                            "unexpected text after an edge: {:?}",
                            rest.trim()
                        ));
                    }
                    break;
                }
            }
        }
        if !saw_content {
            return err("no diagram content found");
        }
        Ok(Diagram { direction: dir, nodes, edges })
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
    let (label, shape) =
        def.unwrap_or_else(|| (id.clone(), Shape::Rect));
    let i = nodes.len();
    nodes.push(Node { id: id.clone(), label, shape });
    index.insert(id, i);
    i
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Box_ {
    x: f64, // top-left
    y: f64,
    w: f64,
    h: f64,
}

fn label_width(label: &str, font_size: f64, pad: f64) -> f64 {
    // conservative monospace advance of 0.62em
    let chars = label.chars().count().max(1);
    (chars as f64) * font_size * 0.62 + pad * 2.0
}

/// Assign ranks by longest path from a root, ignoring back edges.
fn ranks(n: usize, edges: &[Edge]) -> Vec<usize> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        adj[e.from].push(e.to);
    }
    let mut rank = vec![usize::MAX; n];
    let mut on_stack = vec![false; n];
    for u in 0..n {
        dfs_rank(u, &adj, &mut rank, &mut on_stack);
    }
    rank
}

/// Longest-path rank of `u`, ignoring back edges (nodes on the DFS stack).
fn dfs_rank(u: usize, adj: &[Vec<usize>], rank: &mut [usize], on_stack: &mut [bool]) -> usize {
    if rank[u] != usize::MAX {
        return rank[u];
    }
    on_stack[u] = true;
    let mut r = 0;
    for &v in &adj[u] {
        if on_stack[v] {
            continue; // back edge: ignore for ranking
        }
        r = r.max(dfs_rank(v, adj, rank, on_stack) + 1);
    }
    on_stack[u] = false;
    rank[u] = r;
    r
}

struct Laid {
    boxes: Vec<Box_>,
    width: f64,
    height: f64,
}

/// Lay the diagram out top-to-bottom in ranked layers.
fn layout(diag: &Diagram, t: &Theme) -> Laid {
    let n = diag.nodes.len();
    let rank = ranks(n, &diag.edges);
    let max_rank = rank.iter().copied().max().unwrap_or(0);

    // widths/heights per node
    let widths: Vec<f64> = diag
        .nodes
        .iter()
        .map(|nd| label_width(&nd.label, t.font_size, t.node_pad))
        .collect();
    let height = t.font_size + t.node_pad * 2.0;

    // bucket nodes by rank, keeping first-appearance order
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (i, &r) in rank.iter().enumerate() {
        layers[r].push(i);
    }

    // barycenter ordering sweeps (forward and backward)
    let mut pos_of: Vec<usize> = vec![0; n]; // position within layer
    for _ in 0..4 {
        for (pi, l) in layers.iter().enumerate() {
            for (pi2, &ni) in l.iter().enumerate() {
                let _ = pi;
                pos_of[ni] = pi2;
            }
        }
        let mut bary: Vec<f64> = vec![0.0; n];
        for ni in 0..n {
            let mut sum = 0.0;
            let mut cnt = 0usize;
            for e in &diag.edges {
                if e.to == ni && rank[e.from] < rank[ni] {
                    sum += pos_of[e.from] as f64;
                    cnt += 1;
                }
                if e.from == ni && rank[e.to] > rank[ni] {
                    sum += pos_of[e.to] as f64;
                    cnt += 1;
                }
            }
            bary[ni] = if cnt == 0 { pos_of[ni] as f64 } else { sum / cnt as f64 };
        }
        for l in &mut layers {
            l.sort_by(|&a, &b| bary[a].partial_cmp(&bary[b]).unwrap_or(std::cmp::Ordering::Equal));
        }
        for l in &layers {
            for (pi, &ni) in l.iter().enumerate() {
                pos_of[ni] = pi;
            }
        }
    }

    // geometry: every layer spans the same total width, centered
    let max_layer_w = layers
        .iter()
        .map(|l| {
            let w: f64 = l.iter().map(|&ni| widths[ni]).sum();
            w + t.node_gap * (l.len().saturating_sub(1)) as f64
        })
        .fold(0.0f64, f64::max)
        .max(120.0);
    let total_w = max_layer_w;
    let total_h = (max_rank as f64 + 1.0) * height + max_rank as f64 * t.rank_gap;

    let mut boxes = vec![Box_ { x: 0.0, y: 0.0, w: 0.0, h: height }];
    boxes.clear();
    for (li, l) in layers.iter().enumerate() {
        let lw: f64 = l.iter().map(|&ni| widths[ni]).sum::<f64>()
            + t.node_gap * (l.len().saturating_sub(1)) as f64;
        let mut x = (total_w - lw) / 2.0;
        let y = li as f64 * (height + t.rank_gap);
        for &ni in l {
            boxes.push(Box_ { x, y, w: widths[ni], h: height });
            x += widths[ni] + t.node_gap;
        }
    }
    Laid { boxes, width: total_w, height: total_h }
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn anchor(b: &Box_, bottom: bool) -> (f64, f64) {
    let cx = b.x + b.w / 2.0;
    if bottom { (cx, b.y + b.h) } else { (cx, b.y) }
}

fn shape_svg(shape: Shape, b: &Box_, t: &Theme) -> String {
    let common = format!(
        r#"fill="{}" stroke="{}" stroke-width="1.4""#,
        t.fill, t.stroke
    );
    match shape {
        Shape::Rect => {
            format!(r#"<rect x="{}" y="{}" width="{}" height="{}" rx="3" {common}/>"#, f(b.x), f(b.y), f(b.w), f(b.h))
        }
        Shape::Rounded | Shape::Stadium => {
            let r = b.h / 2.0;
            format!(r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{}" {common}/>"#, f(b.x), f(b.y), f(b.w), f(b.h), f(r))
        }
        Shape::Diamond => {
            let cx = b.x + b.w / 2.0;
            let cy = b.y + b.h / 2.0;
            format!(
                r#"<polygon points="{},{} {},{} {},{} {},{}" {common}/>"#,
                f(cx), f(b.y), f(b.x + b.w), f(cy), f(cx), f(b.y + b.h), f(b.x), f(cy)
            )
        }
    }
}

/// Format a float compactly (no trailing zeros).
fn f(v: f64) -> String {
    let s = format!("{v:.1}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Render mermaid source to a self-contained inline `<svg>...</svg>` string.
///
/// # Errors
///
/// Propagates [`Diagram::parse`] failures.
pub fn render(source: &str) -> Result<String, Error> {
    render_with_theme(source, &Theme::default())
}

/// Render with an explicit [`Theme`].
///
/// # Errors
///
/// Propagates [`Diagram::parse`] failures.
pub fn render_with_theme(source: &str, t: &Theme) -> Result<String, Error> {
    let diag = Diagram::parse(source)?;
    let Laid { boxes, width, height } = layout(&diag, t);

    let mut out = String::with_capacity(2048);
    let pad = 8.0;
    out.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{} {} {} {}" width="{}" height="{}" role="img" class="diagram-svg">"#,
        f(-pad), f(-pad), f(width + pad * 2.0), f(height + pad * 2.0), f(width + pad * 2.0), f(height + pad * 2.0)
    ));
    // markers for arrowheads
    out.push_str(&format!(
        r#"<defs><marker id="arr" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="{}"/></marker></defs>"#,
        t.edge
    ));
    // edges first (under the nodes)
    for e in &diag.edges {
        let (a, b) = (&boxes[e.from], &boxes[e.to]);
        let dash = match e.style {
            EdgeStyle::Solid => "",
            EdgeStyle::Dotted => r#" stroke-dasharray="2 4""#,
            EdgeStyle::Thick => r#" stroke-width="2.4""#,
        };
        let (x1, y1) = anchor(a, true);
        let (x2, y2) = anchor(b, false);
        // two-segment route with a mid channel, so crossing layers is tidy
        let midy = f64::midpoint(y1, y2);
        let path = if (x1 - x2).abs() < 0.5 || y2 - y1 <= t.rank_gap + 1.0 {
            format!("M{},{} L{},{}", f(x1), f(y1), f(x2), f(y2))
        } else {
            format!("M{},{} L{},{} L{},{} L{},{}", f(x1), f(y1), f(x1), f(midy), f(x2), f(midy), f(x2), f(y2))
        };
        out.push_str(&format!(
            r#"<path d="{path}" fill="none" stroke="{}"{dash} marker-end="url(#arr)"/>"#,
            t.edge
        ));
        if let Some(label) = &e.label {
            let lw = label.chars().count() as f64 * t.edge_font_size * 0.62 + 8.0;
            let lx = f64::midpoint(x1, x2);
            let ly = midy;
            out.push_str(&format!(
                r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{}" opacity="0.92"/><text x="{}" y="{}" text-anchor="middle" dominant-baseline="central" font-family="{}" font-size="{}" fill="{}">{}</text>"#,
                f(lx - lw / 2.0), f(ly - 8.0), f(lw), f(16.0), t.fill,
                f(lx), f(ly), esc(&t.font_family), f(t.edge_font_size), t.edge_text, esc(label)
            ));
        }
    }
    // nodes
    for (i, nd) in diag.nodes.iter().enumerate() {
        let b = &boxes[i];
        out.push_str(&shape_svg(nd.shape, b, t));
        out.push_str(&format!(
            r#"<text x="{}" y="{}" text-anchor="middle" dominant-baseline="central" font-family="{}" font-size="{}" fill="{}">{}</text>"#,
            f(b.x + b.w / 2.0), f(b.y + b.h / 2.0), esc(&t.font_family), f(t.font_size), t.text, esc(&nd.label)
        ));
    }
    out.push_str("</svg>");
    Ok(out)
}
