//! Drawing a mind map: SVG for files and the live page, PNG for Telegram.
//!
//! Layout is the classic two-sided mind map: the topic in the middle, its
//! branches split left and right by height, every subtree in its own band, so
//! no two nodes can overlap by construction. One column per depth, as wide as
//! its widest node. Branches keep one colour from the topic to their leaves;
//! a node's border says its kind, a cross-link is a dashed curve with its
//! label, a node's journal references are `#N` under its text.
//!
//! Text is measured with the font the PNG is drawn with (Noto Sans, embedded:
//! a server has no fonts to rely on), so boxes fit their text in the PNG and
//! closely enough in a browser.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

use crate::mindmap::{Map, Node};

/// Noto Sans Regular, SIL OFL 1.1 — see assets/fonts/OFL.txt.
pub static FONT: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const FONT_FAMILY: &str = "Noto Sans";

const TEXT_SIZE: f64 = 13.0;
const ROOT_SIZE: f64 = 17.0;
const SMALL_SIZE: f64 = 10.0;
const LINE: f64 = 18.0;
const ROOT_LINE: f64 = 23.0;
const SMALL_LINE: f64 = 14.0;
const PAD_X: f64 = 12.0;
const PAD_Y: f64 = 8.0;
const MAX_TEXT_W: f64 = 250.0;
const MAX_ROOT_W: f64 = 300.0;
const MAX_LINES: usize = 6;
const H_GAP: f64 = 64.0;
const V_GAP: f64 = 14.0;
const MARGIN: f64 = 32.0;
const HEADER: f64 = 56.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
    /// Light, switched to dark by `prefers-color-scheme` — for browsers.
    /// resvg does not evaluate media queries, so a PNG takes Light or Dark.
    Auto,
}

impl Theme {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "light" => Ok(Theme::Light),
            "dark" => Ok(Theme::Dark),
            "auto" => Ok(Theme::Auto),
            other => bail!("unknown theme `{other}` — light, dark or auto"),
        }
    }
}

struct Palette {
    bg: &'static str,
    surface: &'static str,
    text: &'static str,
    muted: &'static str,
    root_fill: &'static str,
    root_text: &'static str,
    link: &'static str,
    label_bg: &'static str,
}

const LIGHT: Palette = Palette {
    bg: "#f8fafc",
    surface: "#ffffff",
    text: "#0f172a",
    muted: "#64748b",
    root_fill: "#1e293b",
    root_text: "#f8fafc",
    link: "#475569",
    label_bg: "#f8fafc",
};

const DARK: Palette = Palette {
    bg: "#0b1120",
    surface: "#111827",
    text: "#e2e8f0",
    muted: "#94a3b8",
    root_fill: "#e2e8f0",
    root_text: "#0b1120",
    link: "#cbd5e1",
    label_bg: "#0b1120",
};

/// Branch colours, one per child of the topic, readable on both backgrounds.
const BRANCHES: [&str; 8] = ["#e8590c", "#1c7ed6", "#2f9e44", "#ae3ec9", "#f08c00", "#0c8599", "#d6336c", "#5c7cfa"];

fn kind_color(kind: &str, dark: bool) -> &'static str {
    match (kind, dark) {
        ("goal", false) => "#2563eb",
        ("goal", true) => "#60a5fa",
        ("fact", false) => "#059669",
        ("fact", true) => "#34d399",
        ("hypothesis", false) => "#d97706",
        ("hypothesis", true) => "#fbbf24",
        ("question", false) => "#7c3aed",
        ("question", true) => "#a78bfa",
        ("decision", false) => "#0f766e",
        ("decision", true) => "#2dd4bf",
        ("idea", false) => "#db2777",
        ("idea", true) => "#f472b6",
        ("task", false) => "#0891b2",
        ("task", true) => "#22d3ee",
        ("dead-end", false) => "#6b7280",
        ("dead-end", true) => "#9ca3af",
        (_, false) => "#64748b",
        (_, true) => "#94a3b8",
    }
}

