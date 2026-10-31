//! Drawing a flowchart map (journal#107): ISO 5807 symbols and logic gates
//! joined by directed, labelled arrows.
//!
//! Layout is layered (Sugiyama, kept small): a depth-first pass marks the
//! arrows that close a cycle (a loop back to an earlier step) and layering
//! treats them as reversed; every symbol goes in the layer after the longest
//! path to it; within a layer, symbols are ordered by the mean position of
//! their predecessors (a few sweeps both ways) and packed with a fixed gap, so
//! two symbols never overlap. `down` stacks layers top to bottom, `right` left
//! to right. A forward arrow leaves its symbol on the far side and enters the
//! next on the near side, ports spread along the side by where the other end
//! is; a decision leaves through its vertices. A loop-back arrow runs beside
//! the layers. A gate is drawn in its distinctive (ANSI/IEEE 91) shape with
//! its inputs on the near side, its caption beside it.

use std::collections::HashMap;

use crate::mindmap::{Map, Node, GATES};
use crate::mindmap_svg::{esc, n, text_width, wrap, Palette, Theme, DARK, FONT_FAMILY, HEADER, LIGHT, MARGIN};

const TEXT: f64 = 13.0;
const LINE: f64 = 18.0;
const SMALL: f64 = 10.0;
const SMALL_LINE: f64 = 14.0;
const PAD_X: f64 = 14.0;
const PAD_Y: f64 = 10.0;
const MAX_W: f64 = 190.0;
const GAP: f64 = 36.0;
const LAYER_GAP: f64 = 56.0;
/// A gate symbol, drawn facing right: inputs at x = 0, output at x = GATE_W.
const GATE_W: f64 = 64.0;
const GATE_H: f64 = 44.0;

fn kind_color(kind: &str, dark: bool) -> &'static str {
    let (light, dk) = match kind {
        "start" | "end" => ("#2f9e44", "#51cf66"),
        "process" | "subprocess" => ("#1c7ed6", "#4dabf7"),
        "decision" => ("#e67700", "#fcc419"),
        "io" | "manual" => ("#7048e8", "#9775fa"),
        "document" | "data" => ("#0c8599", "#3bc9db"),
        "prep" | "delay" | "connector" => ("#495057", "#adb5bd"),
        k if GATES.contains(&k) => ("#c2255c", "#f06595"),
        _ => ("#64748b", "#94a3b8"),
    };
    if dark {
        dk
    } else {
        light
    }
}

/// One symbol placed. (cx, cy) is the centre of the symbol itself; the box
/// (bx, by, bw, bh) also holds a gate's caption — what must not overlap.
#[derive(Debug, Clone)]
pub struct Placed {
    pub id: i64,
    pub layer: usize,
    pub cx: f64,
    pub cy: f64,
    /// The symbol, as drawn (a gate turned for `down`).
    pub sw: f64,
    pub sh: f64,
    pub bx: f64,
    pub by: f64,
    pub bw: f64,
    pub bh: f64,
    lines: Vec<String>,
    refs: String,
}

impl Placed {
    pub fn overlaps(&self, o: &Placed) -> bool {
        self.bx < o.bx + o.bw && o.bx < self.bx + self.bw && self.by < o.by + o.bh && o.by < self.by + self.bh
    }
}

/// The symbol's size for its text, and the extra room around it (a gate's
/// caption), in the drawing's own axes.
fn measure(node: &Node, down: bool) -> Placed {
    let gate = GATES.contains(&node.kind.as_str());
    let max = match node.kind.as_str() {
        "decision" => MAX_W * 0.8,
        _ if gate => 150.0,
        _ => MAX_W,
    };
    let size = if gate { 11.0 } else { TEXT };
    let line_h = if gate { 15.0 } else { LINE };
    let lines = wrap(&node.text, size, max);
    let refs = node.refs.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(" ");
    let cw = lines.iter().map(|l| text_width(l, size)).fold(0.0, f64::max).max(text_width(&refs, SMALL));
    let ch = lines.len() as f64 * line_h + if refs.is_empty() { 0.0 } else { SMALL_LINE };
    let (sw, sh) = match node.kind.as_str() {
        _ if gate => {
            if down {
                (GATE_H, GATE_W)
            } else {
                (GATE_W, GATE_H)
            }
        }
        "decision" => {
            let w = cw * 1.6 + 28.0;
            let h = (ch / (1.0 - cw / w)).max(ch + 2.0 * PAD_Y) + 12.0;
            (w, h.max(56.0))
        }
        "connector" => {
            let d = cw.max(ch) + 18.0;
            (d.max(34.0), d.max(34.0))
        }
        "start" | "end" => {
            let h = ch + 2.0 * PAD_Y;
            (cw + h + 8.0, h)
        }
        "io" => (cw + 2.0 * PAD_X + 24.0, ch + 2.0 * PAD_Y),
        "subprocess" => (cw + 2.0 * PAD_X + 16.0, ch + 2.0 * PAD_Y),
        "prep" => (cw + 2.0 * PAD_X + 24.0, ch + 2.0 * PAD_Y),
        "document" => (cw + 2.0 * PAD_X, ch + 2.0 * PAD_Y + 10.0),
        "data" => (cw + 2.0 * PAD_X, ch + 2.0 * PAD_Y + 16.0),
        "manual" => (cw + 2.0 * PAD_X, ch + 2.0 * PAD_Y + 10.0),
        "delay" => {
            let h = ch + 2.0 * PAD_Y;
            (cw + 2.0 * PAD_X + h / 2.0, h)
        }
        _ => (cw + 2.0 * PAD_X, ch + 2.0 * PAD_Y),
    };
    let (sw, sh) = (sw.max(60.0).min(1000.0), sh.max(30.0));
    // A gate's caption sits beside the symbol, off the arrows' axis: below
    // when arrows run left to right, to the right when they run down.
    let (bw, bh) = if gate {
        if down {
            (sw + 8.0 + cw, sh.max(ch))
        } else {
            (sw.max(cw), sh + 6.0 + ch)
        }
    } else {
        (sw, sh)
    };
    Placed { id: node.id, layer: 0, cx: 0.0, cy: 0.0, sw, sh, bx: 0.0, by: 0.0, bw, bh, lines, refs }
}

