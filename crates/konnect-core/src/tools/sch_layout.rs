//! `check_schematic_layout` — text collisions, off-page items, wires drawn
//! through symbol bodies, and the other drawing-quality rules a reviewer
//! notices that ERC is blind to.
//!
//! This is a port of `hardware/io-expander/tools/sch_layout_check.py` from
//! BoatDash (see `docs/konnect-mcp-issues.md` #15 there): `.kicad_sch` gives
//! exact symbol/label/wire/sheet geometry, and kicad-cli's SVG export gives
//! exact *text* extents — every string is written a second time as an
//! invisible `<text opacity="0">` element carrying `x`, `y`, `textLength`,
//! `font-size`, `text-anchor`, and a `rotate()` wrapper for vertical text.
//! `check_schematic_overlaps` (in `sch_analysis`) ignores text and reports
//! every power symbol sitting on a pin as an "overlap" — the normal idiom —
//! which is why this exists as a separate, additive tool rather than a change
//! to that one.
//!
//! Scope note: each call checks exactly the one `.kicad_sch` file named by
//! `schematic` (root or a sub-sheet) — the same granularity the Python
//! reference uses when invoked as `sch_layout_check.py *.kicad_sch`. It does
//! not recurse into hierarchical sub-sheets automatically.

use crate::gates::GateStatus;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::cli::{export_schematic_svg, SchematicSvgOptions};
use crate::tools::{get_path, invalid_arg, opt_f64, opt_str_list, ToolContext, ToolDef};
use konnect_sexp::schematic::{
    extract_labels, extract_lib_pins_for_unit, extract_symbol_instances, extract_wires,
    find_lib_symbol, parse_at, parse_lib_pin, pin_endpoint, read_schematic,
    symbol_graphics_bounds_for_instance, Label, LabelKind, SymbolInstance, Wire,
};
use konnect_sexp::SexpNode;
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub fn tools() -> Vec<ToolDef> {
    vec![tool!(
        "check_schematic_layout",
        "Lint schematic *drawing* quality: off-page items, text overlapping other text, a \
         symbol body, a power symbol's arrow, or a sheet box; symbols crowding or overlapping \
         each other; wires drawn through a symbol body or a text string; Reference/Value fields \
         drifting away from their symbol; hierarchical sheet pins whose angle or justify puts \
         their text outside the box; a library pin meeting its body at a corner; duplicate \
         labels; and the drawing sitting off-centre on the page. Electrical correctness is \
         run_erc's job — this is what a reviewer sees first. Requires kicad-cli (for exact text \
         extents from its SVG export); returns a BLOCKED status, never a silent pass, when the \
         export or its SVG cannot be read.",
        json!({ "type": "object",
            "properties": {
                "schematic": {
                    "type": "string",
                    "description": "Path to one .kicad_sch file — the root sheet or any sub-sheet"
                },
                "margin_mm": {
                    "type": "number",
                    "description": "How far inside the page edge counts as the drawable frame",
                    "default": 12.0
                },
                "rules": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only report findings for these rule names, e.g. \
                        [\"off_page\", \"text_overlap\"]. Omit to run every rule."
                }
            },
            "required": ["schematic"] }),
        |args, ctx| async move { handle_check_schematic_layout(args, ctx).await }
    )]
}

// ─── Geometry primitives ──────────────────────────────────────────────────────

