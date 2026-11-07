# mmd-svg

Render a focused Mermaid flowchart subset to self-contained inline SVG, in Rust, with no
dependencies. Built for static-site generators: parse a ` ```mermaid ` block at build time,
emit one `<svg>` string. No scripts, no external references, deterministic output — so a
strict Content-Security-Policy can keep serving the page untouched.

## Supported syntax

- `flowchart TD` / `TB` / `LR` (plus `graph`); `RL`/`BT` render as LR
- Node shapes: `[rect]`, `(rounded)`, `{diamond}`, `((stadium))`, `[(cylinder)]`
- Edges: `-->`, `-.->` (dotted), `==>` (thick), chained on one line
- Edge labels: `A -->|text| B`
- Quoted labels: `A["text with (parens) & <tags>"]`
- `%%` comments

## Example

```rust
let svg = mmd_svg::render("flowchart TD\n A[Upload] --> B{TLS record} -->|bad mac| C[AP]")?;
```

## Layout

A small Sugiyama pipeline, the same family of algorithm mermaid uses through dagre:

1. DFS back edges are reversed so the graph is acyclic (they are drawn pointing the right way).
2. Longest-path ranking; sources move down to sit just above their children.
3. Edges spanning several ranks get dummy vertices; an edge label is a sized dummy in the
   middle rank, so labels get real space and never collide.
4. Barycenter sweeps reduce crossings, keeping the best ordering found.
5. Balanced placement: each layer moves toward its neighbours' mean, packed from both sides
   and averaged so spacing holds; the last sweep runs downward so chains line up.
6. Edges are smooth cubic curves through their dummy points, with ports spread when several
   edges share a node side.

Labels wrap at word breaks (`Theme::wrap_chars`); sizes assume a 0.6 em monospace advance.
The `<svg>` carries a `<title>` listing every edge as its text alternative.

## License

MIT OR Apache-2.0
