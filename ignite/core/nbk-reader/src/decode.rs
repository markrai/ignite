//! NBK_MATERIALIZED decode: reading-order driven, structural `nmdl.stroke` ownership.
//! Faithful port of the proven Python `nbk_decode` / `nmdl_stroke` production path (parity is
//! enforced by golden tests over `samples/`). Deliberate deviation: payloads that are not Ion
//! documents contribute no graph edges (Python's regex bytescan fallback is not ported; no
//! known notebook has such payloads — they are counted in [`Diagnostics::non_ion_payloads`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::decode_channel;
use crate::container::FragmentStore;
use crate::ion::{walk_document, Kind, Node, IVM};

const SID_IMPORTS: u64 = 6;
const SID_SYMBOLS: u64 = 7;
const SID_MAX_ID: u64 = 8;
const SYSTEM_SYMBOL_MAX: u64 = 9;
const DEFAULT_YJ_MAX: u64 = 816;
const SID_READING_ORDERS: u64 = 169;
const SID_READING_ORDER_NAME: u64 = 178;
const SID_READING_ORDER_SECTIONS: u64 = 170;
const SID_MAIN_READING_ORDER: i128 = 351;
const SID_PAGE_SECTION: u64 = 260;
const SID_METADATA_KEY: u64 = 492;
const SID_METADATA_VALUE: u64 = 307;
const NOTE_TEMPLATE_COLLECTION: &str = "note_template_collection";