/// Arrows that close a cycle, found depth-first in node order.
fn back_edges(ids: &[i64], out: &HashMap<i64, Vec<i64>>) -> std::collections::HashSet<(i64, i64)> {
    let mut state: HashMap<i64, u8> = HashMap::new(); // 1 on the stack, 2 done
    let mut back = std::collections::HashSet::new();
    for &start in ids {
        if state.contains_key(&start) {
            continue;
        }
        let mut stack: Vec<(i64, usize)> = vec![(start, 0)];
        state.insert(start, 1);
        while let Some((v, i)) = stack.pop() {
            let next = out.get(&v).and_then(|o| o.get(i)).copied();
            match next {
                Some(w) => {
                    stack.push((v, i + 1));
                    match state.get(&w) {
                        Some(1) => {
                            back.insert((v, w));
                        }
                        Some(_) => {}
                        None => {
                            state.insert(w, 1);
                            stack.push((w, 0));
                        }
                    }
                }
                None => {
                    state.insert(v, 2);
                }
            }
        }
    }
    back
}

/// Where every symbol goes, in drawing coordinates relative to (0, 0).
pub fn layout(map: &Map) -> Vec<Placed> {
    layout_full(map).0
}

/// The waypoints of an arrow that spans several layers: one per layer it
/// crosses, each holding its own slot in that layer, so the arrow passes
/// between symbols instead of through them.
type Routes = HashMap<(i64, i64), Vec<(f64, f64)>>;

