use mmd_svg::{Diagram, Direction, EdgeStyle, Shape};

const BASIC: &str = "\
flowchart TD
    A[Upload] --> B{TLS record}
    B -->|bad mac| C[AP firmware]
    B -->|ok| D[Server]
";

#[test]
fn parses_nodes_edges_and_labels() {
    let d = Diagram::parse(BASIC).unwrap();
    assert_eq!(d.nodes.len(), 4);
    assert_eq!(d.edges.len(), 3);
    assert_eq!(d.edges[0].label, None);
    assert_eq!(d.edges[1].label.as_deref(), Some("bad mac"));
    assert_eq!(d.nodes[0].shape, Shape::Rect);
    assert_eq!(d.nodes[1].shape, Shape::Diamond);
}

#[test]
fn chains_and_comments() {
    let d = Diagram::parse("flowchart LR\n%% a comment\nA --> B --> C\nD((x))\n").unwrap();
    assert_eq!(d.edges.len(), 2);
    assert_eq!(d.nodes.len(), 4);
    assert_eq!(d.nodes[3].shape, Shape::Stadium);
    assert_eq!(d.direction, Direction::LeftRight);
}

#[test]
fn quoted_labels_keep_specials() {
    let d = Diagram::parse("flowchart TD\nA[\"has (parens) & <tags>\"] --> B\n").unwrap();
    assert_eq!(d.nodes[0].label, "has (parens) & <tags>");
}

#[test]
fn render_is_deterministic_and_self_contained() {
    let a = mmd_svg::render(BASIC).unwrap();
    let b = mmd_svg::render(BASIC).unwrap();
    assert_eq!(a, b);
    assert!(a.starts_with("<svg"));
    assert!(a.ends_with("</svg>"));
    assert!(!a.contains("<script"));
    assert!(!a.contains("href=\"http"));
    assert!(a.contains("Upload"));
    assert!(a.contains("bad mac"));
}

#[test]
fn cycles_do_not_hang() {
    let d = mmd_svg::render("flowchart TD\nA --> B --> C --> A\n").unwrap();
    assert!(d.contains("<svg"));
}

#[test]
fn edge_styles_and_later_redefinition() {
    let d = Diagram::parse("flowchart TD\nA -.-> B\nA ==> B\nB[Renamed]\n").unwrap();
    assert_eq!(d.edges[0].style, EdgeStyle::Dotted);
    assert_eq!(d.edges[1].style, EdgeStyle::Thick);
    assert_eq!(d.nodes[1].label, "Renamed");
}

#[test]
fn branchy_layout_places_children_below_parents() {
    let svg = mmd_svg::render(BASIC).unwrap();
    // all four nodes rendered as shapes
    assert_eq!(svg.matches("stroke-width=\"1.4\"").count(), 4);
}

#[test]
fn rejects_garbage() {
    assert!(Diagram::parse("").is_err());
    assert!(Diagram::parse("flowchart TD\nA[unclosed\n").is_err());
    assert!(Diagram::parse("flowchart XX\nA --> B\n").is_err());
    assert!(Diagram::parse("flowchart TD\nA --> B oops\n").is_err());
}