fn face() -> &'static ttf_parser::Face<'static> {
    static FACE: OnceLock<ttf_parser::Face<'static>> = OnceLock::new();
    FACE.get_or_init(|| ttf_parser::Face::parse(FONT, 0).expect("embedded Noto Sans parses"))
}

/// Width of `text` at `size` px in the embedded font. A character the font
/// lacks (CJK, emoji) counts as a full em — wide rather than clipped.
pub fn text_width(text: &str, size: f64) -> f64 {
    let f = face();
    let upem = f.units_per_em() as f64;
    text.chars()
        .map(|c| match f.glyph_index(c).and_then(|g| f.glyph_hor_advance(g)) {
            Some(adv) => adv as f64 / upem * size,
            None => size,
        })
        .sum()
}

/// Greedy word wrap to `max` px; a word wider than a line is cut. At most
/// MAX_LINES lines, the last one ending in "…" when text was left over.
fn wrap(text: &str, size: f64, max: f64) -> Vec<String> {
    // Widths add up char by char, so each word is measured once.
    let space = text_width(" ", size);
    let mut lines: Vec<String> = Vec::new();
    let (mut cur, mut cur_w) = (String::new(), 0.0);
    let mut truncated = false;
    'outer: for word in text.split_whitespace() {
        let mut word = word.to_string();
        let mut word_w = text_width(&word, size);
        loop {
            let add = if cur.is_empty() { word_w } else { space + word_w };
            if cur_w + add <= max {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(&word);
                cur_w += add;
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            } else {
                // One word wider than the line: cut it.
                let (mut head, mut head_w) = (String::new(), 0.0);
                for c in word.chars() {
                    let cw = text_width(c.encode_utf8(&mut [0; 4]), size);
                    if head_w + cw > max && !head.is_empty() {
                        break;
                    }
                    head.push(c);
                    head_w += cw;
                }
                word = word[head.len()..].to_string();
                word_w -= head_w;
                lines.push(head);
                if word.is_empty() {
                    break;
                }
            }
            if lines.len() >= MAX_LINES {
                truncated = true;
                break 'outer;
            }
        }
    }
    if !cur.is_empty() {
        if lines.len() >= MAX_LINES {
            truncated = true;
        } else {
            lines.push(cur);
        }
    }
    lines.truncate(MAX_LINES);
    if truncated {
        if let Some(last) = lines.last_mut() {
            let ell = text_width("…", size);
            let mut w = text_width(last, size);
            while let Some(c) = last.chars().last() {
                if w + ell <= max {
                    break;
                }
                w -= text_width(c.encode_utf8(&mut [0; 4]), size);
                last.pop();
            }
            last.push('…');
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// One node's box, centred at (x, y).
#[derive(Debug, Clone)]
pub struct Placed {
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// -1 left of the topic, 1 right, 0 the topic.
    pub side: i8,
    pub depth: usize,
    /// Index into BRANCHES of the topic's child this node descends from.
    pub branch: usize,
    lines: Vec<String>,
    tag: String,
    refs: String,
}

impl Placed {
    pub fn left(&self) -> f64 {
        self.x - self.w / 2.0
    }
    pub fn right(&self) -> f64 {
        self.x + self.w / 2.0
    }
    pub fn top(&self) -> f64 {
        self.y - self.h / 2.0
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h / 2.0
    }
}

fn measure(n: &Node, root: bool) -> Placed {
    let (size, max) = if root { (ROOT_SIZE, MAX_ROOT_W) } else { (TEXT_SIZE, MAX_TEXT_W) };
    let lines = wrap(&n.text, size, max);
    let tag = if root {
        String::new()
    } else if n.state == "open" {
        n.kind.to_uppercase()
    } else {
        format!("{} · {}", n.kind.to_uppercase(), n.state.to_uppercase())
    };
    let refs = n.refs.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(" ");
    let line_h = if root { ROOT_LINE } else { LINE };
    let mut w = lines.iter().map(|l| text_width(l, size)).fold(0.0, f64::max);
    w = w.max(text_width(&tag, SMALL_SIZE)).max(text_width(&refs, SMALL_SIZE)).max(40.0);
    let mut h = lines.len() as f64 * line_h;
    if !tag.is_empty() {
        h += SMALL_LINE;
    }
    if !refs.is_empty() {
        h += SMALL_LINE;
    }
    Placed {
        id: n.id,
        x: 0.0,
        y: 0.0,
        w: w + 2.0 * PAD_X,
        h: h + 2.0 * PAD_Y,
        side: 0,
        depth: 0,
        branch: 0,
        lines,
        tag,
        refs,
    }
}

/// Where every node goes. Coordinates are relative to the topic at (0, 0).
pub fn layout(map: &Map) -> Vec<Placed> {
    let mut placed: HashMap<i64, Placed> =
        map.nodes.iter().map(|n| (n.id, measure(n, n.id == map.root))).collect();
    let kids: HashMap<i64, Vec<i64>> = map.nodes.iter().fold(HashMap::new(), |mut acc, n| {
        if let Some(p) = n.parent {
            acc.entry(p).or_default().push(n.id);
        }
        acc
    });
    let children = |id: i64| kids.get(&id).cloned().unwrap_or_default();

    // Height of each subtree's band.
    fn band(id: i64, placed: &HashMap<i64, Placed>, children: &dyn Fn(i64) -> Vec<i64>, memo: &mut HashMap<i64, f64>) -> f64 {
        if let Some(h) = memo.get(&id) {
            return *h;
        }
        let ks = children(id);
        let own = placed[&id].h;
        let sum: f64 = ks.iter().map(|k| band(*k, placed, children, memo)).sum::<f64>()
            + V_GAP * ks.len().saturating_sub(1) as f64;
        let h = own.max(sum);
        memo.insert(id, h);
        h
    }
    let mut memo = HashMap::new();
    let top = children(map.root);
    for k in &top {
        band(*k, &placed, &children, &mut memo);
    }

    // Split the topic's children: in order, each to the shorter side.
    let (mut right, mut left): (Vec<i64>, Vec<i64>) = (vec![], vec![]);
    let (mut rh, mut lh) = (0.0, 0.0);
    for k in &top {
        if rh <= lh {
            rh += memo[k] + V_GAP;
            right.push(*k);
        } else {
            lh += memo[k] + V_GAP;
            left.push(*k);
        }
    }
    let branch_of: HashMap<i64, usize> = top.iter().enumerate().map(|(i, k)| (*k, i % BRANCHES.len())).collect();

    // Depth, side and branch of every node; widest node per (side, depth).
    let mut widest: HashMap<(i8, usize), f64> = HashMap::new();
    let mut stack: Vec<(i64, i8, usize, usize)> = right
        .iter()
        .map(|k| (*k, 1i8, 1usize, branch_of[k]))
        .chain(left.iter().map(|k| (*k, -1i8, 1usize, branch_of[k])))
        .collect();
    while let Some((id, side, depth, branch)) = stack.pop() {
        let p = placed.get_mut(&id).unwrap();
        p.side = side;
        p.depth = depth;
        p.branch = branch;
        let w = widest.entry((side, depth)).or_insert(0.0);
        *w = w.max(p.w);
        for k in children(id) {
            stack.push((k, side, depth + 1, branch));
        }
    }
    let root_w = placed[&map.root].w;
    // x of each column's centre.
    let column_x = |side: i8, depth: usize| -> f64 {
        let mut edge = root_w / 2.0 + H_GAP;
        for d in 1..depth {
            edge += widest.get(&(side, d)).copied().unwrap_or(0.0) + H_GAP;
        }
        let w = widest.get(&(side, depth)).copied().unwrap_or(0.0);
        side as f64 * (edge + w / 2.0)
    };
    // y: each subtree gets its band, centred on its children.
    fn place(
        id: i64,
        top_y: f64,
        placed: &mut HashMap<i64, Placed>,
        children: &dyn Fn(i64) -> Vec<i64>,
        memo: &HashMap<i64, f64>,
        column_x: &dyn Fn(i8, usize) -> f64,
    ) {
        let band = memo[&id];
        let ks = children(id);
        let kids_h: f64 = ks.iter().map(|k| memo[k]).sum::<f64>() + V_GAP * ks.len().saturating_sub(1) as f64;
        let mut y = top_y + (band - kids_h) / 2.0;
        for k in &ks {
            place(*k, y, placed, children, memo, column_x);
            y += memo[k] + V_GAP;
        }
        let p = placed.get_mut(&id).unwrap();
        p.y = top_y + band / 2.0;
        p.x = column_x(p.side, p.depth);
    }
    for side in [&right, &left] {
        let total: f64 = side.iter().map(|k| memo[k]).sum::<f64>() + V_GAP * side.len().saturating_sub(1) as f64;
        let mut y = -total / 2.0;
        for k in side.iter() {
            place(*k, y, &mut placed, &children, &memo, &column_x);
            y += memo[k] + V_GAP;
        }
    }
    map.nodes.iter().map(|n| placed.remove(&n.id).unwrap()).collect()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn n(v: f64) -> String {
    format!("{:.1}", v)
}

/// The map as a standalone SVG document.
pub fn render_svg(map: &Map, theme: Theme) -> String {
    let dark = theme == Theme::Dark;
    let pal = if dark { &DARK } else { &LIGHT };
    let boxes = layout(map);
    let by_id: HashMap<i64, &Placed> = boxes.iter().map(|b| (b.id, b)).collect();

    let chains: Vec<String> = map
        .chains
        .iter()
        .map(|c| format!("{}: {}", c.name, c.entries.iter().map(|e| format!("#{e}")).collect::<Vec<_>>().join(" › ")))
        .collect();
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for b in &boxes {
        min_x = min_x.min(b.left());
        max_x = max_x.max(b.right());
        min_y = min_y.min(b.top());
        max_y = max_y.max(b.bottom());
    }
    let s = &map.summary;
    let title = format!("{} — mind map #{}", s.title, s.id);
    let subtitle = format!("{} · owner {} · {} nodes, {} open", s.state, s.owner_nick, s.nodes, s.open_nodes);
    let chain_h = if chains.is_empty() { 0.0 } else { 24.0 + chains.len() as f64 * 18.0 };
    let content_w = (max_x - min_x)
        .max(text_width(&title, 18.0))
        .max(chains.iter().map(|c| text_width(c, 12.0)).fold(0.0, f64::max));
    let width = content_w + 2.0 * MARGIN;
    let height = HEADER + (max_y - min_y) + chain_h + 2.0 * MARGIN;
    // Shift so the map's left edge sits at the margin, centred if the title is wider.
    let dx = MARGIN - min_x + (content_w - (max_x - min_x)) / 2.0;
    let dy = MARGIN + HEADER - min_y;

    let mut o = String::with_capacity(16 * 1024);
    o.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"{FONT_FAMILY}, 'Segoe UI', system-ui, sans-serif\">\n",
        w = n(width),
        h = n(height)
    ));
    o.push_str(&format!("<title>{}</title>\n", esc(&title)));
    if theme == Theme::Auto {
        // Browsers only: presentation attributes carry the light theme, and
        // these rules (which outrank attributes) switch to dark.
        o.push_str("<style>@media (prefers-color-scheme: dark){");
        o.push_str(&format!(".bg{{fill:{}}} .surface{{fill:{}}} .tx{{fill:{}}} .mu{{fill:{}}} ", DARK.bg, DARK.surface, DARK.text, DARK.muted));
        o.push_str(&format!(".rootbox{{fill:{}}} .roottx{{fill:{}}} .xl{{stroke:{}}} .xlarrow{{fill:{}}} .lbg{{fill:{}}} ", DARK.root_fill, DARK.root_text, DARK.link, DARK.link, DARK.label_bg));
        for k in crate::mindmap::NODE_KINDS {
            let c = kind_color(k, true);
            o.push_str(&format!(".k-{k} .border{{stroke:{c}}} .k-{k} .tag{{fill:{c}}} "));
        }
        o.push_str("}</style>\n");
    }
    o.push_str(&format!(
        "<defs><marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\"><path class=\"xlarrow\" d=\"M0,0 L10,5 L0,10 z\" fill=\"{}\"/></marker></defs>\n",
        pal.link
    ));
    o.push_str(&format!("<rect class=\"bg\" width=\"100%\" height=\"100%\" fill=\"{}\"/>\n", pal.bg));
    o.push_str(&format!(
        "<text class=\"tx\" x=\"{}\" y=\"{}\" font-size=\"18\" fill=\"{}\">{}</text>\n",
        n(MARGIN),
        n(MARGIN + 18.0),
        pal.text,
        esc(&title)
    ));
    o.push_str(&format!(
        "<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"12\" fill=\"{}\">{}</text>\n",
        n(MARGIN),
        n(MARGIN + 38.0),
        pal.muted,
        esc(&subtitle)
    ));

    // Tree edges, under the boxes.
    o.push_str("<g fill=\"none\" stroke-linecap=\"round\">\n");
    for node in &map.nodes {
        let Some(parent) = node.parent else { continue };
        let (c, p) = (by_id[&node.id], by_id[&parent]);
        let side = c.side as f64;
        let (x1, y1) = (if p.side == 0 { p.x + side * p.w / 2.0 } else { p.x + side * p.w / 2.0 }, p.y);
        let (x2, y2) = (c.x - side * c.w / 2.0, c.y);
        let mx = (x1 + x2) / 2.0;
        let width = (3.2 - c.depth as f64 * 0.5).max(1.2);
        let dim = if node.state == "dropped" { " stroke-opacity=\"0.4\"" } else { "" };
        o.push_str(&format!(
            "<path d=\"M{} {} C{} {} {} {} {} {}\" stroke=\"{}\" stroke-width=\"{}\"{dim}/>\n",
            n(x1 + dx), n(y1 + dy), n(mx + dx), n(y1 + dy), n(mx + dx), n(y2 + dy), n(x2 + dx), n(y2 + dy),
            BRANCHES[c.branch], n(width)
        ));
    }
    o.push_str("</g>\n");

    // Nodes.
    for node in &map.nodes {
        let b = by_id[&node.id];
        let root = node.id == map.root;
        let (x, y) = (b.left() + dx, b.top() + dy);
        let kc = kind_color(&node.kind, dark);
        let dropped = node.state == "dropped";
        o.push_str(&format!(
            "<g class=\"node k-{}\" data-id=\"{}\"{}>\n",
            esc(&node.kind),
            node.id,
            if dropped { " opacity=\"0.5\"" } else { "" }
        ));
        if root {
            o.push_str(&format!(
                "<rect class=\"rootbox\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"14\" fill=\"{}\"/>\n",
                n(x), n(y), n(b.w), n(b.h), pal.root_fill
            ));
        } else {
            let dash = if dropped { " stroke-dasharray=\"5 4\"" } else { "" };
            let sw = if node.state == "done" { "2.5" } else { "1.6" };
            o.push_str(&format!(
                "<rect class=\"surface border\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"9\" fill=\"{}\" stroke=\"{kc}\" stroke-width=\"{sw}\"{dash}/>\n",
                n(x), n(y), n(b.w), n(b.h), pal.surface
            ));
            // The branch colour as a stripe on the side facing the topic.
            let sx = if b.side > 0 { x } else { x + b.w - 4.0 };
            o.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"4\" height=\"{}\" rx=\"2\" fill=\"{}\"/>\n",
                n(sx), n(y + 6.0), n(b.h - 12.0), BRANCHES[b.branch]
            ));
        }
        let mut ty = y + PAD_Y;
        if !b.tag.is_empty() {
            ty += SMALL_LINE - 3.0;
            o.push_str(&format!(
                "<text class=\"tag\" x=\"{}\" y=\"{}\" font-size=\"{SMALL_SIZE}\" letter-spacing=\"0.6\" fill=\"{kc}\">{}</text>\n",
                n(x + PAD_X), n(ty), esc(&b.tag)
            ));
            ty += 3.0;
        }
        let (size, line_h, fill, class) = if root {
            (ROOT_SIZE, ROOT_LINE, pal.root_text, "roottx")
        } else {
            (TEXT_SIZE, LINE, pal.text, "tx")
        };
        let deco = if dropped { " text-decoration=\"line-through\"" } else { "" };
        for line in &b.lines {
            ty += line_h;
            let tx = if root { x + b.w / 2.0 } else { x + PAD_X };
            let anchor = if root { " text-anchor=\"middle\"" } else { "" };
            o.push_str(&format!(
                "<text class=\"{class}\" x=\"{}\" y=\"{}\" font-size=\"{size}\" fill=\"{fill}\"{anchor}{deco}>{}</text>\n",
                n(tx), n(ty - 5.0), esc(line)
            ));
        }
        if !b.refs.is_empty() {
            ty += SMALL_LINE;
            o.push_str(&format!(
                "<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"{SMALL_SIZE}\" fill=\"{}\">{}</text>\n",
                n(x + PAD_X), n(ty - 3.0), pal.muted, esc(&b.refs)
            ));
        }
        o.push_str("</g>\n");
    }

    // Cross-links, over the boxes.
    for e in &map.edges {
        let (Some(a), Some(c)) = (by_id.get(&e.from), by_id.get(&e.to)) else { continue };
        let (x1, y1) = anchor(a, c);
        let (x2, y2) = anchor(c, a);
        let (mx, my) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
        let (vx, vy) = (x2 - x1, y2 - y1);
        let len = (vx * vx + vy * vy).sqrt().max(1.0);
        // Neighbours are close: bend enough that the link shows beside them.
        let bend = (len * 0.25).clamp(40.0, 90.0);
        let (cx, cy) = (mx - vy / len * bend, my + vx / len * bend);
        o.push_str(&format!(
            "<path class=\"xl\" d=\"M{} {} Q{} {} {} {}\" fill=\"none\" stroke=\"{}\" stroke-width=\"1.5\" stroke-dasharray=\"6 4\" marker-end=\"url(#arrow)\"/>\n",
            n(x1 + dx), n(y1 + dy), n(cx + dx), n(cy + dy), n(x2 + dx), n(y2 + dy), pal.link
        ));
        let mut label = e.label.clone();
        if !e.refs.is_empty() {
            let refs = e.refs.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(" ");
            label = if label.is_empty() { refs } else { format!("{label} {refs}") };
        }
        if !label.is_empty() {
            // The curve's midpoint is halfway between the chord and the control point.
            let (lx, ly) = ((mx + cx) / 2.0 + dx, (my + cy) / 2.0 + dy);
            let lw = text_width(&label, 11.0) + 10.0;
            o.push_str(&format!(
                "<rect class=\"lbg\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"16\" rx=\"4\" fill=\"{}\" opacity=\"0.9\"/>\n",
                n(lx - lw / 2.0), n(ly - 9.0), n(lw), pal.label_bg
            ));
            o.push_str(&format!(
                "<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"11\" text-anchor=\"middle\" fill=\"{}\">{}</text>\n",
                n(lx), n(ly + 3.0), pal.muted, esc(&label)
            ));
        }
    }

    if !chains.is_empty() {
        let mut y = dy + max_y + 30.0;
        o.push_str(&format!(
            "<text class=\"tx\" x=\"{}\" y=\"{}\" font-size=\"13\" fill=\"{}\">Chains</text>\n",
            n(MARGIN), n(y), pal.text
        ));
        for c in &chains {
            y += 18.0;
            o.push_str(&format!(
                "<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"12\" fill=\"{}\">{}</text>\n",
                n(MARGIN), n(y), pal.muted, esc(c)
            ));
        }
    }
    o.push_str("</svg>\n");
    o
}