fn layout_full(map: &Map) -> (Vec<Placed>, Routes) {
    let down = map.summary.direction != "right";
    let nodes: Vec<&Node> = map.nodes.iter().filter(|n| n.id != map.root).collect();
    let ids: Vec<i64> = nodes.iter().map(|n| n.id).collect();
    let mut placed: HashMap<i64, Placed> = nodes.iter().map(|n| (n.id, measure(n, down))).collect();
    let arrows: Vec<(i64, i64)> = map
        .edges
        .iter()
        .filter(|e| placed.contains_key(&e.from) && placed.contains_key(&e.to) && e.from != e.to)
        .map(|e| (e.from, e.to))
        .collect();
    let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
    for (a, b) in &arrows {
        out.entry(*a).or_default().push(*b);
    }
    let back = back_edges(&ids, &out);
    // Layering over the acyclic version: back arrows reversed.
    let dag: Vec<(i64, i64)> = arrows.iter().map(|&(a, b)| if back.contains(&(a, b)) { (b, a) } else { (a, b) }).collect();
    let mut indeg: HashMap<i64, usize> = ids.iter().map(|i| (*i, 0)).collect();
    let mut dag_succ: HashMap<i64, Vec<i64>> = HashMap::new();
    for (a, b) in &dag {
        *indeg.get_mut(b).unwrap() += 1;
        dag_succ.entry(*a).or_default().push(*b);
    }
    let mut layer: HashMap<i64, usize> = ids.iter().map(|i| (*i, 0)).collect();
    let mut queue: std::collections::VecDeque<i64> = ids.iter().copied().filter(|i| indeg[i] == 0).collect();
    while let Some(v) = queue.pop_front() {
        for w in dag_succ.get(&v).cloned().unwrap_or_default() {
            let l = layer[&v] + 1;
            if layer[&w] < l {
                layer.insert(w, l);
            }
            let d = indeg.get_mut(&w).unwrap();
            *d -= 1;
            if *d == 0 {
                queue.push_back(w);
            }
        }
    }
    // The graph that is ordered and drawn: forward arrows, each long one cut
    // into one-layer hops through virtual points (negative ids).
    let mut pred: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut succ: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut hops: Vec<((i64, i64), Vec<i64>)> = Vec::new();
    let mut next_virtual = -1i64;
    for &(a, b) in &arrows {
        if back.contains(&(a, b)) || hops.iter().any(|(k, _)| *k == (a, b)) {
            continue;
        }
        let mut prev = a;
        let mut through = Vec::new();
        for l in layer[&a] + 1..layer[&b] {
            let v = next_virtual;
            next_virtual -= 1;
            layer.insert(v, l);
            placed.insert(
                v,
                Placed { id: v, layer: l, cx: 0.0, cy: 0.0, sw: 0.0, sh: 0.0, bx: 0.0, by: 0.0, bw: 14.0, bh: 14.0, lines: vec![], refs: String::new() },
            );
            succ.entry(prev).or_default().push(v);
            pred.entry(v).or_default().push(prev);
            through.push(v);
            prev = v;
        }
        succ.entry(prev).or_default().push(b);
        pred.entry(b).or_default().push(prev);
        hops.push(((a, b), through));
    }
    let layers_n = layer.values().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut rows: Vec<Vec<i64>> = vec![Vec::new(); layers_n];
    for id in &ids {
        rows[layer[id]].push(*id);
    }
    for (_, through) in &hops {
        for v in through {
            rows[layer[v]].push(*v);
        }
    }
    // Barycenter ordering, a few sweeps each way.
    let pos_in = |rows: &Vec<Vec<i64>>| -> HashMap<i64, f64> {
        rows.iter().flat_map(|r| r.iter().enumerate().map(|(i, id)| (*id, i as f64))).collect()
    };
    for sweep in 0..6 {
        let pos = pos_in(&rows);
        let downward = sweep % 2 == 0;
        let order: Vec<usize> = if downward { (1..layers_n).collect() } else { (0..layers_n.saturating_sub(1)).rev().collect() };
        for l in order {
            let neigh = if downward { &pred } else { &succ };
            let mut keyed: Vec<(f64, f64, i64)> = rows[l]
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let ns: Vec<f64> = neigh.get(id).map(|v| v.iter().filter_map(|x| pos.get(x).copied()).collect()).unwrap_or_default();
                    let bary = if ns.is_empty() { i as f64 } else { ns.iter().sum::<f64>() / ns.len() as f64 };
                    (bary, i as f64, *id)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.partial_cmp(&b.1).unwrap()));
            rows[l] = keyed.into_iter().map(|k| k.2).collect();
        }
    }
    // Coordinates: `along` runs with the arrows, `across` along a layer.
    let along_size = |p: &Placed| if down { p.bh } else { p.bw };
    let across_size = |p: &Placed| if down { p.bw } else { p.bh };
    let mut along = 0.0;
    let mut across_of: HashMap<i64, f64> = HashMap::new();
    for (l, row) in rows.iter().enumerate() {
        let thick = row.iter().map(|id| along_size(&placed[id])).fold(0.0, f64::max);
        // Pack in order, each as close to the mean of its predecessors as the
        // gap allows, then shift the layer toward where it wanted to be.
        let mut pos: Vec<f64> = Vec::with_capacity(row.len());
        let mut want: Vec<Option<f64>> = Vec::with_capacity(row.len());
        for (i, id) in row.iter().enumerate() {
            let ps: Vec<f64> = pred.get(id).map(|v| v.iter().filter_map(|x| across_of.get(x).copied()).collect()).unwrap_or_default();
            let w = if ps.is_empty() { None } else { Some(ps.iter().sum::<f64>() / ps.len() as f64) };
            let min = if i == 0 {
                f64::NEG_INFINITY
            } else {
                pos[i - 1] + (across_size(&placed[&row[i - 1]]) + across_size(&placed[id])) / 2.0 + GAP
            };
            let start = if i == 0 { w.unwrap_or(0.0) } else { min };
            pos.push(w.map(|w| w.max(min)).unwrap_or(start).max(min));
            want.push(w);
        }
        let deltas: Vec<f64> = pos.iter().zip(&want).filter_map(|(p, w)| w.map(|w| w - p)).collect();
        let shift = if deltas.is_empty() {
            // Nothing above to align with: centre the layer on 0.
            let (first, last) = (pos.first().copied().unwrap_or(0.0), pos.last().copied().unwrap_or(0.0));
            -(first + last) / 2.0
        } else {
            deltas.iter().sum::<f64>() / deltas.len() as f64
        };
        for (i, id) in row.iter().enumerate() {
            across_of.insert(*id, pos[i] + shift);
            let p = placed.get_mut(id).unwrap();
            p.layer = l;
            let c_along = along + thick / 2.0;
            let c_across = pos[i] + shift;
            // Box centre; the symbol sits at the box's start when a caption
            // trails it.
            let (bcx, bcy) = if down { (c_across, c_along) } else { (c_along, c_across) };
            p.bx = bcx - p.bw / 2.0;
            p.by = bcy - p.bh / 2.0;
            if p.bw > p.sw && down {
                p.cx = p.bx + p.sw / 2.0;
                p.cy = bcy;
            } else if p.bh > p.sh && !down {
                p.cx = bcx;
                p.cy = p.by + p.sh / 2.0;
            } else {
                p.cx = bcx;
                p.cy = bcy;
            }
        }
        along += thick + LAYER_GAP;
    }
    let routes: Routes = hops
        .into_iter()
        .filter(|(_, t)| !t.is_empty())
        .map(|(k, t)| (k, t.iter().map(|v| (placed[v].cx, placed[v].cy)).collect()))
        .collect();
    (ids.iter().map(|id| placed.remove(id).unwrap()).collect(), routes)
}

/// A point on the symbol's outline where an arrow leaves (`out`) or enters.
/// `t` in -1..1 spreads several arrows along the side.
fn port(p: &Placed, kind: &str, down: bool, out: bool, t: f64, vertex: i8) -> (f64, f64) {
    let sign = if out { 1.0 } else { -1.0 };
    if kind == "decision" && out {
        // A decision leaves through a vertex: -1 / 1 the side ones, 0 the far one.
        if vertex != 0 {
            let side = vertex as f64;
            return if down { (p.cx + side * p.sw / 2.0, p.cy) } else { (p.cx, p.cy + side * p.sh / 2.0) };
        }
        return if down { (p.cx, p.cy + p.sh / 2.0) } else { (p.cx + p.sw / 2.0, p.cy) };
    }
    let spread = if GATES.contains(&kind) { 0.32 } else { 0.25 };
    if down {
        (p.cx + t * p.sw * spread, p.cy + sign * p.sh / 2.0)
    } else {
        (p.cx + sign * p.sw / 2.0, p.cy + t * p.sh * spread)
    }
}