/// Freeform notebook canvas @ normalized_ppi 2520 (Python defaults).
pub const DEFAULT_CANVAS_WIDTH: i128 = 15624;
pub const DEFAULT_CANVAS_HEIGHT: i128 = 20832;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedStroke {
    /// Absolute canvas coordinates (channel value + stroke_bounds origin).
    pub xs: Vec<i64>,
    pub ys: Vec<i64>,
    /// Raw `nmdl.pressure` channel (observed 1..4095), same length as `xs` when present.
    pub pressure: Option<Vec<i64>>,
    pub brush_type: Option<i128>,
    pub color: Option<i128>,
    pub thickness: Option<f64>,
    /// Decode problems for this stroke (it may still carry points).
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedPage {
    pub page_id: String,
    /// The reading order lists this page but its fragment is absent.
    pub missing: bool,
    pub width: i128,
    pub height: i128,
    pub normalized_ppi: Option<f64>,
    pub template_id: Option<String>,
    /// Canvas fields that fell back to defaults.
    pub field_fallbacks: Vec<&'static str>,
    pub strokes: Vec<DecodedStroke>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub fingerprints_removed: usize,
    pub base_fragment_count: usize,
    pub delta_count: usize,
    pub delta_tombstone_count: usize,
    pub live_fragment_count: usize,
    pub yj_import_max_id: u64,
    pub local_symbol_count: usize,
    pub reading_order_count: usize,
    pub orphan_page_count: usize,
    pub non_ion_payloads: usize,
    pub structural_edges: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedNotebook {
    /// Live pages in main reading order.
    pub pages: Vec<DecodedPage>,
    pub template_ids: Vec<String>,
    /// `kindle_title_metadata` → `book_id` (internal Amazon id; provenance only).
    pub book_id: Option<String>,
    pub diagnostics: Diagnostics,
}

impl DecodedNotebook {
    /// Python `nbk_is_usable`: a main reading order with at least one page.
    pub fn is_usable(&self) -> bool {
        !self.pages.is_empty()
    }

    pub fn stroke_count(&self) -> usize {
        self.pages.iter().map(|p| p.strokes.len()).sum()
    }
}

/// NMDL local symbol lookup (`nmdl.*` names → absolute SIDs).
struct Symbols {
    local: Vec<String>,
    base: u64,
}

impl Symbols {
    fn parse(store: &FragmentStore) -> (Self, u64) {
        let mut yj_max = DEFAULT_YJ_MAX;
        let mut local: Vec<String> = Vec::new();
        if let Some(lst) = store.get("$ion_symbol_table") {
            for root in walk_document(lst) {
                root.visit(&mut |n| {
                    if n.sid == Some(SID_IMPORTS) {
                        for imp in &n.children {
                            for f in &imp.children {
                                if f.sid == Some(SID_MAX_ID) {
                                    if let Kind::Int(Some(v)) = f.kind {
                                        if let Ok(v) = u64::try_from(v) {
                                            yj_max = v;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if n.sid == Some(SID_SYMBOLS) {
                        local = n
                            .children
                            .iter()
                            .filter_map(|c| match &c.kind {
                                Kind::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect();
                    }
                });
            }
        }
        let base = SYSTEM_SYMBOL_MAX + yj_max + 1;
        (Self { local, base }, yj_max)
    }

    fn sid(&self, name: &str) -> Option<u64> {
        self.local
            .iter()
            .position(|s| s == name)
            .map(|i| self.base + i as u64)
    }
}

/// Resolved `nmdl.*` SIDs used by the decoder.
struct Nmdl {
    kind: Option<u64>,
    stroke: Option<u64>,
    canvas_width: Option<u64>,
    canvas_height: Option<u64>,
    normalized_ppi: Option<u64>,
    template_id: Option<u64>,
    brush_type: Option<u64>,
    color: Option<u64>,
    thickness: Option<u64>,
    stroke_bounds: Option<u64>,
    stroke_points: Option<u64>,
    num_points: Option<u64>,
    position_x: Option<u64>,
    position_y: Option<u64>,
    pressure: Option<u64>,
    optional_channels: [Option<u64>; 4],
}

impl Nmdl {
    fn new(s: &Symbols) -> Self {
        Self {
            kind: s.sid("nmdl.type"),
            stroke: s.sid("nmdl.stroke"),
            canvas_width: s.sid("nmdl.canvas_width"),
            canvas_height: s.sid("nmdl.canvas_height"),
            normalized_ppi: s.sid("nmdl.normalized_ppi"),
            template_id: s.sid("nmdl.template_id"),
            brush_type: s.sid("nmdl.brush_type"),
            color: s.sid("nmdl.color"),
            thickness: s.sid("nmdl.thickness"),
            stroke_bounds: s.sid("nmdl.stroke_bounds"),
            stroke_points: s.sid("nmdl.stroke_points"),
            num_points: s.sid("nmdl.num_points"),
            position_x: s.sid("nmdl.position_x"),
            position_y: s.sid("nmdl.position_y"),
            pressure: s.sid("nmdl.pressure"),
            optional_channels: [
                s.sid("nmdl.tilt_x"),
                s.sid("nmdl.tilt_y"),
                s.sid("nmdl.thickness_adjust_factor"),
                s.sid("nmdl.density_adjust_factor"),
            ],
        }
    }
}

fn is_ion(payload: &[u8]) -> bool {
    payload.starts_with(&IVM)
}

/// Python `_string_value`: a string, or an annotation's first child string.
fn string_value(n: &Node<'_>) -> Option<String> {
    match &n.kind {
        Kind::String(s) => Some(s.clone()),
        Kind::Annotation(_) => n.children.first().and_then(string_value),
        _ => None,
    }
}

#[derive(Debug)]
enum OrderName {
    Num(i128),
    Str(String),
    None,
}

struct ReadingOrder {
    name: OrderName,
    sections: Vec<String>,
}

fn section_ids(n: &Node<'_>) -> Vec<String> {
    let core = match (&n.kind, n.children.first()) {
        (Kind::Annotation(_), Some(c)) => c,
        _ => n,
    };
    if !matches!(core.kind, Kind::List) {
        return string_value(core).into_iter().collect();
    }
    core.children.iter().filter_map(string_value).collect()
}

fn reading_orders(payload: &[u8]) -> Vec<ReadingOrder> {
    let mut orders = Vec::new();
    for root in walk_document(payload) {
        root.visit(&mut |n| {
            if n.sid == Some(SID_READING_ORDERS) && matches!(n.kind, Kind::List) {
                for entry in &n.children {
                    let mut name = OrderName::None;
                    let mut sections = Vec::new();
                    for f in entry.fields() {
                        if f.sid == Some(SID_READING_ORDER_NAME) {
                            name = match &f.kind {
                                Kind::Symbol(s) => OrderName::Num(*s as i128),
                                Kind::Int(Some(v)) => OrderName::Num(*v),
                                _ => match string_value(f) {
                                    Some(s) => OrderName::Str(s),
                                    None => OrderName::None,
                                },
                            };
                        } else if f.sid == Some(SID_READING_ORDER_SECTIONS) {
                            sections = section_ids(f);
                        }
                    }
                    orders.push(ReadingOrder { name, sections });
                }
            }
        });
    }
    orders
}

/// Python `FRAG_ID_RE` full match.
fn frag_id_like(s: &str) -> bool {
    const SHORT: [&str; 14] = [
        "lEL", "iEL", "cEL", "llZFi", "clZFi", "tlZFi", "ilZFi", "lfA", "cfA", "tfA", "ifA",
        "lYJmp", "cYJmp", "tYJmp",
    ];
    if SHORT.contains(&s) {
        return true;
    }
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && s.chars().count() >= 7
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn string_refs(roots: &[Node<'_>]) -> Vec<String> {
    let mut out = Vec::new();
    for r in roots {
        r.visit(&mut |n| {
            if let Kind::String(v) = &n.kind {
                if frag_id_like(v) || (v.contains('-') && v.chars().count() >= 10) {
                    out.push(v.clone());
                }
            }
        });
    }
    out
}

fn reachable(root: &str, adj: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&str> = if adj.contains_key(root) {
        vec![root]
    } else {
        Vec::new()
    };
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.to_string()) {
            continue;
        }
        if let Some(next) = adj.get(cur) {
            stack.extend(
                next.iter()
                    .map(String::as_str)
                    .filter(|n| !seen.contains(*n)),
            );
        }
    }
    seen
}

fn page_fields(
    payload: &[u8],
    nm: &Nmdl,
) -> (i128, i128, Option<f64>, Option<String>, Vec<&'static str>) {
    let (mut w, mut h, mut ppi, mut tmpl) = (None, None, None, None);
    for root in walk_document(payload) {
        root.visit(&mut |n| {
            let Some(sid) = n.sid else { return };
            let num = match n.kind {
                Kind::Int(Some(v)) => Some(v as f64),
                Kind::Float(Some(f)) => Some(f),
                _ => None,
            };
            if Some(sid) == nm.canvas_width {
                if let Some(v) = num {
                    w = Some(v.trunc() as i128);
                }
            } else if Some(sid) == nm.canvas_height {
                if let Some(v) = num {
                    h = Some(v.trunc() as i128);
                }
            } else if Some(sid) == nm.normalized_ppi {
                if let Some(v) = num {
                    ppi = Some(v);
                }
            } else if Some(sid) == nm.template_id {
                if let Some(s) = string_value(n) {
                    tmpl = Some(s);
                }
            }
        });
    }
    let mut fallbacks = Vec::new();
    let w = w.unwrap_or_else(|| {
        fallbacks.push("canvas_width_default");
        DEFAULT_CANVAS_WIDTH
    });
    let h = h.unwrap_or_else(|| {
        fallbacks.push("canvas_height_default");
        DEFAULT_CANVAS_HEIGHT
    });
    if ppi.is_none() {
        fallbacks.push("normalized_ppi_default");
        ppi = Some(2520.0);
    }
    (w, h, ppi, tmpl, fallbacks)
}

fn bounds4(n: &Node<'_>) -> Option<[i64; 4]> {
    let l = n.unwrap();
    if !matches!(l.kind, Kind::List) || l.children.len() != 4 {
        return None;
    }
    let mut out = [0i64; 4];
    for (o, c) in out.iter_mut().zip(&l.children) {
        *o = i64::try_from(c.as_int()?).ok()?;
    }
    Some(out)
}

fn decode_stroke(node: &Node<'_>, nm: &Nmdl) -> DecodedStroke {
    let mut s = DecodedStroke {
        brush_type: node.field(nm.brush_type).and_then(Node::as_int),
        color: node.field(nm.color).and_then(Node::as_int),
        thickness: node.field(nm.thickness).and_then(Node::as_f64),
        ..Default::default()
    };
    let bounds = node.field(nm.stroke_bounds).and_then(bounds4);
    if bounds.is_none() {
        s.diagnostics.push("missing_bounds".into());
    }
    let Some(sp) = node.field(nm.stroke_points) else {
        s.diagnostics.push("missing_stroke_points".into());
        return s;
    };
    let num_points = sp.field(nm.num_points).and_then(Node::as_int);
    let channel = |sid: Option<u64>, expected: Option<usize>| -> Option<Result<Vec<i64>, String>> {
        sp.field(sid)
            .and_then(Node::as_blob)
            .map(|b| decode_channel(b, expected))
    };
    let xs = channel(nm.position_x, None);
    let ys = channel(nm.position_y, None);
    let (xs, ys) = match (xs, ys) {
        (Some(Ok(x)), Some(Ok(y))) => (x, y),
        (x, y) => {
            for (c, name) in [(x, "position_x"), (y, "position_y")] {
                match c {
                    None => s.diagnostics.push(format!("missing_channel {name}")),
                    Some(Err(e)) => s.diagnostics.push(format!("channel_error {name}: {e}")),
                    Some(Ok(_)) => {}
                }
            }
            return s;
        }
    };
    if xs.len() != ys.len() {
        s.diagnostics
            .push(format!("xy_length_mismatch x={} y={}", xs.len(), ys.len()));
        return s;
    }
    if let Some(n) = num_points {
        if n != xs.len() as i128 {
            s.diagnostics
                .push(format!("num_points_mismatch {n} vs {}", xs.len()));
        }
    }
    if xs.is_empty() {
        s.diagnostics.push("too_few_points".into());
    } else if let Some([x0, y0, _, _]) = bounds {
        s.xs = xs.iter().map(|x| x + x0).collect();
        s.ys = ys.iter().map(|y| y + y0).collect();
    } else {
        s.xs = xs.clone();
        s.ys = ys;
    }
    match channel(nm.pressure, Some(xs.len())) {
        Some(Ok(p)) => s.pressure = Some(p),
        Some(Err(e)) => s.diagnostics.push(format!("channel_error pressure: {e}")),
        None => {}
    }
    for sid in nm.optional_channels {
        if let Some(Err(e)) = channel(sid, Some(xs.len())) {
            s.diagnostics.push(format!("channel_error optional: {e}"));
        }
    }
    s
}

fn strokes_in_fragment(payload: &[u8], nm: &Nmdl) -> Vec<DecodedStroke> {
    let (Some(kind), Some(stroke)) = (nm.kind, nm.stroke) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in walk_document(payload) {
        root.visit(&mut |n| {
            if n.is_struct() {
                if let Some(t) = n.field(Some(kind)) {
                    if t.as_symbol() == Some(stroke) {
                        out.push(decode_stroke(n, nm));
                    }
                }
            }
        });
    }
    out
}

fn book_id(store: &FragmentStore) -> Option<String> {
    let payload = store.get("book_metadata")?;
    let mut found = None;
    for root in walk_document(payload) {
        root.visit(&mut |n| {
            if found.is_none()
                && n.is_struct()
                && n.field(Some(SID_METADATA_KEY)).and_then(Node::as_str) == Some("book_id")
            {
                found = n
                    .field(Some(SID_METADATA_VALUE))
                    .and_then(Node::as_str)
                    .map(str::to_string);
            }
        });
    }
    found
}

/// Decode the materialized notebook state of an effective fragment store.
pub fn decode_store(store: &FragmentStore) -> DecodedNotebook {
    let (symbols, yj_max) = Symbols::parse(store);
    let nm = Nmdl::new(&symbols);

    let mut orders = Vec::new();
    for root_id in ["document_data", "metadata"] {
        if let Some(p) = store.get(root_id) {
            orders = reading_orders(p);
            if !orders.is_empty() {
                break;
            }
        }
    }
    let mut live: Vec<String> = Vec::new();
    let mut templates: Vec<String> = Vec::new();
    for o in &orders {
        match &o.name {
            OrderName::Num(SID_MAIN_READING_ORDER) => live = o.sections.clone(),
            OrderName::Str(s) if s == NOTE_TEMPLATE_COLLECTION => templates = o.sections.clone(),
            _ => {}
        }
    }
    if live.is_empty()
        && orders.len() == 1
        && !matches!(&orders[0].name, OrderName::Str(s) if s == NOTE_TEMPLATE_COLLECTION)
    {
        live = orders[0].sections.clone();
    }

    // Fragment types, string-reference adjacency (one walk per fragment).
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut page_sections = Vec::new();
    let mut diag = Diagnostics {
        fingerprints_removed: store.fingerprints_removed,
        base_fragment_count: store.base_count,
        delta_count: store.delta_count,
        delta_tombstone_count: store.delta_deleted_count,
        live_fragment_count: store.fragments.len(),
        yj_import_max_id: yj_max,
        local_symbol_count: symbols.local.len(),
        reading_order_count: orders.len(),
        ..Default::default()
    };
    for (fid, payload) in &store.fragments {
        let edges = adj.entry(fid.clone()).or_default();
        if payload.is_empty() {
            continue;
        }
        if !is_ion(payload) {
            diag.non_ion_payloads += 1;
            continue;
        }
        let roots = walk_document(payload);
        if let Some(Kind::Annotation(anns)) = roots.first().map(|r| &r.kind) {
            if anns.first() == Some(&SID_PAGE_SECTION) {
                page_sections.push(fid.clone());
            }
        }
        for r in string_refs(&roots) {
            if r != *fid && store.fragments.contains_key(&r) && edges.insert(r) {
                diag.structural_edges += 1;
            }
        }
    }
    let referenced: BTreeSet<&String> = live.iter().chain(&templates).collect();
    diag.orphan_page_count = page_sections
        .iter()
        .filter(|p| !referenced.contains(p))
        .count();

    let mut pages = Vec::new();
    for pid in &live {
        let Some(payload) = store.get(pid) else {
            pages.push(DecodedPage {
                page_id: pid.clone(),
                missing: true,
                width: DEFAULT_CANVAS_WIDTH,
                height: DEFAULT_CANVAS_HEIGHT,
                field_fallbacks: vec!["missing_fragment"],
                ..Default::default()
            });
            continue;
        };
        let (width, height, normalized_ppi, template_id, field_fallbacks) =
            page_fields(payload, &nm);
        let mut strokes = Vec::new();
        for fid in reachable(pid, &adj) {
            if let Some(p) = store.get(&fid) {
                if !p.is_empty() {
                    strokes.extend(strokes_in_fragment(p, &nm));
                }
            }
        }
        pages.push(DecodedPage {
            page_id: pid.clone(),
            missing: false,
            width,
            height,
            normalized_ppi,
            template_id,
            field_fallbacks,
            strokes,
        });
    }
    DecodedNotebook {
        pages,
        template_ids: templates,
        book_id: book_id(store),
        diagnostics: diag,
    }
}