/// Where a link leaves box `a` towards box `b`: the border point on the line
/// between their centres.
fn anchor(a: &Placed, b: &Placed) -> (f64, f64) {
    let (vx, vy) = (b.x - a.x, b.y - a.y);
    if vx == 0.0 && vy == 0.0 {
        return (a.x, a.y);
    }
    let tx = if vx != 0.0 { (a.w / 2.0) / vx.abs() } else { f64::INFINITY };
    let ty = if vy != 0.0 { (a.h / 2.0) / vy.abs() } else { f64::INFINITY };
    let t = tx.min(ty);
    (a.x + vx * t, a.y + vy * t)
}

fn options() -> resvg::usvg::Options<'static> {
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_font_data(FONT.to_vec());
    opt.font_family = FONT_FAMILY.to_string();
    opt
}

/// Parse an SVG the way the PNG path does. Errors mean a broken document.
pub fn parse(svg: &str) -> Result<resvg::usvg::Tree> {
    resvg::usvg::Tree::from_str(svg, &options()).context("parse the mind-map SVG")
}

/// The longest side a PNG gets, and Telegram's own limit on a photo
/// (width + height at most 10000, sides at most 20:1).
pub const PNG_MAX_SIDE: f64 = 4096.0;

/// Rasterise for Telegram: 2x for a small map, scaled down to fit a large one.
pub fn render_png(map: &Map, theme: Theme) -> Result<Vec<u8>> {
    let theme = if theme == Theme::Auto { Theme::Light } else { theme };
    let tree = parse(&render_svg(map, theme))?;
    let size = tree.size();
    let (w, h) = (size.width() as f64, size.height() as f64);
    let scale = (2.0f64).min(PNG_MAX_SIDE / w.max(h)).min(9_000.0 / (w + h));
    let (pw, ph) = ((w * scale).ceil().max(1.0) as u32, (h * scale).ceil().max(1.0) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pw, ph).context("allocate the mind-map image")?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale as f32, scale as f32), &mut pixmap.as_mut());
    pixmap.encode_png().context("encode the mind-map PNG")
}

