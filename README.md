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

Ranked layers (longest-path ranking with cycle breaking), barycenter ordering sweeps, and
per-layer centering. Nodes are sized from a conservative monospace advance estimate.

## License

MIT OR Apache-2.0