/// Axis-aligned bounding box in schematic millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BBox {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl BBox {
    fn point(x: f64, y: f64) -> Self {
        BBox {
            x0: x,
            y0: y,
            x1: x,
            y1: y,
        }
    }

    fn of_points<I: IntoIterator<Item = (f64, f64)>>(pts: I) -> Option<Self> {
        let mut it = pts.into_iter();
        let (x, y) = it.next()?;
        let mut b = BBox::point(x, y);
        for (x, y) in it {
            b.x0 = b.x0.min(x);
            b.y0 = b.y0.min(y);
            b.x1 = b.x1.max(x);
            b.y1 = b.y1.max(y);
        }
        Some(b)
    }

    fn union(self, o: BBox) -> BBox {
        BBox {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    /// True when the boxes overlap, or (with a positive `tol`) are closer
    /// than `tol`. A negative `tol` loosens the test — used to treat
    /// near-duplicate SVG strokes of the same string as "the same box".
    fn intersects(self, o: BBox, tol: f64) -> bool {
        !(self.x1 - tol <= o.x0
            || o.x1 - tol <= self.x0
            || self.y1 - tol <= o.y0
            || o.y1 - tol <= self.y0)
    }

    fn contains_pt(self, x: f64, y: f64, tol: f64) -> bool {
        self.x0 - tol <= x && x <= self.x1 + tol && self.y0 - tol <= y && y <= self.y1 + tol
    }

    /// Shortest distance between two boxes; 0 when they touch or overlap.
    fn gap(self, o: BBox) -> f64 {
        let dx = (o.x0 - self.x1).max(self.x0 - o.x1).max(0.0);
        let dy = (o.y0 - self.y1).max(self.y0 - o.y1).max(0.0);
        dx.hypot(dy)
    }
}

impl From<konnect_sexp::schematic::SymbolBounds> for BBox {
    fn from(b: konnect_sexp::schematic::SymbolBounds) -> Self {
        BBox {
            x0: b.min_x,
            y0: b.min_y,
            x1: b.max_x,
            y1: b.max_y,
        }
    }
}

/// Axis-aligned wire segment (KiCad wires are always H or V) vs a box.
fn seg_hits_box(x1: f64, y1: f64, x2: f64, y2: f64, b: BBox, tol: f64) -> bool {
    if (y1 - y2).abs() < 1e-6 {
        let (lo, hi) = (x1.min(x2), x1.max(x2));
        return b.y0 + tol < y1 && y1 < b.y1 - tol && lo < b.x1 - tol && hi > b.x0 + tol;
    }
    if (x1 - x2).abs() < 1e-6 {
        let (lo, hi) = (y1.min(y2), y1.max(y2));
        return b.x0 + tol < x1 && x1 < b.x1 - tol && lo < b.y1 - tol && hi > b.y0 + tol;
    }
    false
}

fn round_key(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

// ─── Schematic model ──────────────────────────────────────────────────────────

const PAPER_MM: &[(&str, (f64, f64))] = &[
    ("A0", (1189.0, 841.0)),
    ("A1", (841.0, 594.0)),
    ("A2", (594.0, 420.0)),
    ("A3", (420.0, 297.0)),
    ("A4", (297.0, 210.0)),
    ("A5", (210.0, 148.0)),
    ("A", (279.4, 215.9)),
    ("B", (431.8, 279.4)),
    ("C", (558.8, 431.8)),
    ("D", (863.6, 558.8)),
    ("E", (1117.6, 863.6)),
    ("USLetter", (279.4, 215.9)),
    ("USLegal", (355.6, 215.9)),
    ("USLedger", (431.8, 279.4)),
];

fn paper_size(tree: &SexpNode) -> (f64, f64) {
    let Some(paper) = tree.find("paper") else {
        return (297.0, 210.0);
    };
    let name = paper.get(1).and_then(SexpNode::as_str).unwrap_or("A4");
    let mut size = PAPER_MM
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .unwrap_or((297.0, 210.0));
    let portrait = paper
        .children()
        .unwrap_or(&[])
        .iter()
        .any(|c| c.as_str() == Some("portrait"));
    if portrait {
        size = (size.1, size.0);
    }
    size
}

struct Field {
    name: String,
    text: String,
    x: f64,
    y: f64,
    hidden: bool,
}

struct Sym {
    reference: String,
    lib_name: String,
    x: f64,
    y: f64,
    body: Option<BBox>, // graphics only
    pins: Vec<(f64, f64, String)>,
    fields: Vec<Field>,
    is_power: bool,
}

impl Sym {
    /// Body unioned with pin endpoints — falls back to the placement origin
    /// when neither is known (an unresolved library definition).
    fn full(&self) -> BBox {
        let pin_box = BBox::of_points(self.pins.iter().map(|(x, y, _)| (*x, *y)));
        match (self.body, pin_box) {
            (Some(b), Some(p)) => b.union(p),
            (Some(b), None) => b,
            (None, Some(p)) => p,
            (None, None) => BBox::point(self.x, self.y),
        }
    }
}

fn extract_fields(node: &SexpNode) -> Vec<Field> {
    node.find_all("property")
        .into_iter()
        .filter_map(|p| {
            let name = p.get(1)?.as_str()?.to_string();
            let text = p.get(2)?.as_str()?.to_string();
            let at = p.find("at")?;
            let x = at.get_f64(1)?;
            let y = at.get_f64(2)?;
            let mut hidden = p
                .find("hide")
                .map(|h| h.get(1).and_then(SexpNode::as_str).unwrap_or("yes") == "yes")
                .unwrap_or(false);
            if let Some(effects) = p.find("effects") {
                if effects.find("hide").is_some() {
                    hidden = true;
                }
            }
            Some(Field {
                name,
                text,
                x,
                y,
                hidden,
            })
        })
        .collect()
}

fn build_syms(tree: &SexpNode) -> Vec<Sym> {
    let lib_syms: Vec<&SexpNode> = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    // extract_symbol_instances applies the same two-condition filter
    // (has lib_id, has a parseable `at`) as this raw scan, so the two lists
    // stay in lock-step order — the raw nodes are only needed for `property`
    // (field) positions, which the typed instance does not carry.
    let raw_nodes: Vec<&SexpNode> = tree
        .find_all("symbol")
        .into_iter()
        .filter(|n| n.find("lib_id").is_some() && parse_at(n).is_some())
        .collect();
    let instances: Vec<SymbolInstance> = extract_symbol_instances(tree);

    raw_nodes
        .into_iter()
        .zip(instances)
        .map(|(node, inst)| {
            let lib_node = find_lib_symbol(&lib_syms, &inst);
            let is_power = lib_node.map(|n| n.find("power").is_some()).unwrap_or(false);
            let body = lib_node
                .and_then(|n| symbol_graphics_bounds_for_instance(n, &inst))
                .map(BBox::from);
            let transform = inst.pin_transform();
            let pins = lib_node
                .map(|n| {
                    extract_lib_pins_for_unit(n, inst.unit)
                        .into_iter()
                        .map(|p| {
                            let (x, y) = pin_endpoint(&p, transform);
                            (x, y, p.number)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Sym {
                reference: inst.reference.clone(),
                lib_name: inst.lib_symbol_name().to_string(),
                x: inst.x,
                y: inst.y,
                body,
                pins,
                fields: extract_fields(node),
                is_power,
            }
        })
        .collect()
}

struct SheetPin {
    name: String,
    x: f64,
    y: f64,
    rot: f64,
    justify: String,
}

struct Sheet {
    name: String,
    file: String,
    bbox: BBox,
    pins: Vec<SheetPin>,
    name_at: (f64, f64),
    file_at: (f64, f64),
}

fn build_sheets(tree: &SexpNode) -> Vec<Sheet> {
    tree.find_all("sheet")
        .into_iter()
        .filter_map(|sh| {
            let at = sh.find("at")?;
            let (x, y) = (at.get_f64(1)?, at.get_f64(2)?);
            let sz = sh.find("size")?;
            let (w, h) = (sz.get_f64(1)?, sz.get_f64(2)?);

            let mut name = String::new();
            let mut file = String::new();
            let mut name_at = (x, y);
            let mut file_at = (x, y);
            for p in sh.find_all("property") {
                let pname = p.get(1).and_then(SexpNode::as_str).unwrap_or("");
                let ptext = p
                    .get(2)
                    .and_then(SexpNode::as_str)
                    .unwrap_or("")
                    .to_string();
                let pat = p
                    .find("at")
                    .and_then(|a| Some((a.get_f64(1)?, a.get_f64(2)?)))
                    .unwrap_or((x, y));
                match pname {
                    "Sheetname" => {
                        name = ptext;
                        name_at = pat;
                    }
                    "Sheetfile" => {
                        file = ptext;
                        file_at = pat;
                    }
                    _ => {}
                }
            }

            let pins = sh
                .find_all("pin")
                .into_iter()
                .filter_map(|pn| {
                    let pname = pn.get(1).and_then(SexpNode::as_str)?.to_string();
                    let pat = pn.find("at")?;
                    let px = pat.get_f64(1)?;
                    let py = pat.get_f64(2)?;
                    let prot = pat.get_f64(3).unwrap_or(0.0);
                    let justify = pn
                        .find("effects")
                        .and_then(|e| e.find("justify"))
                        .and_then(|j| j.get(1))
                        .and_then(SexpNode::as_str)
                        .unwrap_or("left")
                        .to_string();
                    Some(SheetPin {
                        name: pname,
                        x: px,
                        y: py,
                        rot: prot,
                        justify,
                    })
                })
                .collect();

            Some(Sheet {
                name,
                file,
                bbox: BBox {
                    x0: x,
                    y0: y,
                    x1: x + w,
                    y1: y + h,
                },
                pins,
                name_at,
                file_at,
            })
        })
        .collect()
}

struct Note {
    text: String,
    x: f64,
    y: f64,
    size: f64,
}

fn build_notes(tree: &SexpNode) -> Vec<Note> {
    tree.find_all("text")
        .into_iter()
        .filter_map(|t| {
            let text = t.get(1)?.as_str()?.to_string();
            let at = t.find("at")?;
            let x = at.get_f64(1)?;
            let y = at.get_f64(2)?;
            let size = t
                .find("effects")
                .and_then(|e| e.find("font"))
                .and_then(|f| f.find("size"))
                .and_then(|s| s.get_f64(1))
                .unwrap_or(1.27);
            Some(Note { text, x, y, size })
        })
        .collect()
}

/// A library pin meeting its drawn body at a corner (the pin number then
/// prints on the corner and the part looks mis-drawn). Rectangles thinner
/// than `MIN_BODY_SIDE` are pin marks (connector stubs, jumper pads), not
/// bodies. Unlike every other check here this ignores unit selection — it
/// runs once over the whole library definition, matching the reference tool.
const CORNER_TOL: f64 = 1.0;
const MIN_BODY_SIDE: f64 = 1.5;

fn collect_rects_and_pins(
    node: &SexpNode,
    rects: &mut Vec<(f64, f64, f64, f64)>,
    pins: &mut Vec<(f64, f64, f64, f64, String)>,
) {
    for r in node.find_all("rectangle") {
        if let (Some(s), Some(e)) = (r.find("start"), r.find("end")) {
            if let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                (s.get_f64(1), s.get_f64(2), e.get_f64(1), e.get_f64(2))
            {
                rects.push((x0, y0, x1, y1));
            }
        }
    }
    for p in node.find_all("pin") {
        if let Some(lp) = parse_lib_pin(p) {
            pins.push((lp.local_x, lp.local_y, lp.rotation, lp.length, lp.number));
        }
    }
    for child in node.find_all("symbol") {
        collect_rects_and_pins(child, rects, pins);
    }
}

fn lib_pin_corner_issues(sym_node: &SexpNode) -> Vec<String> {
    let mut rects = Vec::new();
    let mut pins = Vec::new();
    collect_rects_and_pins(sym_node, &mut rects, &mut pins);
    let bodies: Vec<_> = rects
        .into_iter()
        .filter(|(x0, y0, x1, y1)| (x1 - x0).abs().min((y1 - y0).abs()) >= MIN_BODY_SIDE)
        .collect();
    let mut out = Vec::new();
    for (px, py, ang, len, pnum) in &pins {
        let a = ang.to_radians();
        let (ex, ey) = (px + len * a.cos(), py + len * a.sin());
        for (x0, y0, x1, y1) in &bodies {
            for cx in [*x0, *x1] {
                for cy in [*y0, *y1] {
                    if (ex - cx).hypot(ey - cy) < CORNER_TOL {
                        out.push(format!(
                            "pin {pnum} meets the body at its corner ({cx:.3},{cy:.3})"
                        ));
                    }
                }
            }
        }
    }
    out
}

// Sheet pin edge/orientation rules — a pin's angle tells KiCad which edge it
// is on (0 right, 180 left, 90 top, 270 bottom) and KiCad snaps the
// connection point to that edge; the justify decides which way the *text*
// reads. See `docs/agents/kicad-schematic.md`'s drawing-rules table.
fn sheet_pin_rot_for_edge(edge: &str) -> f64 {
    match edge {
        "left" => 180.0,
        "right" => 0.0,
        "top" => 90.0,
        _ => 270.0, // "bottom"
    }
}

fn sheet_pin_justify_for_edge(edge: &str) -> &'static str {
    match edge {
        "left" => "left",
        "right" => "right",
        "top" => "right",
        _ => "left", // "bottom"
    }
}

fn sheet_pin_edge(sh: &Sheet, x: f64, y: f64) -> Option<&'static str> {
    const TOL: f64 = 0.05;
    let b = sh.bbox;
    if (x - b.x0).abs() < TOL && b.y0 - TOL <= y && y <= b.y1 + TOL {
        return Some("left");
    }
    if (x - b.x1).abs() < TOL && b.y0 - TOL <= y && y <= b.y1 + TOL {
        return Some("right");
    }
    if (y - b.y0).abs() < TOL && b.x0 - TOL <= x && x <= b.x1 + TOL {
        return Some("top");
    }
    if (y - b.y1).abs() < TOL && b.x0 - TOL <= x && x <= b.x1 + TOL {
        return Some("bottom");
    }
    None
}

// ─── SVG text extraction ──────────────────────────────────────────────────────

struct SvgText {
    text: String,
    bbox: BBox,
    x: f64,
    y: f64,
}

/// Parse a kicad-cli-exported SVG document. kicad-cli always writes a
/// `<!DOCTYPE svg PUBLIC ...>` prolog, which roxmltree's default options
/// reject outright (`DtdDetected`) since a DTD can define entities — not a
/// concern here, where the document is our own kicad-cli's output, not
/// arbitrary untrusted SVG.
fn parse_svg_document(text: &str) -> Result<roxmltree::Document<'_>, roxmltree::Error> {
    roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        },
    )
}

/// Parse `rotate(angle cx cy)` out of an SVG `transform` attribute.
fn parse_rotate(transform: &str) -> Option<(f64, f64, f64)> {
    let start = transform.find("rotate(")? + "rotate(".len();
    let end = transform[start..].find(')')? + start;
    let mut nums = transform[start..end]
        .split_whitespace()
        .map(|s| s.parse::<f64>().ok());
    Some((nums.next()??, nums.next()??, nums.next()??))
}

fn collect_svg_texts(node: roxmltree::Node, rot: Option<(f64, f64, f64)>, out: &mut Vec<SvgText>) {
    let mut rot = rot;
    if let Some(tr) = node.attribute("transform") {
        if let Some(r) = parse_rotate(tr) {
            rot = Some(r);
        }
    }
    if node.tag_name().name() == "text" && node.attribute("opacity") == Some("0") {
        let text: String = node
            .descendants()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect();
        if text.trim().is_empty() {
            return;
        }
        let (Some(x), Some(y)) = (
            node.attribute("x").and_then(|v| v.parse::<f64>().ok()),
            node.attribute("y").and_then(|v| v.parse::<f64>().ok()),
        ) else {
            return;
        };
        let w = node
            .attribute("textLength")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let fs = node
            .attribute("font-size")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1.27);
        let anchor = node.attribute("text-anchor").unwrap_or("start");
        let x0 = match anchor {
            "middle" => x - w / 2.0,
            "end" => x - w,
            _ => x,
        };
        // KiCad's stroke font: glyphs reach ~0.75 em above the baseline,
        // ~0.2 em below.
        let mut corners = [
            (x0, y - 0.75 * fs),
            (x0 + w, y - 0.75 * fs),
            (x0, y + 0.2 * fs),
            (x0 + w, y + 0.2 * fs),
        ];
        if let Some((a, cx, cy)) = rot {
            let (ca, sa) = (a.to_radians().cos(), a.to_radians().sin());
            for p in &mut corners {
                let (px, py) = *p;
                *p = (
                    cx + (px - cx) * ca - (py - cy) * sa,
                    cy + (px - cx) * sa + (py - cy) * ca,
                );
            }
        }
        out.push(SvgText {
            text,
            bbox: BBox::of_points(corners).expect("4 corners"),
            x,
            y,
        });
        return;
    }
    for child in node.children().filter(|n| n.is_element()) {
        collect_svg_texts(child, rot, out);
    }
}