fn gate_path(kind: &str) -> (String, bool) {
    // Drawn facing right in a GATE_W x GATE_H box; the bool says "bubble".
    let negated = matches!(kind, "not" | "nand" | "nor" | "xnor");
    let end = if negated { 50.0 } else { 56.0 };
    let or = |x0: f64| {
        format!(
            "M{x0},0 Q{},22 {x0},44 Q{},44 {end},22 Q{},0 {x0},0 Z",
            x0 + 12.0,
            x0 + (end - x0) * 0.62,
            x0 + (end - x0) * 0.62
        )
    };
    let body = match kind {
        "and" | "nand" => format!("M0,0 H{a} A22,22 0 0 1 {a},44 H0 Z", a = end - 22.0),
        "or" | "nor" => or(0.0),
        "xor" | "xnor" => format!("{} M0,0 Q12,22 0,44", or(6.0)),
        _ => format!("M0,0 L{end},22 L0,44 Z"), // not
    };
    (body, negated)
}

/// The SVG of one symbol's shape.
fn shape(node: &Node, p: &Placed, down: bool, stroke: &str, fill: &str, sw: &str, dash: &str) -> String {
    let (x, y, w, h, cx, cy) = (p.cx - p.sw / 2.0, p.cy - p.sh / 2.0, p.sw, p.sh, p.cx, p.cy);
    let st = format!("class=\"shape surface\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{sw}\"{dash}");
    let poly = |pts: &[(f64, f64)]| {
        format!(
            "<polygon {st} points=\"{}\"/>",
            pts.iter().map(|(a, b)| format!("{},{}", n(*a), n(*b))).collect::<Vec<_>>().join(" ")
        )
    };
    match node.kind.as_str() {
        "start" | "end" => format!("<rect {st} x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{}\"/>", n(x), n(y), n(w), n(h), n(h / 2.0)),
        "decision" => poly(&[(cx, y), (x + w, cy), (cx, y + h), (x, cy)]),
        "io" => poly(&[(x + 12.0, y), (x + w, y), (x + w - 12.0, y + h), (x, y + h)]),
        "manual" => poly(&[(x, y + 10.0), (x + w, y), (x + w, y + h), (x, y + h)]),
        "prep" => poly(&[(x + 12.0, y), (x + w - 12.0, y), (x + w, cy), (x + w - 12.0, y + h), (x + 12.0, y + h), (x, cy)]),
        "subprocess" => format!(
            "<rect {st} x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/><path class=\"edge-line\" d=\"M{} {} V{} M{} {} V{}\" stroke=\"{stroke}\" stroke-width=\"{sw}\"/>",
            n(x), n(y), n(w), n(h), n(x + 8.0), n(y), n(y + h), n(x + w - 8.0), n(y), n(y + h)
        ),
        "document" => format!(
            "<path {st} d=\"M{} {} H{} V{} Q{} {} {} {} T{} {} Z\"/>",
            n(x), n(y), n(x + w), n(y + h - 8.0),
            n(x + w * 0.75), n(y + h - 18.0), n(x + w * 0.5), n(y + h - 8.0),
            n(x), n(y + h - 8.0)
        ),
        "data" => {
            let e = 7.0;
            format!(
                "<path {st} d=\"M{x0} {t} A{rx} {e} 0 0 1 {x1} {t} V{b} A{rx} {e} 0 0 1 {x0} {b} Z\"/><path fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{sw}\" d=\"M{x0} {t} A{rx} {e} 0 0 0 {x1} {t}\"/>",
                x0 = n(x), x1 = n(x + w), t = n(y + e), b = n(y + h - e), rx = n(w / 2.0)
            )
        }
        "delay" => format!(
            "<path {st} d=\"M{} {} H{} A{r} {r} 0 0 1 {} {} H{} Z\"/>",
            n(x), n(y), n(x + w - h / 2.0), n(x + w - h / 2.0), n(y + h), n(x), r = n(h / 2.0)
        ),
        "connector" => format!("<circle {st} cx=\"{}\" cy=\"{}\" r=\"{}\"/>", n(cx), n(cy), n(w / 2.0)),
        "note" => format!(
            "<path fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{sw}\" d=\"M{} {} H{} V{} H{}\"/>",
            n(x + 12.0), n(y), n(x), n(y + h), n(x + 12.0)
        ),
        k if GATES.contains(&k) => {
            let (body, bubble) = gate_path(k);
            // Facing right as drawn; turned a quarter for `down`, inputs on top.
            let tf = if down {
                format!("translate({} {}) rotate(90) translate({} {})", n(cx), n(cy), n(-GATE_W / 2.0), n(-GATE_H / 2.0))
            } else {
                format!("translate({} {})", n(cx - GATE_W / 2.0), n(cy - GATE_H / 2.0))
            };
            let mut g = format!("<g transform=\"{tf}\"><path {st} d=\"{body}\"/>");
            if bubble {
                g.push_str(&format!("<circle {st} cx=\"54\" cy=\"22\" r=\"4\"/><path fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{sw}\" d=\"M58 22 H{GATE_W}\"/>"));
            } else {
                g.push_str(&format!("<path fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{sw}\" d=\"M56 22 H{GATE_W}\"/>"));
            }
            g.push_str("</g>");
            g
        }
        _ => format!("<rect {st} x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"3\"/>", n(x), n(y), n(w), n(h)),
    }
}