/// Whether Telegram takes this PNG as a photo (else it goes as a document).
pub fn fits_telegram_photo(png: &[u8]) -> bool {
    // PNG IHDR: width and height big-endian at bytes 16..24.
    if png.len() < 24 {
        return false;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]) as f64;
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]) as f64;
    w + h <= 10_000.0 && w.max(h) / w.min(h).max(1.0) <= 20.0 && png.len() <= 10 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mindmap::{Chain, Edge, MapSummary};

    fn map_of(n_nodes: usize) -> Map {
        let mut nodes = vec![Node { id: 1, parent: None, kind: "topic".into(), text: "Почему пул тормозит под нагрузкой".into(), state: "open".into(), refs: vec![] }];
        let kinds = ["fact", "hypothesis", "question", "decision", "idea", "task", "dead-end", "goal", "note"];
        for i in 2..=n_nodes as i64 {
            // A bushy tree: parent among the earlier nodes, deeper as it grows.
            let parent = if i <= 6 { 1 } else { 2 + (i * 7919) % (i - 2) };
            nodes.push(Node {
                id: i,
                parent: Some(parent),
                kind: kinds[(i as usize) % kinds.len()].into(),
                text: format!("узел {i}: lock contention on the bus writer, measured under 40 rps"),
                state: ["open", "done", "dropped"][(i as usize) % 3].into(),
                refs: if i % 4 == 0 { vec![i, i + 100] } else { vec![] },
            });
        }
        // Tree order the renderer expects: parents first.
        let summary = MapSummary {
            id: 7,
            title: nodes[0].text.clone(),
            owner_nick: "mira".into(),
            owner_host: None,
            state: "open".into(),
            nodes: n_nodes as i64,
            open_nodes: 0,
            created_at: 0,
            updated_at: 0,
        };
        let edges = if n_nodes > 5 {
            vec![Edge { id: 1, from: 3, to: 5, label: "противоречит".into(), refs: vec![12] }]
        } else {
            vec![]
        };
        Map { summary, root: 1, nodes, edges, chains: vec![Chain { id: 1, name: "фикс".into(), entries: vec![1, 2, 3] }] }
    }

    fn overlap(a: &Placed, b: &Placed) -> bool {
        a.left() < b.right() && b.left() < a.right() && a.top() < b.bottom() && b.top() < a.bottom()
    }

    #[test]
    fn the_font_covers_latin_and_cyrillic() {
        // Every symbol the drawing itself writes, too: a missing one is a box.
        for c in "Hello, Привет — ёЁ жЖ щЩ · … ›".chars().filter(|c| !c.is_whitespace()) {
            assert!(face().glyph_index(c).is_some(), "no glyph for {c:?}");
        }
        assert!(text_width("Привет", 13.0) > 20.0);
    }

    #[test]
    fn wrapping_keeps_lines_inside_the_box_and_cuts_overflow() {
        let long = "слово ".repeat(200);
        let lines = wrap(&long, TEXT_SIZE, MAX_TEXT_W);
        assert_eq!(lines.len(), MAX_LINES);
        assert!(lines.last().unwrap().ends_with('…'));
        assert!(lines.iter().all(|l| text_width(l, TEXT_SIZE) <= MAX_TEXT_W + 0.01));
        let word = "x".repeat(400);
        assert!(wrap(&word, TEXT_SIZE, MAX_TEXT_W).iter().all(|l| text_width(l, TEXT_SIZE) <= MAX_TEXT_W + 0.01));
    }

    #[test]
    fn no_two_boxes_overlap() {
        for size in [1, 30, 300] {
            let boxes = layout(&map_of(size));
            assert_eq!(boxes.len(), size);
            for (i, a) in boxes.iter().enumerate() {
                for b in &boxes[i + 1..] {
                    assert!(!overlap(a, b), "{size} nodes: {} overlaps {}", a.id, b.id);
                }
            }
        }
    }

    #[test]
    fn the_svg_parses_and_draws_text_for_every_theme() {
        for size in [1, 30, 300] {
            for theme in [Theme::Light, Theme::Dark, Theme::Auto] {
                let svg = render_svg(&map_of(size), theme);
                let tree = parse(&svg).unwrap_or_else(|e| panic!("{size} nodes {theme:?}: {e:#}"));
                assert!(tree.root().has_children());
            }
        }
        let svg = render_svg(&map_of(30), Theme::Auto);
        assert!(svg.contains("prefers-color-scheme: dark"));
        assert!(svg.contains("противоречит"));
    }

    #[test]
    fn cyrillic_text_is_drawn_not_dropped() {
        // usvg turns text into outlines with the fonts it has; a glyph the
        // font lacks would leave nothing to draw.
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"200\" height=\"40\"><text x=\"4\" y=\"28\" font-size=\"20\" font-family=\"{FONT_FAMILY}\">Привет</text></svg>"
        );
        let tree = parse(&svg).unwrap();
        let bbox = tree.root().abs_bounding_box();
        assert!(bbox.width() > 40.0, "text was not drawn: {bbox:?}");
    }

    /// `MINDMAP_SAMPLE_DIR=/some/dir cargo test --lib mindmap_svg::tests::sample -- --ignored`
    /// writes sample drawings to look at.
    #[test]
    #[ignore]
    fn sample() {
        let dir = std::path::PathBuf::from(std::env::var("MINDMAP_SAMPLE_DIR").expect("MINDMAP_SAMPLE_DIR"));
        for size in [12, 40] {
            let map = map_of(size);
            std::fs::write(dir.join(format!("map-{size}.svg")), render_svg(&map, Theme::Auto)).unwrap();
            std::fs::write(dir.join(format!("map-{size}-light.png")), render_png(&map, Theme::Light).unwrap()).unwrap();
            std::fs::write(dir.join(format!("map-{size}-dark.png")), render_png(&map, Theme::Dark).unwrap()).unwrap();
        }
    }

    #[test]
    fn png_renders_fast_enough_and_fits_telegram() {
        let map = map_of(300);
        let t = std::time::Instant::now();
        let _svg = render_svg(&map, Theme::Light);
        let svg_ms = t.elapsed().as_millis();
        assert!(svg_ms < 200, "SVG of 300 nodes took {svg_ms} ms");
        let png = render_png(&map, Theme::Light).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert!(fits_telegram_photo(&png), "300 nodes: {} bytes", png.len());
        let small = render_png(&map_of(5), Theme::Dark).unwrap();
        assert!(fits_telegram_photo(&small));
    }
}