/// Nearest unused text in `svgs` with exactly this content, within `radius`
/// mm of `(x, y)`. Marks the match used so the same rendered string cannot
/// satisfy two different labels/fields.
fn find_match(
    svgs: &[SvgText],
    used: &mut [bool],
    text: &str,
    x: f64,
    y: f64,
    radius: f64,
) -> Option<BBox> {
    let mut best: Option<(usize, f64)> = None;
    for (i, t) in svgs.iter().enumerate() {
        if used[i] || t.text != text {
            continue;
        }
        let d = (t.x - x).hypot(t.y - y);
        if d <= radius && best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((i, d));
        }
    }
    let (i, _) = best?;
    used[i] = true;
    Some(svgs[i].bbox)
}

// ─── Checks ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Label,
    Field,
    Note,
    SheetName,
    SheetFile,
    SheetPin,
    PinText,
}

impl What {
    fn label(self) -> &'static str {
        match self {
            What::Label => "label",
            What::Field => "field",
            What::Note => "note",
            What::SheetName => "sheet_name",
            What::SheetFile => "sheet_file",
            What::SheetPin => "sheet_pin",
            What::PinText => "pin_text",
        }
    }
}

struct Tracked {
    what: What,
    owner: String,
    text: String,
    bbox: BBox,
    anchor: (f64, f64),
}

#[derive(Debug, Clone)]
struct Finding {
    rule: &'static str,
    severity: &'static str,
    message: String,
    x: f64,
    y: f64,
}

struct Checker {
    findings: Vec<Finding>,
}

impl Checker {
    fn add(&mut self, rule: &'static str, severity: &'static str, message: String, x: f64, y: f64) {
        self.findings.push(Finding {
            rule,
            severity,
            message,
            x,
            y,
        });
    }
    fn warn(&mut self, rule: &'static str, message: String, x: f64, y: f64) {
        self.add(rule, "warn", message, x, y);
    }
    fn error(&mut self, rule: &'static str, message: String, x: f64, y: f64) {
        self.add(rule, "error", message, x, y);
    }
}

const TEXT_CLEAR: f64 = 0.15;
const SAME_ROW_CLEAR: f64 = 0.4;
const BODY_CLEAR: f64 = 0.4;
const SYMBOL_CLEAR: f64 = 2.54;
const FIELD_GAP: f64 = 4.5;
const POWER_FIELD_GAP: f64 = 2.0;
const CENTER_TOL: f64 = 0.10;

/// Everything `run_checks` needs, gathered from one parsed schematic plus its
/// kicad-cli SVG export. Bundled into one struct rather than passed as
/// separate arguments — this is a single cohesive "one schematic's worth of
/// geometry" unit, not a grab-bag. Every field is a borrow, so the struct
/// itself is `Copy`.
#[derive(Clone, Copy)]
struct LayoutModel<'a> {
    syms: &'a [Sym],
    labels: &'a [Label],
    notes: &'a [Note],
    wires: &'a [Wire],
    sheets: &'a [Sheet],
    lib_issues: &'a HashMap<String, Vec<String>>,
    svgs: &'a [SvgText],
    paper: (f64, f64),
}

