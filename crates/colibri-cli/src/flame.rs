//! A small flame graph renderer: folded stacks in, one SVG out.
//!
//! The graph grows upward from a root frame named `all`. A frame is as wide as the weight of
//! the stacks that pass through it, and its children sit above it in name order, so the same
//! profile always draws the same picture. Every frame is one `<g>` with a tooltip (`<title>`),
//! a rectangle and, when it is wide enough, a label. Frames narrower than a tenth of a pixel are
//! left out.

use std::fmt::Write;

const WIDTH: f64 = 1200.0;
const MARGIN: f64 = 10.0;
const ROW: f64 = 16.0;
const TITLE_SPACE: f64 = 34.0;
/// Characters of the 12 px monospace font are about this wide.
const CHAR_WIDTH: f64 = 7.2;
const MIN_WIDTH: f64 = 0.1;

struct Node<'a> {
    name: &'a str,
    weight: u64,
    children: Vec<usize>,
}

/// The tree of a list of stacks (outermost frame first) with their weights; node 0 is the root.
fn build<'a>(stacks: &[(Vec<&'a str>, u64)]) -> Vec<Node<'a>> {
    let mut nodes = vec![Node {
        name: "all",
        weight: 0,
        children: Vec::new(),
    }];
    for (stack, weight) in stacks {
        nodes[0].weight += weight;
        let mut at = 0;
        for &name in stack {
            let found = nodes[at]
                .children
                .iter()
                .copied()
                .find(|&c| nodes[c].name == name);
            let next = match found {
                Some(c) => c,
                None => {
                    nodes.push(Node {
                        name,
                        weight: 0,
                        children: Vec::new(),
                    });
                    let id = nodes.len() - 1;
                    nodes[at].children.push(id);
                    id
                }
            };
            nodes[next].weight += weight;
            at = next;
        }
    }
    for i in 0..nodes.len() {
        let mut kids = std::mem::take(&mut nodes[i].children);
        kids.sort_by(|&a, &b| nodes[a].name.cmp(nodes[b].name));
        nodes[i].children = kids;
    }
    nodes
}

fn depth(nodes: &[Node<'_>]) -> usize {
    let mut deepest = 0;
    let mut todo = vec![(0usize, 0usize)];
    while let Some((n, d)) = todo.pop() {
        deepest = deepest.max(d);
        todo.extend(nodes[n].children.iter().map(|&c| (c, d + 1)));
    }
    deepest
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// A warm colour that depends only on the name, as `hsl`.
fn colour(name: &str) -> String {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(16777619);
    }
    let hue = 5 + h % 50;
    let light = 55 + (h >> 8) % 20;
    format!("hsl({hue},80%,{light}%)")
}

/// The label that fits in `width` pixels: the name, cut short with `..`, or nothing.
fn label(name: &str, width: f64) -> Option<String> {
    let room = ((width - 6.0) / CHAR_WIDTH).floor();
    if room < 3.0 {
        return None;
    }
    let room = room as usize;
    let count = name.chars().count();
    if count <= room {
        return Some(name.to_string());
    }
    let mut cut: String = name.chars().take(room - 2).collect();
    cut.push_str("..");
    Some(cut)
}

/// What the profile weighs, for the tooltips: `unit` follows the number (`steps`, `ms`).
pub struct Units {
    pub unit: &'static str,
    /// The weight divided by this is what is shown.
    pub divisor: f64,
}

fn show(weight: u64, units: &Units) -> String {
    let v = weight as f64 / units.divisor;
    if units.divisor == 1.0 {
        format!("{weight} {}", units.unit)
    } else {
        format!("{v:.2} {}", units.unit)
    }
}

/// Draws `stacks` (outermost frame first, with weights) as an SVG document.
pub fn render(title: &str, stacks: &[(Vec<&str>, u64)], units: &Units) -> String {
    let nodes = build(stacks);
    let total = nodes[0].weight.max(1) as f64;
    let rows = depth(&nodes) + 1;
    let plot = WIDTH - 2.0 * MARGIN;
    let height = TITLE_SPACE + rows as f64 * ROW + MARGIN;
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{WIDTH}\" height=\"{height}\" viewBox=\"0 0 {WIDTH} {height}\" font-family=\"monospace\" font-size=\"12\">"
    );
    let _ = writeln!(
        svg,
        "<style>.bg {{ fill: #fafafa; }} .title {{ fill: #222; font-size: 15px; }} .f text {{ fill: #111; }} .f rect {{ stroke: #fafafa; stroke-width: 0.5; }} .f:hover rect {{ stroke: #222; }} @media (prefers-color-scheme: dark) {{ .bg {{ fill: #1b1b1b; }} .title {{ fill: #ddd; }} .f rect {{ stroke: #1b1b1b; }} .f:hover rect {{ stroke: #fff; }} }}</style>"
    );
    let _ = writeln!(svg, "<rect class=\"bg\" width=\"100%\" height=\"100%\"/>");
    let _ = writeln!(
        svg,
        "<text class=\"title\" x=\"{MARGIN}\" y=\"22\">{}</text>",
        escape(title)
    );
    // Depth first with an explicit stack: a profile of deep recursion has deep stacks.
    let mut todo = vec![(0usize, 0usize, MARGIN)];
    while let Some((n, level, x)) = todo.pop() {
        let node = &nodes[n];
        let width = node.weight as f64 / total * plot;
        if width < MIN_WIDTH {
            continue;
        }
        let y = height - MARGIN - (level + 1) as f64 * ROW;
        let percent = node.weight as f64 / total * 100.0;
        let _ = write!(
            svg,
            "<g class=\"f\"><title>{} ({}, {percent:.2}%)</title><rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{width:.2}\" height=\"{:.2}\" fill=\"{}\"/>",
            escape(node.name),
            show(node.weight, units),
            ROW - 1.0,
            colour(node.name)
        );
        if let Some(text) = label(node.name, width) {
            let _ = write!(
                svg,
                "<text x=\"{:.2}\" y=\"{:.2}\">{}</text>",
                x + 3.0,
                y + ROW - 5.0,
                escape(&text)
            );
        }
        let _ = writeln!(svg, "</g>");
        let mut at = x;
        for &c in &node.children {
            todo.push((c, level + 1, at));
            at += nodes[c].weight as f64 / total * plot;
        }
    }
    svg.push_str("</svg>\n");
    svg
}