pub fn render_svg(map: &Map, theme: Theme) -> String {
    let dark = theme == Theme::Dark;
    let pal: &Palette = if dark { &DARK } else { &LIGHT };
    let down = map.summary.direction != "right";
    let (placed, routes) = layout_full(map);
    let by_id: HashMap<i64, &Placed> = placed.iter().map(|p| (p.id, p)).collect();
    let nodes: HashMap<i64, &Node> = map.nodes.iter().map(|n| (n.id, n)).collect();

    let (mut min_x, mut max_x, mut min_y, mut max_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for p in &placed {
        min_x = min_x.min(p.bx);
        max_x = max_x.max(p.bx + p.bw);
        min_y = min_y.min(p.by);
        max_y = max_y.max(p.by + p.bh);
    }
    // Loop-back arrows run beside the layers: room for them.
    let arrows: Vec<&crate::mindmap::Edge> =
        map.edges.iter().filter(|e| by_id.contains_key(&e.from) && by_id.contains_key(&e.to) && e.from != e.to).collect();
    let is_back = |e: &crate::mindmap::Edge| by_id[&e.from].layer >= by_id[&e.to].layer;
    let backs = arrows.iter().filter(|e| is_back(e)).count() as f64;
    // Their lanes start past the widest layer, and their labels sit on them.
    let lanes_from = if down { max_x } else { max_y };
    let back_label = arrows
        .iter()
        .filter(|e| is_back(e))
        .map(|e| text_width(&e.label, 11.0) + 16.0)
        .fold(0.0, f64::max);
    if backs > 0.0 {
        if down {
            max_x += 16.0 + backs * 14.0 + back_label;
        } else {
            max_y += 16.0 + backs * 14.0 + 22.0;
        }
    }

    let s = &map.summary;
    let title = format!("{} — flowchart #{}", s.title, s.id);
    let subtitle = format!(
        "{}{} · owner {} · {} symbols",
        s.state,
        if s.shared { " · shared" } else { "" },
        s.owner_nick,
        placed.len()
    );
    let chains: Vec<String> = map
        .chains
        .iter()
        .map(|c| format!("{}: {}", c.name, c.entries.iter().map(|e| format!("#{e}")).collect::<Vec<_>>().join(" › ")))
        .collect();
    let chain_h = if chains.is_empty() { 0.0 } else { 24.0 + chains.len() as f64 * 18.0 };
    let content_w = (max_x - min_x).max(text_width(&title, 18.0)).max(chains.iter().map(|c| text_width(c, 12.0)).fold(0.0, f64::max));
    let width = content_w + 2.0 * MARGIN;
    let height = HEADER + (max_y - min_y) + chain_h + 2.0 * MARGIN;
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
        o.push_str("<style>@media (prefers-color-scheme: dark){");
        o.push_str(&format!(
            ".bg{{fill:{}}} .surface{{fill:{}}} .tx{{fill:{}}} .mu{{fill:{}}} .lbg{{fill:{}}} .arrow{{stroke:{}}} .head{{fill:{}}} ",
            DARK.bg, DARK.surface, DARK.text, DARK.muted, DARK.label_bg, DARK.link, DARK.link
        ));
        for k in crate::mindmap::FLOW_KINDS {
            let c = kind_color(k, true);
            o.push_str(&format!(".f-{k} .shape,.f-{k} .edge-line{{stroke:{c}}} "));
        }
        o.push_str("}</style>\n");
    }
    o.push_str(&format!(
        "<defs><marker id=\"head\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"8\" markerHeight=\"8\" orient=\"auto-start-reverse\"><path class=\"head\" d=\"M0,0 L10,5 L0,10 z\" fill=\"{}\"/></marker></defs>\n",
        pal.link
    ));
    o.push_str(&format!("<rect class=\"bg\" width=\"100%\" height=\"100%\" fill=\"{}\"/>\n", pal.bg));
    o.push_str(&format!(
        "<text class=\"tx\" x=\"{}\" y=\"{}\" font-size=\"18\" fill=\"{}\">{}</text>\n",
        n(MARGIN), n(MARGIN + 18.0), pal.text, esc(&title)
    ));
    o.push_str(&format!(
        "<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"12\" fill=\"{}\">{}</text>\n",
        n(MARGIN), n(MARGIN + 38.0), pal.muted, esc(&subtitle)
    ));

    // Ports: spread a symbol's arrows along its side by where the other end is.
    let across = |p: &Placed| if down { p.cx } else { p.cy };
    let spread = |list: &mut Vec<(i64, f64)>| -> HashMap<i64, f64> {
        list.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let k = list.len();
        list.iter().enumerate().map(|(i, (eid, _))| (*eid, if k <= 1 { 0.0 } else { -1.0 + 2.0 * i as f64 / (k - 1) as f64 })).collect()
    };
    let mut outs: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    let mut ins: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    let across_pt = |pt: (f64, f64)| if down { pt.0 } else { pt.1 };
    for e in arrows.iter().filter(|e| !is_back(e)) {
        // The other end as the arrow first and last heads: a waypoint if any.
        let way = routes.get(&(e.from, e.to));
        let toward = way.and_then(|w| w.first()).map(|p| across_pt(*p)).unwrap_or(across(by_id[&e.to]));
        let from = way.and_then(|w| w.last()).map(|p| across_pt(*p)).unwrap_or(across(by_id[&e.from]));
        outs.entry(e.from).or_default().push((e.id, toward));
        ins.entry(e.to).or_default().push((e.id, from));
    }
    // Each branch of a decision gets its own vertex (journal#108): ordered
    // across, the first takes the near side when it lies that way, the last
    // the far side, the rest the bottom.
    let mut vertex: HashMap<i64, i8> = HashMap::new();
    for (from, list) in outs.iter_mut() {
        if nodes[from].kind != "decision" {
            continue;
        }
        list.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let me = across(by_id[from]);
        let thr = by_id[from].sw.max(by_id[from].sh) / 3.0;
        let k = list.len();
        for (i, (eid, toward)) in list.iter().enumerate() {
            let rel = toward - me;
            let v = if k == 1 {
                if rel.abs() > thr { rel.signum() as i8 } else { 0 }
            } else if i == 0 {
                if rel < -thr || (k > 2 && rel < 0.0) { -1 } else { 0 }
            } else if i == k - 1 {
                if k == 2 && list[0].1 - me < -thr && rel.abs() <= thr { 0 } else { 1 }
            } else {
                0
            };
            vertex.insert(*eid, v);
        }
    }
    let out_t: HashMap<i64, f64> = outs.values_mut().flat_map(|l| spread(l)).collect();
    let in_t: HashMap<i64, f64> = ins.values_mut().flat_map(|l| spread(l)).collect();

    // Arrows first, under the symbols.
    let mut labels = String::new();
    let mut lane = 0.0;
    o.push_str("<g fill=\"none\" stroke-linejoin=\"round\">\n");
    for e in &arrows {
        let (a, b) = (by_id[&e.from], by_id[&e.to]);
        let (ka, kb) = (nodes[&e.from].kind.as_str(), nodes[&e.to].kind.as_str());
        // A path from the drawing's own coordinates, shifted onto the page.
        let pt = |x: f64, y: f64| format!("{} {}", n(x + dx), n(y + dy));
        let (path, lx, ly) = if !is_back(e) {
            let way: Vec<(f64, f64)> = routes.get(&(e.from, e.to)).cloned().unwrap_or_default();
            let v = vertex.get(&e.id).copied().unwrap_or(0);
            let (x1, y1) = port(a, ka, down, true, out_t.get(&e.id).copied().unwrap_or(0.0), v);
            let (x2, y2) = port(b, kb, down, false, in_t.get(&e.id).copied().unwrap_or(0.0), 0);
            let side_exit = ka == "decision" && if down { (y1 - a.cy).abs() < 1.0 } else { (x1 - a.cx).abs() < 1.0 };
            // A side exit of a decision runs a short way out, then turns with
            // the flow — never across the rest of its layer.
            let mut d = format!("M{}", pt(x1, y1));
            let mut cur = (x1, y1);
            if side_exit {
                cur = if down { (x1 + v as f64 * 16.0, y1) } else { (x1, y1 + v as f64 * 16.0) };
                d.push_str(&format!(" L{}", pt(cur.0, cur.1)));
            }
            let mut points = way.clone();
            points.push((x2, y2));
            for next in points {
                // Leave and arrive along the flow: vertical tangents for
                // `down`, horizontal for `right`.
                if down {
                    let my = (cur.1 + next.1) / 2.0;
                    d.push_str(&format!(" C{} {} {}", pt(cur.0, my), pt(next.0, my), pt(next.0, next.1)));
                } else {
                    let mx = (cur.0 + next.0) / 2.0;
                    d.push_str(&format!(" C{} {} {}", pt(mx, cur.1), pt(mx, next.1), pt(next.0, next.1)));
                }
                cur = next;
            }
            // The label sits by the arrow's start, where "yes"/"no" belong.
            let (lx, ly) = if side_exit {
                if down { (x1 + v as f64 * 6.0 - if v < 0 { 24.0 } else { 0.0 }, y1 - 6.0) } else { (x1 + 6.0, y1 + v as f64 * 14.0) }
            } else if down {
                (x1 + 5.0, y1 + 15.0)
            } else {
                (x1 + 8.0, y1 - 6.0)
            };
            (d, lx, ly)
        } else {
            // Loop back: out of the far side into the gap after the source's
            // layer, along a lane beside every layer, into the gap before the
            // target's layer and in through its near side.
            lane += 14.0;
            if down {
                let x = lanes_from + 16.0 + lane - 14.0;
                let (x1, y1) = (a.cx, a.cy + a.sh / 2.0);
                let (x2, y2) = (b.cx, b.cy - b.sh / 2.0);
                let (g1, g2) = (y1 + LAYER_GAP * 0.35, y2 - LAYER_GAP * 0.35);
                (
                    format!("M{} V{} H{} V{} H{} V{}", pt(x1, y1), n(g1 + dy), n(x + dx), n(g2 + dy), n(x2 + dx), n(y2 + dy)),
                    x + 6.0,
                    (g1 + g2) / 2.0,
                )
            } else {
                let y = lanes_from + 16.0 + lane - 14.0;
                let (x1, y1) = (a.cx + a.sw / 2.0, a.cy);
                let (x2, y2) = (b.cx - b.sw / 2.0, b.cy);
                let (g1, g2) = (x1 + LAYER_GAP * 0.35, x2 - LAYER_GAP * 0.35);
                (
                    format!("M{} H{} V{} H{} V{} H{}", pt(x1, y1), n(g1 + dx), n(y + dy), n(g2 + dx), n(y2 + dy), n(x2 + dx)),
                    (g1 + g2) / 2.0,
                    y + 15.0,
                )
            }
        };
        let dim = if nodes[&e.to].state == "dropped" || nodes[&e.from].state == "dropped" { " stroke-opacity=\"0.4\"" } else { "" };
        // An annotation is tied to its symbol by a dashed line, not a flow.
        let note = nodes[&e.from].kind == "note" || nodes[&e.to].kind == "note";
        let tail = if note { " stroke-dasharray=\"4 3\"".to_string() } else { " marker-end=\"url(#head)\"".to_string() };
        o.push_str(&format!(
            "<path class=\"arrow\" d=\"{path}\" stroke=\"{}\" stroke-width=\"1.6\"{tail}{dim}/>\n",
            pal.link
        ));
        let mut label = e.label.clone();
        if !e.refs.is_empty() {
            let refs = e.refs.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(" ");
            label = if label.is_empty() { refs } else { format!("{label} {refs}") };
        }
        if !label.is_empty() {
            let lw = text_width(&label, 11.0) + 8.0;
            labels.push_str(&format!(
                "<rect class=\"lbg\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"15\" rx=\"3\" fill=\"{}\" opacity=\"0.92\"/>\n<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"11\" fill=\"{}\">{}</text>\n",
                n(lx + dx - 4.0), n(ly + dy - 11.0), n(lw), pal.label_bg, n(lx + dx), n(ly + dy), pal.muted, esc(&label)
            ));
        }
    }
    o.push_str("</g>\n");

    // Symbols, then their text.
    for p in &placed {
        let node = nodes[&p.id];
        let kc = kind_color(&node.kind, dark);
        let gate = GATES.contains(&node.kind.as_str());
        let dropped = node.state == "dropped";
        let sw = if node.state == "done" { "2.6" } else { "1.6" };
        let dash = if dropped { " stroke-dasharray=\"5 4\"" } else { "" };
        o.push_str(&format!(
            "<g class=\"f-{}\" data-id=\"{}\"{}>\n",
            esc(&node.kind),
            node.id,
            if dropped { " opacity=\"0.5\"" } else { "" }
        ));
        let mut moved = p.clone();
        moved.cx += dx;
        moved.cy += dy;
        o.push_str(&shape(node, &moved, down, kc, pal.surface, sw, dash));
        o.push('\n');
        let (size, line_h) = if gate { (11.0, 15.0) } else { (TEXT, LINE) };
        let block_h = p.lines.len() as f64 * line_h + if p.refs.is_empty() { 0.0 } else { SMALL_LINE };
        // Text centred in the symbol; a gate's caption beside it.
        let (tx, mut ty, anchor) = if gate {
            if down {
                (moved.cx + p.sw / 2.0 + 8.0, moved.cy - block_h / 2.0, "start")
            } else {
                (moved.cx, moved.cy + p.sh / 2.0 + 6.0, "middle")
            }
        } else {
            let lift = match node.kind.as_str() {
                "document" => 4.0,
                "data" => -3.0,
                "manual" => -4.0,
                _ => 0.0,
            };
            (moved.cx, moved.cy - block_h / 2.0 - lift, "middle")
        };
        let deco = if dropped { " text-decoration=\"line-through\"" } else { "" };
        for line in &p.lines {
            ty += line_h;
            o.push_str(&format!(
                "<text class=\"tx\" x=\"{}\" y=\"{}\" font-size=\"{size}\" text-anchor=\"{anchor}\" fill=\"{}\"{deco}>{}</text>\n",
                n(tx), n(ty - 5.0), pal.text, esc(line)
            ));
        }
        if !p.refs.is_empty() {
            ty += SMALL_LINE;
            o.push_str(&format!(
                "<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"{SMALL}\" text-anchor=\"{anchor}\" fill=\"{}\">{}</text>\n",
                n(tx), n(ty - 3.0), pal.muted, esc(&p.refs)
            ));
        }
        o.push_str("</g>\n");
    }
    o.push_str(&labels);
    if !chains.is_empty() {
        let mut y = dy + max_y + 30.0;
        o.push_str(&format!("<text class=\"tx\" x=\"{}\" y=\"{}\" font-size=\"13\" fill=\"{}\">Chains</text>\n", n(MARGIN), n(y), pal.text));
        for c in &chains {
            y += 18.0;
            o.push_str(&format!("<text class=\"mu\" x=\"{}\" y=\"{}\" font-size=\"12\" fill=\"{}\">{}</text>\n", n(MARGIN), n(y), pal.muted, esc(c)));
        }
    }
    o.push_str("</svg>\n");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mindmap::{Chain, Edge, MapSummary};
    use crate::mindmap_svg::parse;

    fn node(id: i64, kind: &str, text: &str) -> Node {
        Node { id, parent: Some(1), kind: kind.into(), text: text.into(), state: "open".into(), refs: vec![], author: None }
    }

    fn flow(direction: &str, nodes: Vec<Node>, edges: Vec<(i64, i64, &str)>) -> Map {
        let mut all = vec![Node { id: 1, parent: None, kind: "topic".into(), text: "Оформление заказа".into(), state: "open".into(), refs: vec![], author: None }];
        all.extend(nodes);
        let summary = MapSummary {
            id: 9,
            title: "Оформление заказа".into(),
            owner_nick: "mira".into(),
            owner_host: None,
            state: "open".into(),
            shared: true,
            layout: "flowchart".into(),
            direction: direction.into(),
            nodes: all.len() as i64,
            open_nodes: 0,
            created_at: 0,
            updated_at: 0,
        };
        let edges = edges
            .into_iter()
            .enumerate()
            .map(|(i, (f, t, l))| Edge { id: i as i64 + 1, from: f, to: t, label: l.into(), refs: if i == 1 { vec![42] } else { vec![] } })
            .collect();
        Map { summary, root: 1, nodes: all, edges, chains: vec![Chain { id: 1, name: "путь".into(), entries: vec![3, 4] }] }
    }

    /// Every symbol and gate, with a decision that loops back.
    fn every_symbol(direction: &str) -> Map {
        let mut ns = vec![
            node(2, "start", "Начало"),
            node(3, "io", "Получить корзину"),
            node(4, "decision", "Корзина пуста?"),
            node(5, "process", "Посчитать сумму со скидкой"),
            node(6, "subprocess", "Проверить склад"),
            node(7, "data", "orders DB"),
            node(8, "document", "Счёт"),
            node(9, "manual", "Ввод промокода"),
            node(10, "prep", "Инициализация"),
            node(11, "delay", "Ждать оплату"),
            node(12, "connector", "A"),
            node(13, "note", "оплата через банк"),
            node(14, "end", "Конец"),
            node(15, "and", "оплачено И на складе"),
            node(16, "or", "курьер ИЛИ самовывоз"),
            node(17, "not", "не отменён"),
            node(18, "nand", "nand"),
            node(19, "nor", "nor"),
            node(20, "xor", "xor"),
            node(21, "xnor", "xnor"),
        ];
        ns[3].refs = vec![7, 8];
        flow(
            direction,
            ns,
            vec![
                (2, 10, ""),
                (10, 3, ""),
                (3, 4, ""),
                (4, 14, "да"),
                (4, 9, "нет"),
                (9, 5, ""),
                (5, 6, ""),
                (6, 7, ""),
                (6, 15, ""),
                (16, 15, ""),
                (15, 17, ""),
                (16, 18, ""),
                (17, 8, ""),
                (8, 11, ""),
                (11, 4, "повтор"), // loop back
                (18, 19, ""),
                (19, 20, ""),
                (20, 21, ""),
                (21, 12, ""),
                (12, 14, ""),
                (13, 5, ""),
            ],
        )
    }

    fn chain(n_nodes: i64, direction: &str) -> Map {
        let kinds = ["process", "decision", "io", "data", "and", "subprocess", "or"];
        let ns: Vec<Node> = (2..n_nodes + 2).map(|i| node(i, kinds[(i as usize) % kinds.len()], &format!("шаг {i}: обработать заказ"))).collect();
        // A tangle: each step feeds the next two, every ninth loops back.
        let mut es = vec![];
        for i in 2..n_nodes + 2 {
            if i + 1 < n_nodes + 2 {
                es.push((i, i + 1, ""));
            }
            if i + 2 < n_nodes + 2 && i % 3 == 0 {
                es.push((i, i + 2, "да"));
            }
            if i % 9 == 0 && i > 4 {
                es.push((i, i - 4, "снова"));
            }
        }
        flow(direction, ns, es)
    }

    fn assert_clean(map: &Map) {
        let placed = layout(map);
        assert_eq!(placed.len(), map.nodes.len() - 1);
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(!a.overlaps(b), "{} overlaps {} ({} nodes, {})", a.id, b.id, placed.len(), map.summary.direction);
            }
        }
        // Every arrow that is not a loop back goes to a later layer.
        let layer: HashMap<i64, usize> = placed.iter().map(|p| (p.id, p.layer)).collect();
        let ids: Vec<i64> = placed.iter().map(|p| p.id).collect();
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        for e in &map.edges {
            out.entry(e.from).or_default().push(e.to);
        }
        let back = back_edges(&ids, &out);
        for e in &map.edges {
            if !back.contains(&(e.from, e.to)) {
                assert!(layer[&e.from] < layer[&e.to], "arrow {} -> {} does not go forward", e.from, e.to);
            }
        }
    }

    #[test]
    fn layers_follow_the_arrows_and_nothing_overlaps() {
        for dir in ["down", "right"] {
            assert_clean(&flow(dir, vec![node(2, "start", "Начало")], vec![]));
            assert_clean(&every_symbol(dir));
            assert_clean(&chain(150, dir));
        }
    }

    #[test]
    fn the_loop_back_is_found() {
        let map = every_symbol("down");
        let ids: Vec<i64> = map.nodes.iter().skip(1).map(|n| n.id).collect();
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        for e in &map.edges {
            out.entry(e.from).or_default().push(e.to);
        }
        let back = back_edges(&ids, &out);
        assert!(back.contains(&(11, 4)), "{back:?}");
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn every_symbol_draws_in_both_directions_and_themes() {
        for dir in ["down", "right"] {
            for theme in [Theme::Light, Theme::Dark, Theme::Auto] {
                let svg = crate::mindmap_svg::render_svg(&every_symbol(dir), theme);
                assert!(svg.contains("flowchart #9"), "dispatched to the flowchart renderer");
                let tree = parse(&svg).unwrap_or_else(|e| panic!("{dir} {theme:?}: {e:#}"));
                assert!(tree.root().has_children());
                // The journal references of a symbol and of an arrow are drawn.
                assert!(svg.contains("#7 #8"), "node refs");
                assert!(svg.contains("#42"), "arrow refs");
                assert!(svg.contains("повтор") && svg.contains("нет"));
            }
        }
        let big = crate::mindmap_svg::render_svg(&chain(150, "down"), Theme::Light);
        parse(&big).unwrap();
    }

    /// `MINDMAP_SAMPLE_DIR=/dir cargo test --lib mindmap_flow::tests::sample -- --ignored`
    #[test]
    #[ignore]
    fn sample() {
        let dir = std::path::PathBuf::from(std::env::var("MINDMAP_SAMPLE_DIR").expect("MINDMAP_SAMPLE_DIR"));
        for d in ["down", "right"] {
            let map = every_symbol(d);
            std::fs::write(dir.join(format!("flow-{d}.svg")), crate::mindmap_svg::render_svg(&map, Theme::Auto)).unwrap();
            std::fs::write(dir.join(format!("flow-{d}.png")), crate::mindmap_svg::render_png(&map, Theme::Light).unwrap()).unwrap();
        }
        std::fs::write(dir.join("flow-dark.png"), crate::mindmap_svg::render_png(&every_symbol("down"), Theme::Dark).unwrap()).unwrap();
    }
}