#[allow(clippy::too_many_lines)]
fn run_checks(model: &LayoutModel, margin: f64) -> Vec<Finding> {
    let LayoutModel {
        syms,
        labels,
        notes,
        wires,
        sheets,
        lib_issues,
        svgs,
        paper,
    } = *model;
    let mut c = Checker {
        findings: Vec::new(),
    };
    let (w, h) = paper;
    let frame = BBox {
        x0: margin,
        y0: margin,
        x1: w - margin,
        y1: h - margin,
    };
    let mut used = vec![false; svgs.len()];
    let mut tracked: Vec<Tracked> = Vec::new();

    for l in labels {
        if let Some(bbox) = find_match(svgs, &mut used, &l.net, l.x, l.y, 25.0) {
            tracked.push(Tracked {
                what: What::Label,
                owner: l.net.clone(),
                text: l.net.clone(),
                bbox,
                anchor: (l.x, l.y),
            });
        }
    }
    for s in syms {
        for field in &s.fields {
            if field.hidden || field.text.is_empty() {
                continue;
            }
            if field.name != "Reference" && field.name != "Value" {
                continue;
            }
            if s.is_power && field.name == "Reference" {
                continue;
            }
            if let Some(bbox) = find_match(svgs, &mut used, &field.text, field.x, field.y, 25.0) {
                tracked.push(Tracked {
                    what: What::Field,
                    owner: s.reference.clone(),
                    text: format!("{}.{}=\"{}\"", s.reference, field.name, field.text),
                    bbox,
                    anchor: (field.x, field.y),
                });
            }
        }
    }
    for n in notes {
        let lines: Vec<&str> = n.text.lines().filter(|l| !l.trim().is_empty()).collect();
        let radius = (60.0_f64).max(3.0 * n.size * lines.len() as f64);
        for line in &lines {
            if let Some(bbox) = find_match(svgs, &mut used, line, n.x, n.y, radius) {
                tracked.push(Tracked {
                    what: What::Note,
                    owner: format!("note@({},{})", n.x, n.y),
                    text: line.chars().take(60).collect(),
                    bbox,
                    anchor: (n.x, n.y),
                });
            }
        }
    }
    for sh in sheets {
        if let Some(bbox) = find_match(svgs, &mut used, &sh.name, sh.name_at.0, sh.name_at.1, 25.0)
        {
            tracked.push(Tracked {
                what: What::SheetName,
                owner: sh.name.clone(),
                text: sh.name.clone(),
                bbox,
                anchor: sh.name_at,
            });
        }
        let file_label = format!("File: {}", sh.file);
        let file_bbox = find_match(
            svgs,
            &mut used,
            &file_label,
            sh.file_at.0,
            sh.file_at.1,
            25.0,
        )
        .or_else(|| find_match(svgs, &mut used, &sh.file, sh.file_at.0, sh.file_at.1, 25.0));
        if let Some(bbox) = file_bbox {
            tracked.push(Tracked {
                what: What::SheetFile,
                owner: sh.name.clone(),
                text: sh.file.clone(),
                bbox,
                anchor: sh.file_at,
            });
        }
        for pin in &sh.pins {
            if let Some(bbox) = find_match(svgs, &mut used, &pin.name, pin.x, pin.y, 25.0) {
                tracked.push(Tracked {
                    what: What::SheetPin,
                    owner: sh.name.clone(),
                    text: format!("{}:{}", sh.name, pin.name),
                    bbox,
                    anchor: (pin.x, pin.y),
                });
            }
        }
    }

    // Every other rendered string (pin numbers, pin names, title block) is
    // the symbol's own lettering: checked against labels/fields, never
    // against itself. kicad-cli sometimes strokes a string twice; a leftover
    // sitting on one already tracked (or on another leftover) is the same
    // lettering, not a second string, and is dropped.
    fn same_string(a: BBox, text_a: &str, b: BBox, text_b: &str) -> bool {
        (text_b == text_a || text_b.ends_with(&format!("=\"{text_a}\""))) && a.intersects(b, -0.3)
    }
    let mut extra: Vec<Tracked> = Vec::new();
    for (i, t) in svgs.iter().enumerate() {
        if used[i] || t.text.trim().is_empty() {
            continue;
        }
        let dup = tracked
            .iter()
            .any(|tr| same_string(t.bbox, &t.text, tr.bbox, &tr.text))
            || extra
                .iter()
                .any(|e| same_string(t.bbox, &t.text, e.bbox, &e.text));
        if dup {
            continue;
        }
        extra.push(Tracked {
            what: What::PinText,
            owner: String::new(),
            text: t.text.clone(),
            bbox: t.bbox,
            anchor: (t.bbox.x0, t.bbox.y0),
        });
    }
    tracked.extend(extra);

    // 0a. hierarchical sheet pins: on an edge, angle agreeing with it, text
    // pointing into the box.
    for sh in sheets {
        for pin in &sh.pins {
            let Some(edge) = sheet_pin_edge(sh, pin.x, pin.y) else {
                c.error(
                    "sheet_pin_off_edge",
                    format!(
                        "sheet pin {}:{} is not on the sheet border",
                        sh.name, pin.name
                    ),
                    pin.x,
                    pin.y,
                );
                continue;
            };
            let want_rot = sheet_pin_rot_for_edge(edge);
            let rot_diff = (pin.rot - want_rot).rem_euclid(360.0);
            if rot_diff > 0.01 && (360.0 - rot_diff) > 0.01 {
                c.error(
                    "sheet_pin_side",
                    format!(
                        "sheet pin {}:{} sits on the {edge} edge but has angle {:.0}; \
                         KiCad will snap it to the other edge (want {want_rot:.0})",
                        sh.name, pin.name, pin.rot,
                    ),
                    pin.x,
                    pin.y,
                );
            }
            let want_justify = sheet_pin_justify_for_edge(edge);
            if pin.justify != want_justify {
                c.warn(
                    "sheet_pin_text_outside",
                    format!(
                        "sheet pin {}:{} on the {edge} edge has (justify {}); its text hangs \
                         outside the box (want {want_justify})",
                        sh.name, pin.name, pin.justify
                    ),
                    pin.x,
                    pin.y,
                );
            }
        }
    }

    // 0b. library drawing: a pin meeting the body at a corner. Reported once
    // per distinct library definition, at the position of its first placed
    // instance (matching the reference tool).
    let mut seen_lib: HashSet<&str> = HashSet::new();
    for s in syms {
        if let Some(msgs) = lib_issues.get(&s.lib_name) {
            if seen_lib.insert(&s.lib_name) {
                for msg in msgs {
                    c.warn(
                        "pin_at_corner",
                        format!(
                            "{} ({}): {msg} -- enlarge the body or move the pin",
                            s.lib_name, s.reference
                        ),
                        s.x,
                        s.y,
                    );
                }
            }
        }
    }

    // 0c. Reference/Value drifting away from their symbol.
    let mut field_owner: HashMap<(String, i64, i64), usize> = HashMap::new();
    for (i, s) in syms.iter().enumerate() {
        for field in &s.fields {
            field_owner.insert(
                (s.reference.clone(), round_key(field.x), round_key(field.y)),
                i,
            );
        }
    }
    for tr in &tracked {
        if tr.what != What::Field {
            continue;
        }
        let Some(&i) = field_owner.get(&(
            tr.owner.clone(),
            round_key(tr.anchor.0),
            round_key(tr.anchor.1),
        )) else {
            continue;
        };
        let s = &syms[i];
        if s.is_power {
            let ref_box = s.body.unwrap_or_else(|| s.full());
            let g = tr.bbox.gap(ref_box);
            if g > POWER_FIELD_GAP {
                c.warn(
                    "field_far",
                    format!("{} sits {g:.1} mm from its power symbol", tr.text),
                    tr.bbox.x0,
                    tr.bbox.y0,
                );
            }
        } else {
            let g = tr.bbox.gap(s.full());
            if g > FIELD_GAP {
                c.warn(
                    "field_far",
                    format!("{} sits {g:.1} mm from {}", tr.text, s.reference),
                    tr.bbox.x0,
                    tr.bbox.y0,
                );
            }
        }
    }

    // 0d. the drawing should sit roughly in the middle of the page.
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for s in syms {
        let b = s.full();
        xs.extend([b.x0, b.x1]);
        ys.extend([b.y0, b.y1]);
    }
    for l in labels {
        xs.push(l.x);
        ys.push(l.y);
    }
    for n in notes {
        xs.push(n.x);
        ys.push(n.y);
    }
    for sh in sheets {
        xs.extend([sh.bbox.x0, sh.bbox.x1]);
        ys.extend([sh.bbox.y0, sh.bbox.y1]);
    }
    if let (Some(&xmin), Some(&xmax), Some(&ymin), Some(&ymax)) = (
        xs.iter().min_by(|a, b| a.total_cmp(b)),
        xs.iter().max_by(|a, b| a.total_cmp(b)),
        ys.iter().min_by(|a, b| a.total_cmp(b)),
        ys.iter().max_by(|a, b| a.total_cmp(b)),
    ) {
        let (cx, cy) = ((xmin + xmax) / 2.0, (ymin + ymax) / 2.0);
        let (offx, offy) = (cx - w / 2.0, cy - h / 2.0);
        if offx.abs() > CENTER_TOL * w || offy.abs() > CENTER_TOL * h {
            c.warn(
                "content_off_center",
                format!(
                    "drawing centre is ({offx:+.0}, {offy:+.0}) mm from the page centre; \
                     content spans x {xmin:.0}..{xmax:.0}, y {ymin:.0}..{ymax:.0} on a \
                     {w:.1}x{h:.1} page -- shift everything by about ({:+.0}, {:+.0})",
                    -offx, -offy
                ),
                cx,
                cy,
            );
        }
    }

    // 1. off-page.
    for s in syms {
        let b = s.full();
        if !frame.contains_pt(b.x0, b.y0, 0.0) || !frame.contains_pt(b.x1, b.y1, 0.0) {
            c.error(
                "off_page",
                format!(
                    "symbol {} extends outside the frame: [{:.2},{:.2}..{:.2},{:.2}]",
                    s.reference, b.x0, b.y0, b.x1, b.y1
                ),
                s.x,
                s.y,
            );
        }
    }
    for l in labels {
        if !frame.contains_pt(l.x, l.y, 0.0) {
            c.error(
                "off_page",
                format!("label {} outside the frame", l.net),
                l.x,
                l.y,
            );
        }
    }
    for tr in &tracked {
        if tr.what == What::PinText {
            continue; // includes the title block, in the margin by design
        }
        if !frame.contains_pt(tr.bbox.x0, tr.bbox.y0, 0.0)
            || !frame.contains_pt(tr.bbox.x1, tr.bbox.y1, 0.0)
        {
            c.warn(
                "off_page",
                format!("text {} runs outside the frame", tr.text),
                tr.bbox.x0,
                tr.bbox.y0,
            );
        }
    }
    for w_ in wires {
        for (x, y) in [(w_.x1, w_.y1), (w_.x2, w_.y2)] {
            if !frame.contains_pt(x, y, 0.0) {
                c.error("off_page", "wire end outside the frame".to_string(), x, y);
                break;
            }
        }
    }
    for sh in sheets {
        if !frame.contains_pt(sh.bbox.x0, sh.bbox.y0, 0.0)
            || !frame.contains_pt(sh.bbox.x1, sh.bbox.y1, 0.0)
        {
            c.error(
                "off_page",
                format!("sheet {} extends outside the frame", sh.name),
                sh.bbox.x0,
                sh.bbox.y0,
            );
        }
    }

    // 2. text vs text.
    for i in 0..tracked.len() {
        for j in (i + 1)..tracked.len() {
            let (a, b) = (&tracked[i], &tracked[j]);
            if a.what == What::PinText && b.what == What::PinText {
                continue;
            }
            if (a.what == What::PinText || b.what == What::PinText)
                && (a.what == What::Note || b.what == What::Note)
            {
                continue;
            }
            if a.what == What::Field && b.what == What::Field && a.owner == b.owner {
                let stacked = a.bbox.y1 <= b.bbox.y0 + 0.01 || b.bbox.y1 <= a.bbox.y0 + 0.01;
                if stacked {
                    continue;
                }
                let g = a.bbox.gap(b.bbox);
                if g < SAME_ROW_CLEAR {
                    c.warn(
                        "text_touching",
                        format!("{}  ~  {} side by side, {g:.2} mm apart", a.text, b.text),
                        (a.bbox.x0 + b.bbox.x0) / 2.0,
                        (a.bbox.y0 + b.bbox.y0) / 2.0,
                    );
                }
                continue;
            }
            if a.bbox.intersects(b.bbox, 0.15) {
                if a.what == What::Label
                    && b.what == What::Label
                    && a.text == b.text
                    && (a.anchor.0 - b.anchor.0).hypot(a.anchor.1 - b.anchor.1) < 0.5
                {
                    c.warn(
                        "duplicate_label",
                        format!("two \"{}\" labels at the same point", a.text),
                        a.anchor.0,
                        a.anchor.1,
                    );
                } else {
                    c.warn(
                        "text_overlap",
                        format!(
                            "{} {}  <->  {} {}",
                            a.what.label(),
                            a.text,
                            b.what.label(),
                            b.text
                        ),
                        (a.bbox.x0 + b.bbox.x0) / 2.0,
                        (a.bbox.y0 + b.bbox.y0) / 2.0,
                    );
                }
            } else {
                let g = a.bbox.gap(b.bbox);
                if g < TEXT_CLEAR {
                    c.warn(
                        "text_touching",
                        format!(
                            "{} {}  ~  {} {} ({g:.2} mm apart)",
                            a.what.label(),
                            a.text,
                            b.what.label(),
                            b.text
                        ),
                        (a.bbox.x0 + b.bbox.x0) / 2.0,
                        (a.bbox.y0 + b.bbox.y0) / 2.0,
                    );
                }
            }
        }
    }

    // 3. text vs symbol body / sheet box.
    for tr in &tracked {
        if tr.what == What::PinText {
            continue;
        }
        for s in syms {
            let Some(body) = s.body else { continue };
            if tr.what == What::Field && tr.owner == s.reference {
                continue;
            }
            if s.is_power {
                if tr.bbox.intersects(body, 0.15) {
                    c.warn(
                        "text_on_symbol",
                        format!(
                            "{} {} runs over power symbol {} ({})",
                            tr.what.label(),
                            tr.text,
                            s.reference,
                            s.lib_name
                        ),
                        tr.bbox.x0,
                        tr.bbox.y0,
                    );
                }
                continue;
            }
            if tr.bbox.intersects(body, 0.15) {
                c.warn(
                    "text_on_symbol",
                    format!(
                        "{} {} runs over {} body",
                        tr.what.label(),
                        tr.text,
                        s.reference
                    ),
                    tr.bbox.x0,
                    tr.bbox.y0,
                );
            } else {
                let g = tr.bbox.gap(body);
                if g < BODY_CLEAR {
                    c.warn(
                        "text_near_symbol",
                        format!(
                            "{} {} is {g:.2} mm from {} body",
                            tr.what.label(),
                            tr.text,
                            s.reference
                        ),
                        tr.bbox.x0,
                        tr.bbox.y0,
                    );
                }
            }
        }
        for sh in sheets {
            if tr.owner == sh.name {
                continue;
            }
            if tr.bbox.intersects(sh.bbox, 0.15) {
                c.warn(
                    "text_on_sheet",
                    format!(
                        "{} {} runs over sheet \"{}\"",
                        tr.what.label(),
                        tr.text,
                        sh.name
                    ),
                    tr.bbox.x0,
                    tr.bbox.y0,
                );
            }
        }
    }

    // 4. symbol vs symbol.
    for i in 0..syms.len() {
        for j in (i + 1)..syms.len() {
            let (a, b) = (&syms[i], &syms[j]);
            if a.is_power || b.is_power {
                continue;
            }
            let (Some(ab), Some(bb)) = (a.body, b.body) else {
                continue;
            };
            if ab.intersects(bb, 0.2) {
                c.error(
                    "symbol_overlap",
                    format!("{} body overlaps {} body", a.reference, b.reference),
                    a.x,
                    a.y,
                );
                continue;
            }
            let g = ab.gap(bb);
            if g < SYMBOL_CLEAR - 0.01 {
                c.warn(
                    "symbol_crowding",
                    format!(
                        "{} and {} bodies are {g:.2} mm apart (want >= {SYMBOL_CLEAR} mm; \
                         there is sheet to spare)",
                        a.reference, b.reference
                    ),
                    a.x,
                    a.y,
                );
            }
            let mut hit = false;
            for (px, py, _) in &a.pins {
                if bb.contains_pt(*px, *py, -0.2) {
                    c.error(
                        "symbol_overlap",
                        format!("{} pin inside {} body", a.reference, b.reference),
                        *px,
                        *py,
                    );
                    hit = true;
                    break;
                }
            }
            if !hit {
                for (px, py, _) in &b.pins {
                    if ab.contains_pt(*px, *py, -0.2) {
                        c.error(
                            "symbol_overlap",
                            format!("{} pin inside {} body", b.reference, a.reference),
                            *px,
                            *py,
                        );
                        break;
                    }
                }
            }
        }
    }

    // 5. wire through a symbol body / through text.
    for w_ in wires {
        let (x1, y1, x2, y2) = (w_.x1, w_.y1, w_.x2, w_.y2);
        let ends: HashSet<(i64, i64)> = [
            (round_key(x1), round_key(y1)),
            (round_key(x2), round_key(y2)),
        ]
        .into_iter()
        .collect();
        for s in syms {
            let Some(body) = s.body else { continue };
            if s.is_power {
                continue;
            }
            if seg_hits_box(x1, y1, x2, y2, body, 0.1) {
                c.error(
                    "wire_through_symbol",
                    format!("wire ({x1},{y1})-({x2},{y2}) crosses {} body", s.reference),
                    x1,
                    y1,
                );
            }
        }
        for tr in &tracked {
            if tr.what == What::PinText {
                continue;
            }
            let (ax, ay) = tr.anchor;
            if ends.contains(&(round_key(ax), round_key(ay))) {
                continue;
            }
            // A label sitting on its own wire is the normal idiom, not a
            // collision.
            let on_wire = ((y1 - y2).abs() < 1e-6
                && (ay - y1).abs() < 0.02
                && x1.min(x2) - 0.02 <= ax
                && ax <= x1.max(x2) + 0.02)
                || ((x1 - x2).abs() < 1e-6
                    && (ax - x1).abs() < 0.02
                    && y1.min(y2) - 0.02 <= ay
                    && ay <= y1.max(y2) + 0.02);
            if on_wire && tr.what == What::Label {
                continue;
            }
            if seg_hits_box(x1, y1, x2, y2, tr.bbox, 0.1) {
                c.warn(
                    "wire_through_text",
                    format!(
                        "wire ({x1},{y1})-({x2},{y2}) crosses {} {}",
                        tr.what.label(),
                        tr.text
                    ),
                    x1,
                    y1,
                );
            }
        }
    }

    c.findings
}

/// Build the schematic-side model from a parsed `.kicad_sch` tree and run
/// every check against the matching set of SVG texts. Pure and
/// kicad-cli-free — split out of the handler so tests can feed it a static
/// SVG fixture instead of invoking kicad-cli.
fn analyze_schematic(tree: &SexpNode, svgs: &[SvgText], margin_mm: f64) -> Vec<Finding> {
    let paper = paper_size(tree);
    let syms = build_syms(tree);
    let labels = extract_labels(tree)
        .into_iter()
        .filter(|l| l.kind != LabelKind::PowerSymbol)
        .collect::<Vec<_>>();
    let notes = build_notes(tree);
    let wires = extract_wires(tree);
    let sheets = build_sheets(tree);

    let lib_syms: Vec<&SexpNode> = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_issues: HashMap<String, Vec<String>> = lib_syms
        .iter()
        .filter_map(|n| {
            let name = n.get(1)?.as_str()?.to_string();
            let issues = lib_pin_corner_issues(n);
            if issues.is_empty() {
                None
            } else {
                Some((name, issues))
            }
        })
        .collect();

    let model = LayoutModel {
        syms: &syms,
        labels: &labels,
        notes: &notes,
        wires: &wires,
        sheets: &sheets,
        lib_issues: &lib_issues,
        svgs,
        paper,
    };
    run_checks(&model, margin_mm)
}

// ─── Tool handler ─────────────────────────────────────────────────────────────

fn blocked_result(schematic: &str, margin_mm: f64, reason: String) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": GateStatus::Blocked,
        "schematic": schematic,
        "margin_mm": margin_mm,
        "count": 0,
        "counts_by_rule": {},
        "findings": [],
        "blocked_reason": reason
    }))
}

async fn handle_check_schematic_layout(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let margin_mm = opt_f64(args, "margin_mm").unwrap_or(12.0);
    if !margin_mm.is_finite() || margin_mm < 0.0 {
        return Ok(invalid_arg(
            "margin_mm",
            "must be a finite, non-negative number",
        ));
    }
    let rule_filter = match opt_str_list(args, "rules") {
        Ok(v) => v.map(|v| v.into_iter().collect::<HashSet<_>>()),
        Err(e) => return Ok(e),
    };

    let sheet_name = sch_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let schematic_display = sch_path.display().to_string();

    let (_, tree) = read_schematic(&sch_path)?;

    let out_dir = std::env::temp_dir()
        .join("konnect-schematic-layout")
        .join(&sheet_name);
    if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
        return Ok(blocked_result(
            &schematic_display,
            margin_mm,
            format!("could not create a scratch directory for the SVG export: {e}"),
        ));
    }
    let svg_path = match export_schematic_svg(
        &ctx.config.kicad_cli,
        &sch_path,
        &out_dir,
        &SchematicSvgOptions::default(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            return Ok(blocked_result(
                &schematic_display,
                margin_mm,
                format!("kicad-cli SVG export failed (is kicad-cli available?): {e}"),
            ))
        }
    };
    let svg_content = match tokio::fs::read_to_string(&svg_path).await {
        Ok(s) => s,
        Err(e) => {
            return Ok(blocked_result(
                &schematic_display,
                margin_mm,
                format!(
                    "could not read the exported SVG at {}: {e}",
                    svg_path.display()
                ),
            ))
        }
    };
    let svg_doc = match parse_svg_document(&svg_content) {
        Ok(d) => d,
        Err(e) => {
            return Ok(blocked_result(
                &schematic_display,
                margin_mm,
                format!("could not parse the exported SVG as XML: {e}"),
            ))
        }
    };
    let mut svgs = Vec::new();
    collect_svg_texts(svg_doc.root(), None, &mut svgs);

    let mut findings = analyze_schematic(&tree, &svgs, margin_mm);
    if let Some(rules) = &rule_filter {
        findings.retain(|f| rules.contains(f.rule));
    }
    findings.sort_by(|a, b| {
        (a.severity != "error")
            .cmp(&(b.severity != "error"))
            .then_with(|| a.rule.cmp(b.rule))
            .then_with(|| a.y.total_cmp(&b.y))
            .then_with(|| a.x.total_cmp(&b.x))
    });

    let mut counts_by_rule: HashMap<&str, usize> = HashMap::new();
    for f in &findings {
        *counts_by_rule.entry(f.rule).or_insert(0) += 1;
    }
    let has_error = findings.iter().any(|f| f.severity == "error");
    let status = if findings.is_empty() {
        GateStatus::Pass
    } else if has_error {
        GateStatus::Fail
    } else {
        GateStatus::Warn
    };

    let findings_json: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            json!({
                "rule": f.rule,
                "severity": f.severity,
                "message": f.message,
                "x_mm": (f.x * 100.0).round() / 100.0,
                "y_mm": (f.y * 100.0).round() / 100.0,
                "sheet": sheet_name,
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "status": status,
        "schematic": schematic_display,
        "sheet": sheet_name,
        "margin_mm": margin_mm,
        "count": findings.len(),
        "counts_by_rule": counts_by_rule,
        "findings": findings_json
    })))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn gap_is_zero_when_boxes_touch_or_overlap() {
        let a = BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let touching = BBox {
            x0: 10.0,
            y0: 0.0,
            x1: 20.0,
            y1: 10.0,
        };
        let overlapping = BBox {
            x0: 5.0,
            y0: 5.0,
            x1: 15.0,
            y1: 15.0,
        };
        assert_eq!(a.gap(touching), 0.0);
        assert_eq!(a.gap(overlapping), 0.0);
    }

    #[test]
    fn gap_measures_shortest_distance_between_separated_boxes() {
        let a = BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        // 3 mm clear on X, 4 mm clear on Y -> 5 mm diagonal (3-4-5 triangle).
        let b = BBox {
            x0: 13.0,
            y0: 14.0,
            x1: 20.0,
            y1: 20.0,
        };
        assert!((a.gap(b) - 5.0).abs() < 1e-9, "{}", a.gap(b));
    }

    #[test]
    fn intersects_negative_tolerance_treats_nearby_boxes_as_overlapping() {
        let a = BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let b = BBox {
            x0: 10.2,
            y0: 0.0,
            x1: 20.0,
            y1: 10.0,
        };
        assert!(
            !a.intersects(b, 0.0),
            "0.2 mm apart, must not touch at tol 0"
        );
        assert!(
            a.intersects(b, -0.3),
            "a negative tolerance must loosen the test enough to cover 0.2 mm"
        );
    }

    #[test]
    fn seg_hits_box_true_for_a_horizontal_wire_crossing_a_body() {
        let body = BBox {
            x0: 98.984,
            y0: 47.46,
            x1: 101.016,
            y1: 52.54,
        };
        assert!(seg_hits_box(90.0, 50.0, 110.0, 50.0, body, 0.1));
    }

    #[test]
    fn seg_hits_box_false_when_the_wire_only_touches_a_corner() {
        let body = BBox {
            x0: 98.984,
            y0: 47.46,
            x1: 101.016,
            y1: 52.54,
        };
        // Passes along the body's own top edge, not through its interior.
        assert!(!seg_hits_box(90.0, 47.46, 110.0, 47.46, body, 0.1));
    }

    #[test]
    fn seg_hits_box_false_for_a_wire_that_ends_before_the_body() {
        let body = BBox {
            x0: 98.984,
            y0: 47.46,
            x1: 101.016,
            y1: 52.54,
        };
        assert!(!seg_hits_box(90.0, 50.0, 95.0, 50.0, body, 0.1));
    }

    fn minimal_svg(body: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm" viewBox="0 0 10 10">{body}</svg>"#
        )
    }

    #[test]
    fn svg_text_bbox_uses_exact_textlength_and_font_size() {
        let svg = minimal_svg(
            r#"<text x="60.0" y="80.0" textLength="4.0" font-size="1.27"
                text-anchor="start" opacity="0" stroke-opacity="0">F1</text>"#,
        );
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let mut out = Vec::new();
        collect_svg_texts(doc.root(), None, &mut out);
        assert_eq!(out.len(), 1);
        let t = &out[0];
        assert_eq!(t.text, "F1");
        assert!((t.bbox.x0 - 60.0).abs() < 1e-9);
        assert!(
            (t.bbox.x1 - 64.0).abs() < 1e-9,
            "start-anchored: x0 + textLength"
        );
        assert!((t.bbox.y0 - (80.0 - 0.75 * 1.27)).abs() < 1e-9);
        assert!((t.bbox.y1 - (80.0 + 0.2 * 1.27)).abs() < 1e-9);
    }

    #[test]
    fn opacity_full_text_is_not_extracted() {
        // The visible stroked-outline text kicad-cli draws alongside the
        // invisible measurement text must not be double-counted.
        let svg = minimal_svg(
            r#"<text x="60.0" y="80.0" textLength="4.0" font-size="1.27"
                text-anchor="start">F1</text>"#,
        );
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let mut out = Vec::new();
        collect_svg_texts(doc.root(), None, &mut out);
        assert!(out.is_empty());
    }

    /// A field rotated 90 degrees, matching kicad-cli's
    /// `<g transform="rotate(a cx cy)"><text .../></g>` wrapper for vertical
    /// text. The unrotated box is 4 mm wide and ~1.2 mm tall; after a 90
    /// degree turn about its own center those dimensions must swap, proving
    /// the rotation is applied to the box corners, not just recorded.
    #[test]
    fn rotated_text_bbox_swaps_width_and_height() {
        let svg = minimal_svg(
            r#"<g transform="rotate(90.000000 60.0000 80.0000)">
                <text x="60.0" y="80.0" textLength="4.0" font-size="1.27"
                    text-anchor="middle" opacity="0" stroke-opacity="0">F1</text>
                </g>"#,
        );
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let mut out = Vec::new();
        collect_svg_texts(doc.root(), None, &mut out);
        assert_eq!(out.len(), 1);
        let t = &out[0];
        let em = 1.27_f64;
        let expected_thin_side = 0.75 * em + 0.2 * em; // was the height, pre-rotation
        assert!(
            (t.bbox.x1 - t.bbox.x0 - expected_thin_side).abs() < 1e-9,
            "width after rotation should be the pre-rotation height: {:?}",
            t.bbox
        );
        assert!(
            (t.bbox.y1 - t.bbox.y0 - 4.0).abs() < 1e-9,
            "height after rotation should be the pre-rotation width: {:?}",
            t.bbox
        );
    }

    #[test]
    fn parse_rotate_reads_angle_and_center() {
        assert_eq!(
            parse_rotate("rotate(-90.000000 60.7060 83.8200)"),
            Some((-90.0, 60.706, 83.82))
        );
        assert_eq!(parse_rotate("translate(1 2)"), None);
    }
}

#[cfg(test)]
mod pure_rule_tests {
    use super::*;
    use konnect_sexp::parse_sexp;

    /// Two `Device:R` placements (real KiCad 10 library geometry, trimmed to
    /// the retained rectangle + two pins) at the given centres, plus any
    /// extra top-level content the caller supplies (wires, sheets, ...).
    /// These tests need no SVG at all: off-page, symbol/symbol, wire/symbol,
    /// sheet-pin and pin-at-corner checks run purely off `.kicad_sch`
    /// geometry.
    const DEVICE_R: &str = r#"(symbol "Device:R"
    (symbol "R_0_1"
        (rectangle (start -1.016 -2.54) (end 1.016 2.54)
            (stroke (width 0.254) (type default)) (fill (type none))))
    (symbol "R_1_1"
        (pin passive line (at 0 3.81 270) (length 1.27) (name "") (number "1"))
        (pin passive line (at 0 -3.81 90) (length 1.27) (name "") (number "2"))))
"#;

    fn resistor(reference: &str, x: f64, y: f64) -> String {
        format!(
            "(symbol (lib_id \"Device:R\") (at {x} {y} 0) (unit 1) (uuid \"{reference}\")\n\
             (property \"Reference\" \"{reference}\" (at {x} {y} 0))\n\
             (property \"Value\" \"10k\" (at {x} {y} 0)))\n"
        )
    }

    fn schematic(extra_syms: &str, extra: &str) -> String {
        format!(
            "(kicad_sch (version 20260306) (generator \"eeschema\") (paper \"A4\")\n\
             (lib_symbols {DEVICE_R})\n{extra_syms}{extra}\
             (sheet_instances (path \"/\" (page \"1\"))))\n"
        )
    }

    fn findings_for(sch: &str) -> Vec<Finding> {
        let tree = parse_sexp(sch).unwrap();
        analyze_schematic(&tree, &[], 12.0)
    }

    #[test]
    fn a_symbol_placed_inside_the_margin_is_off_page() {
        let sch = schematic(&resistor("R1", 3.0, 3.0), "");
        let findings = findings_for(&sch);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "off_page" && f.severity == "error" && f.message.contains("R1")),
            "{findings:?}"
        );
    }

    #[test]
    fn a_symbol_well_inside_the_frame_is_not_off_page() {
        let sch = schematic(&resistor("R1", 100.0, 100.0), "");
        let findings = findings_for(&sch);
        assert!(
            !findings.iter().any(|f| f.rule == "off_page"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_wire_drawn_through_a_symbol_body_is_flagged() {
        let extra = "(wire (pts (xy 90 50) (xy 110 50)) (uuid \"w1\"))\n";
        let sch = schematic(&resistor("R1", 100.0, 50.0), extra);
        let findings = findings_for(&sch);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "wire_through_symbol" && f.severity == "error"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_wire_that_stops_short_of_the_body_is_not_flagged() {
        let extra = "(wire (pts (xy 90 50) (xy 95 50)) (uuid \"w1\"))\n";
        let sch = schematic(&resistor("R1", 100.0, 50.0), extra);
        let findings = findings_for(&sch);
        assert!(
            !findings.iter().any(|f| f.rule == "wire_through_symbol"),
            "{findings:?}"
        );
    }

    #[test]
    fn two_bodies_closer_than_2_54mm_apart_are_crowding() {
        let mut syms = resistor("R1", 200.0, 100.0);
        syms.push_str(&resistor("R2", 203.0, 100.0)); // ~0.97 mm body gap
        let sch = schematic(&syms, "");
        let findings = findings_for(&sch);
        assert!(
            findings.iter().any(|f| f.rule == "symbol_crowding"),
            "{findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.rule == "symbol_overlap"),
            "bodies must not actually overlap: {findings:?}"
        );
    }

    #[test]
    fn overlapping_bodies_are_symbol_overlap_not_crowding() {
        let mut syms = resistor("R1", 200.0, 100.0);
        syms.push_str(&resistor("R2", 200.5, 100.0)); // bodies overlap
        let sch = schematic(&syms, "");
        let findings = findings_for(&sch);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "symbol_overlap" && f.severity == "error"),
            "{findings:?}"
        );
    }

    #[test]
    fn power_symbols_are_exempt_from_crowding_and_overlap() {
        let power_lib = r#"(symbol "power:GND" (power global)
            (symbol "GND_1_1" (pin power_in line (at 0 0 270) (length 0) (name "") (number "1"))))
"#;
        let sch = format!(
            "(kicad_sch (version 20260306) (generator \"eeschema\") (paper \"A4\")\n\
             (lib_symbols {DEVICE_R}{power_lib})\n\
             (symbol (lib_id \"Device:R\") (at 100 100 0) (unit 1) (uuid \"R1\")\n\
             (property \"Reference\" \"R1\" (at 100 100 0)))\n\
             (symbol (lib_id \"power:GND\") (at 100.1 100 0) (unit 1) (uuid \"P1\")\n\
             (property \"Reference\" \"#PWR01\" (at 100.1 100 0))\n\
             (property \"Value\" \"GND\" (at 100.1 100 0)))\n\
             (sheet_instances (path \"/\" (page \"1\"))))\n"
        );
        let tree = parse_sexp(&sch).unwrap();
        let findings = analyze_schematic(&tree, &[], 12.0);
        assert!(
            !findings
                .iter()
                .any(|f| f.rule == "symbol_overlap" || f.rule == "symbol_crowding"),
            "a power symbol sitting on a pin is the normal idiom: {findings:?}"
        );
    }

    #[test]
    fn sheet_pin_on_the_left_edge_wants_angle_180_and_justify_left() {
        let sheet = r#"(sheet (at 80 90) (size 20 20) (uuid "s1")
            (property "Sheetname" "sub" (at 80 89 0))
            (property "Sheetfile" "sub.kicad_sch" (at 80 111 0))
            (pin "OUT" input (at 80 95 180)
                (effects (font (size 1.27 1.27)) (justify left))))
"#;
        let sch = schematic("", sheet);
        let findings = findings_for(&sch);
        assert!(
            !findings.iter().any(|f| f.rule.starts_with("sheet_pin")),
            "a correctly-oriented left-edge pin must be clean: {findings:?}"
        );
    }

    #[test]
    fn sheet_pin_with_the_wrong_justify_reads_outside_the_box() {
        // Right-edge pin (angle 0) written with `justify left` — the #350
        // defect: KiCad honours the stored justify, so the text hangs in the
        // gutter between sheets instead of reading into the box.
        let sheet = r#"(sheet (at 80 90) (size 20 20) (uuid "s1")
            (property "Sheetname" "sub" (at 80 89 0))
            (property "Sheetfile" "sub.kicad_sch" (at 80 111 0))
            (pin "OUT" input (at 100 95 0)
                (effects (font (size 1.27 1.27)) (justify left))))
"#;
        let sch = schematic("", sheet);
        let findings = findings_for(&sch);
        assert!(
            findings.iter().any(|f| f.rule == "sheet_pin_text_outside"),
            "{findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.rule == "sheet_pin_side"),
            "the angle itself agrees with the right edge: {findings:?}"
        );
    }

    #[test]
    fn sheet_pin_angle_disagreeing_with_its_edge_is_flagged() {
        // On the left edge (x == sheet.x) but angle 0 (right-edge angle) —
        // KiCad will snap the pin to the opposite edge.
        let sheet = r#"(sheet (at 80 90) (size 20 20) (uuid "s1")
            (property "Sheetname" "sub" (at 80 89 0))
            (property "Sheetfile" "sub.kicad_sch" (at 80 111 0))
            (pin "OUT" input (at 80 95 0)
                (effects (font (size 1.27 1.27)) (justify right))))
"#;
        let sch = schematic("", sheet);
        let findings = findings_for(&sch);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "sheet_pin_side" && f.severity == "error"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_pin_meeting_the_body_at_a_corner_is_flagged() {
        // A body rectangle whose corner sits exactly on a pin's tip: the
        // reference tool's corner check runs in library-local coordinates,
        // unfiltered by unit.
        let lib = r#"(symbol "Bad:Part"
            (symbol "P_0_1"
                (rectangle (start -2 -2) (end 2 2)
                    (stroke (width 0.254) (type default)) (fill (type none))))
            (symbol "P_1_1"
                (pin passive line (at 4 4 225) (length 2.828) (name "") (number "1"))))
"#;
        let sch = format!(
            "(kicad_sch (version 20260306) (generator \"eeschema\") (paper \"A4\")\n\
             (lib_symbols {lib})\n\
             (symbol (lib_id \"Bad:Part\") (at 100 100 0) (unit 1) (uuid \"U1\")\n\
             (property \"Reference\" \"U1\" (at 100 100 0)))\n\
             (sheet_instances (path \"/\" (page \"1\"))))\n"
        );
        let tree = parse_sexp(&sch).unwrap();
        let findings = analyze_schematic(&tree, &[], 12.0);
        assert!(
            findings.iter().any(|f| f.rule == "pin_at_corner"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_rules_filter_argument_would_keep_only_named_rules() {
        // `run_checks` itself does not filter — the handler does, post hoc —
        // but the counts it returns must still let the handler do that
        // faithfully: every finding carries its own rule name.
        let mut syms = resistor("R1", 100.0, 50.0); // under the wire below
        syms.push_str(&resistor("R2", 3.0, 3.0)); // inside the margin
        let extra = "(wire (pts (xy 90 50) (xy 110 50)) (uuid \"w1\"))\n";
        let sch = schematic(&syms, extra);
        let findings = findings_for(&sch);
        let rules: std::collections::HashSet<&str> = findings.iter().map(|f| f.rule).collect();
        assert!(rules.contains("off_page"), "{findings:?}");
        assert!(rules.contains("wire_through_symbol"), "{findings:?}");
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use konnect_sexp::parse_sexp;

    const FIXTURE_SCH: &str = include_str!("../../tests/fixtures/layout_lint_fixture.kicad_sch");
    const FIXTURE_SVG: &str = include_str!("../../tests/fixtures/layout_lint_fixture.svg");

    /// Real KiCad 10 geometry (`Device:R`'s stock library definition) and a
    /// real `kicad-cli sch export svg` render of it — not a hand-estimated
    /// SVG. Five resistors: R1 sits under a wire drawn straight through its
    /// body with a label sharing its Reference field's exact point; R2's
    /// Value field is placed 10+ mm from its body; R3 sits inside the page
    /// margin; R4/R5 are 3 mm apart (crowding, not overlapping). Two "AUX"
    /// labels sit 0.1 mm apart.
    fn fixture_findings() -> Vec<Finding> {
        let tree = parse_sexp(FIXTURE_SCH).unwrap();
        let doc = parse_svg_document(FIXTURE_SVG).unwrap();
        let mut svgs = Vec::new();
        collect_svg_texts(doc.root(), None, &mut svgs);
        assert!(
            !svgs.is_empty(),
            "the fixture SVG must carry measurement text"
        );
        analyze_schematic(&tree, &svgs, 12.0)
    }

    #[test]
    fn wire_drawn_through_r1_is_flagged() {
        let findings = fixture_findings();
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "wire_through_symbol" && f.message.contains("R1")),
            "{findings:?}"
        );
    }

    #[test]
    fn the_sig_label_overlapping_r1s_reference_is_flagged() {
        let findings = fixture_findings();
        assert!(
            findings.iter().any(|f| f.rule == "text_overlap"
                && f.message.contains("SIG")
                && f.message.contains("R1")),
            "{findings:?}"
        );
    }

    #[test]
    fn r2s_far_flung_value_field_is_flagged() {
        let findings = fixture_findings();
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "field_far" && f.message.contains("R2")),
            "{findings:?}"
        );
    }

    #[test]
    fn the_duplicate_aux_labels_are_flagged() {
        let findings = fixture_findings();
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "duplicate_label" && f.message.contains("AUX")),
            "{findings:?}"
        );
    }

    #[test]
    fn r3_inside_the_page_margin_is_off_page() {
        let findings = fixture_findings();
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "off_page" && f.message.contains("R3")),
            "{findings:?}"
        );
    }

    #[test]
    fn r4_and_r5_three_mm_apart_are_crowding_not_overlapping() {
        let findings = fixture_findings();
        assert!(
            findings.iter().any(|f| f.rule == "symbol_crowding"
                && (f.message.contains("R4") || f.message.contains("R5"))),
            "{findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.rule == "symbol_overlap"),
            "{findings:?}"
        );
    }

    #[test]
    fn a_rules_filter_can_isolate_one_finding_class() {
        let findings = fixture_findings();
        let only_off_page: Vec<_> = findings
            .into_iter()
            .filter(|f| f.rule == "off_page")
            .collect();
        assert!(!only_off_page.is_empty());
        assert!(only_off_page.iter().all(|f| f.rule == "off_page"));
    }
}

#[cfg(test)]
mod handler_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn ctx_with_cli(kicad_cli: &str) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: kicad_cli.to_string(),
                ..ServerConfig::default()
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    /// kicad-cli missing (or any other reason the SVG export fails) must
    /// come back as a `BLOCKED` status the caller can see, never a silent
    /// `count: 0` pass — the whole point of #15's `check_schematic_layout`
    /// over hand-rolled text estimation is that the checker refuses to guess.
    #[tokio::test]
    async fn missing_kicad_cli_reports_blocked_not_a_silent_pass() {
        let tmp = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        std::fs::write(
            tmp.path(),
            "(kicad_sch (version 20260306) (generator \"eeschema\") (paper \"A4\") \
             (sheet_instances (path \"/\" (page \"1\"))))\n",
        )
        .unwrap();
        let ctx = ctx_with_cli("C:/does/not/exist/kicad-cli.exe");
        let result = handle_check_schematic_layout(
            &json!({ "schematic": tmp.path().to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            !result.is_error,
            "a blocked check is still a successful call"
        );
        let body = response_json(&result);
        assert_eq!(body["status"], "BLOCKED");
        assert_eq!(body["count"], 0);
        assert!(!body["blocked_reason"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn negative_margin_is_a_named_argument_error() {
        let tmp = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        std::fs::write(
            tmp.path(),
            "(kicad_sch (version 20260306) (generator \"eeschema\") (paper \"A4\") \
             (sheet_instances (path \"/\" (page \"1\"))))\n",
        )
        .unwrap();
        let ctx = ctx_with_cli("C:/does/not/exist/kicad-cli.exe");
        let result = handle_check_schematic_layout(
            &json!({ "schematic": tmp.path().to_str().unwrap(), "margin_mm": -1.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let body = response_json(&result);
        assert_eq!(body["error"]["kind"], "invalid_argument");
        assert_eq!(body["error"]["field"], "margin_mm");
    }

    #[test]
    fn the_tool_is_registered_with_a_schematic_argument() {
        let defs = tools();
        let def = defs
            .iter()
            .find(|d| d.name == "check_schematic_layout")
            .expect("check_schematic_layout must be registered");
        assert_eq!(
            def.input_schema["required"],
            serde_json::json!(["schematic"])
        );
    }
}
