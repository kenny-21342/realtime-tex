//! Native drawing of a page's PDF literals (TikZ / pgf pictures).
//!
//! A page whose only degradation is PDF literals can be drawn without its PDF when every
//! literal is understood. [`native_graphics`] replays what LuaTeX writes into the page content
//! stream and returns the paint operations in page coordinates, or the first thing it does not
//! understand. It never returns a partial result: a page is drawn natively completely or not at
//! all.
//!
//! What LuaTeX writes (checked against its output): a literal in mode 0 ("origin") is preceded by
//! `1 0 0 1 dx dy cm`, the move from the position of the previous positioned output to the
//! literal's own (its `at`), in the current user space. q/Q inside literals save and restore the
//! real graphics state, not LuaTeX's idea of the position; pgf opens and closes its scopes at the
//! same TeX position, so the two stay in step. Text is written with absolute text matrices after
//! moving back to the origin: a glyph lands at its position mapped by the current transform
//! without the bookkeeping translation, which differs from its display-list position only inside
//! a transformed pgf node (rotated or scaled text). Those glyphs (and rules, images) get an entry
//! in [`NativePage::transforms`]. `pdf_save`/`pdf_setmatrix`/`pdf_restore` (graphicx, the MATRIX
//! records) are replayed as LuaTeX writes them (a move, then `q` / `a b c d 0 0 cm` / `Q`), so
//! literals inside `\resizebox` come out scaled.
//!
//! Understood operators: path construction (`m l c v y h re`), painting (`S s f F f* B B* b b*
//! n`), clipping (`W W*`), state (`q Q cm w J j M d`), device colors (`g G rg RG k K`, `cs CS sc
//! SC scn SCN` in DeviceGray/RGB/CMYK), pgf's opacity states (`/pgf@ca<v> gs`, `/pgf@CA<v> gs`)
//! and pdf_colorstack operations. Anything else (text in literals, XObjects, shadings, patterns,
//! other graphics states, literal modes other than origin/page, \special) is an error.

use crate::{DisplayList, Item, Line, Sp, SP_PER_BP};
use serde::Serialize;
use std::collections::BTreeMap;

/// An affine map `[a b c d e f]` in PDF's row-vector convention: `(x, y) -> (a x + c y + e,
/// b x + d y + f)`.
pub type Matrix = [f64; 6];

pub const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `m` then `n` (apply `m` first).
pub fn concat(m: &Matrix, n: &Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

pub fn apply(m: &Matrix, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

fn translate(x: f64, y: f64) -> Matrix {
    [1.0, 0.0, 0.0, 1.0, x, y]
}

fn is_identity(m: &Matrix) -> bool {
    m.iter().zip(IDENTITY.iter()).all(|(a, b)| (a - b).abs() < 1e-9)
}

/// Where an item sits in a display list: a row (`line`) or the page's `other` items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ItemRef {
    pub line: Option<usize>,
    pub item: usize,
}

/// A path segment in user space (PDF units, y up); the op's `ctm` maps it to the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "s")]
pub enum Seg {
    M { x: f64, y: f64 },
    L { x: f64, y: f64 },
    C { x1: f64, y1: f64, x2: f64, y2: f64, x: f64, y: f64 },
    Z,
}

/// A device color as the PDF gives it (`comps`: 1 gray, 3 RGB or 4 CMYK components, 0..1) and
/// its opacity. Hosts convert CMYK as their color management does; [`Color::rgb`] is the plain
/// formula.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Color {
    pub comps: Vec<f64>,
    pub alpha: f64,
}

impl Color {
    fn black() -> Color {
        Color { comps: vec![0.0], alpha: 1.0 }
    }
    pub fn rgb(&self) -> [f64; 3] {
        match self.comps.as_slice() {
            [g] => [*g, *g, *g],
            [r, g, b] => [*r, *g, *b],
            [c, m, y, k] => [(1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k)],
            _ => [0.0, 0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stroke {
    pub color: Color,
    /// In user space (scaled by `ctm`).
    pub width: f64,
    /// 0 butt, 1 round, 2 projecting square.
    pub cap: u8,
    /// 0 miter, 1 round, 2 bevel.
    pub join: u8,
    pub miter_limit: f64,
    pub dash: Vec<f64>,
    pub dash_phase: f64,
}

/// One drawing operation. `ctm` maps user space to page coordinates in sp, y down (the display
/// list's frame). `at` is the literal whose operator produced it: draw ops in item order,
/// interleaved with the glyphs, rules and images of the list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op")]
pub enum GfxOp {
    /// Save the clip (PDF `q`).
    Save { at: ItemRef },
    /// Restore the clip saved by the matching `Save` (PDF `Q`).
    Restore { at: ItemRef },
    /// Intersect the clip with a path (nonzero, or even-odd).
    Clip { at: ItemRef, ctm: Matrix, path: Vec<Seg>, even_odd: bool },
    /// Paint a pgf shading (a form XObject `/Sh sh`) over `bbox` (form space, PDF units, `[x0
    /// y0 x1 y1]`), clipped by the current clip. Axial: `coords` `[x0 y0 x1 y1]`; radial: `[x0 y0
    /// r0 x1 y1 r1]` (form space). Colors vary linearly between `stops` (offsets 0..1 along the
    /// shading's parameter); `extend` continues the end colors beyond the ends. `alpha` is the
    /// fill opacity in force.
    Shade {
        at: ItemRef,
        ctm: Matrix,
        bbox: [f64; 4],
        radial: bool,
        coords: Vec<f64>,
        extend: [bool; 2],
        stops: Vec<(f64, Color)>,
        alpha: f64,
    },
    /// Fill and/or stroke a path (fill first).
    Paint {
        at: ItemRef,
        ctm: Matrix,
        path: Vec<Seg>,
        fill: Option<Color>,
        even_odd: bool,
        stroke: Option<Stroke>,
    },
}

/// A page's literals as drawing operations.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NativePage {
    pub ops: Vec<GfxOp>,
    /// Items (glyphs, rules, images) that a transformed pgf scope moves away from their
    /// display-list position: draw them with this map (display-list coordinates to display-list
    /// coordinates) instead of their MATRIX records.
    pub transforms: Vec<(ItemRef, Matrix)>,
}

/// Why a page cannot be drawn natively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unsupported {
    pub at: Option<ItemRef>,
    pub what: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.at {
            Some(r) => write!(f, "{} (line {:?} item {})", self.what, r.line, r.item),
            None => write!(f, "{}", self.what),
        }
    }
}

/// Flags that native drawing resolves: a page with only these is drawn natively when
/// [`native_graphics`] succeeds.
pub const RESOLVED_FLAGS: &[&str] = &["literal", "shading"];

/// Is the page degraded only by literals (and so a candidate for native drawing)?
pub fn only_literals(dl: &DisplayList) -> bool {
    let flags = dl.flags_map();
    !flags.is_empty() && flags.keys().all(|k| RESOLVED_FLAGS.contains(&k.as_str()))
}

#[derive(Debug, Clone)]
struct GState {
    ctm: Matrix,
    fill: Color,
    stroke: Color,
    width: f64,
    cap: u8,
    join: u8,
    miter: f64,
    dash: Vec<f64>,
    dash_phase: f64,
    /// Color spaces set by `cs`/`CS`: components expected by `sc`/`scn`.
    fill_space: usize,
    stroke_space: usize,
}

impl Default for GState {
    fn default() -> Self {
        GState {
            ctm: IDENTITY,
            fill: Color::black(),
            stroke: Color::black(),
            width: 1.0,
            cap: 0,
            join: 0,
            miter: 10.0,
            dash: vec![],
            dash_phase: 0.0,
            fill_space: 1,
            stroke_space: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Name(String),
    Op(String),
    Array(Vec<f64>),
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let is_delim = |c: u8| matches!(c, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%');
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'%' => {
                while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                    i += 1;
                }
            }
            b'/' => {
                let s0 = i + 1;
                i += 1;
                while i < b.len() && !b[i].is_ascii_whitespace() && !is_delim(b[i]) {
                    i += 1;
                }
                out.push(Tok::Name(s[s0..i].to_string()));
            }
            b'[' => {
                let end = s[i..].find(']').ok_or("unterminated array")? + i;
                let mut v = Vec::new();
                for t in s[i + 1..end].split_ascii_whitespace() {
                    v.push(t.parse::<f64>().map_err(|_| format!("array element {t:?}"))?);
                }
                out.push(Tok::Array(v));
                i = end + 1;
            }
            b'(' | b'<' | b'>' | b'{' | b'}' | b']' | b')' => {
                return Err(format!("unsupported token {:?}", c as char));
            }
            _ => {
                let s0 = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && !is_delim(b[i]) {
                    i += 1;
                }
                let t = &s[s0..i];
                if let Ok(v) = t.parse::<f64>() {
                    out.push(Tok::Num(v));
                } else if t.starts_with(['+', '-', '.']) || t.as_bytes()[0].is_ascii_digit() {
                    return Err(format!("bad number {t:?}"));
                } else {
                    out.push(Tok::Op(t.to_string()));
                }
            }
        }
    }
    Ok(out)
}

struct ColorStacks {
    stacks: BTreeMap<i64, Vec<String>>,
}

struct Interp {
    page_h: f64,
    gs: GState,
    stack: Vec<GState>,
    /// LuaTeX's idea of the output position (PDF coordinates), moved before positioned output.
    pos: (f64, f64),
    path: Vec<Seg>,
    clip: Option<bool>,
    ops: Vec<GfxOp>,
    transforms: Vec<(ItemRef, Matrix)>,
    colors: ColorStacks,
    /// q/Q depth of the current row: a row may not restore what it did not save.
    depth: usize,
}

/// From PDF space (bp, y up, origin bottom-left) to display-list space (sp, y down).
fn pdf_to_dl(page_h: f64) -> Matrix {
    [SP_PER_BP, 0.0, 0.0, -SP_PER_BP, 0.0, page_h * SP_PER_BP]
}

impl Interp {
    fn to_pdf(&self, x: Sp, y: Sp) -> (f64, f64) {
        (x as f64 / SP_PER_BP, self.page_h - y as f64 / SP_PER_BP)
    }

    /// LuaTeX's move to a positioned output: `1 0 0 1 dx dy cm` in the current user space.
    fn move_to(&mut self, x: Sp, y: Sp) {
        let (px, py) = self.to_pdf(x, y);
        let (dx, dy) = (px - self.pos.0, py - self.pos.1);
        if dx != 0.0 || dy != 0.0 {
            self.gs.ctm = concat(&translate(dx, dy), &self.gs.ctm);
            self.pos = (px, py);
        }
    }

    /// The transform LuaTeX's absolute output (text matrices, rules) is under: the current one
    /// without the bookkeeping translation, from display-list to display-list coordinates.
    fn output_map(&self) -> Matrix {
        let m = concat(&translate(-self.pos.0, -self.pos.1), &self.gs.ctm);
        let to_dl = pdf_to_dl(self.page_h);
        let from_dl = [1.0 / SP_PER_BP, 0.0, 0.0, -1.0 / SP_PER_BP, 0.0, self.page_h];
        concat(&concat(&from_dl, &m), &to_dl)
    }

    fn dl_ctm(&self) -> Matrix {
        concat(&self.gs.ctm, &pdf_to_dl(self.page_h))
    }

    fn set_color(fill: bool, gs: &mut GState, comps: &[f64]) -> Result<(), String> {
        if !matches!(comps.len(), 1 | 3 | 4) {
            return Err(format!("color with {} components", comps.len()));
        }
        let slot = if fill { &mut gs.fill } else { &mut gs.stroke };
        slot.comps = comps.to_vec();
        Ok(())
    }

    /// Run the operators of a literal (or of a color stack entry: `colors_only`).
    fn run(&mut self, data: &str, at: ItemRef, colors_only: bool) -> Result<(), String> {
        let toks = tokenize(data)?;
        let mut args: Vec<Tok> = Vec::new();
        for t in toks {
            let op = match t {
                Tok::Op(op) => op,
                other => {
                    args.push(other);
                    continue;
                }
            };
            let nums: Vec<f64> = args.iter().filter_map(|a| if let Tok::Num(v) = a { Some(*v) } else { None }).collect();
            let need = |n: usize| -> Result<(), String> {
                if nums.len() == n && args.len() == n {
                    Ok(())
                } else {
                    Err(format!("operator {op} with operands {args:?}"))
                }
            };
            let is_color = matches!(op.as_str(), "g" | "G" | "rg" | "RG" | "k" | "K" | "cs" | "CS" | "sc" | "SC" | "scn" | "SCN");
            if colors_only && !is_color {
                return Err(format!("color stack entry with operator {op}"));
            }
            match op.as_str() {
                // path construction
                "m" => {
                    need(2)?;
                    self.path.push(Seg::M { x: nums[0], y: nums[1] })
                }
                "l" => {
                    need(2)?;
                    self.path.push(Seg::L { x: nums[0], y: nums[1] })
                }
                "c" => {
                    need(6)?;
                    self.path.push(Seg::C { x1: nums[0], y1: nums[1], x2: nums[2], y2: nums[3], x: nums[4], y: nums[5] })
                }
                "v" | "y" => {
                    need(4)?;
                    let cur = self.current_point().ok_or(format!("{op} without a current point"))?;
                    self.path.push(if op == "v" {
                        Seg::C { x1: cur.0, y1: cur.1, x2: nums[0], y2: nums[1], x: nums[2], y: nums[3] }
                    } else {
                        Seg::C { x1: nums[0], y1: nums[1], x2: nums[2], y2: nums[3], x: nums[2], y: nums[3] }
                    });
                }
                "h" => {
                    need(0)?;
                    self.path.push(Seg::Z)
                }
                "re" => {
                    need(4)?;
                    let (x, y, w, h) = (nums[0], nums[1], nums[2], nums[3]);
                    self.path.extend([
                        Seg::M { x, y },
                        Seg::L { x: x + w, y },
                        Seg::L { x: x + w, y: y + h },
                        Seg::L { x, y: y + h },
                        Seg::Z,
                    ]);
                }
                // painting
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                    need(0)?;
                    if matches!(op.as_str(), "s" | "b" | "b*") {
                        self.path.push(Seg::Z);
                    }
                    let fill = matches!(op.as_str(), "f" | "F" | "f*" | "B" | "B*" | "b" | "b*");
                    let stroke = matches!(op.as_str(), "S" | "s" | "B" | "B*" | "b" | "b*");
                    let even_odd = op.ends_with('*');
                    let path = std::mem::take(&mut self.path);
                    let ctm = self.dl_ctm();
                    if (fill || stroke) && !path.is_empty() {
                        self.ops.push(GfxOp::Paint {
                            at,
                            ctm,
                            path: path.clone(),
                            fill: fill.then(|| self.gs.fill.clone()),
                            even_odd,
                            stroke: stroke.then(|| Stroke {
                                color: self.gs.stroke.clone(),
                                width: self.gs.width,
                                cap: self.gs.cap,
                                join: self.gs.join,
                                miter_limit: self.gs.miter,
                                dash: self.gs.dash.clone(),
                                dash_phase: self.gs.dash_phase,
                            }),
                        });
                    }
                    if let Some(eo) = self.clip.take() {
                        self.ops.push(GfxOp::Clip { at, ctm, path, even_odd: eo });
                    }
                }
                "W" | "W*" => {
                    need(0)?;
                    self.clip = Some(op == "W*");
                }
                // state
                "q" => {
                    need(0)?;
                    self.stack.push(self.gs.clone());
                    self.depth += 1;
                    self.ops.push(GfxOp::Save { at });
                }
                "Q" => {
                    need(0)?;
                    if self.depth == 0 {
                        return Err("Q without q in this row".into());
                    }
                    self.depth -= 1;
                    self.gs = self.stack.pop().unwrap();
                    self.ops.push(GfxOp::Restore { at });
                }
                "cm" => {
                    need(6)?;
                    if !self.path.is_empty() {
                        return Err("cm inside a path".into());
                    }
                    let m = [nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]];
                    self.gs.ctm = concat(&m, &self.gs.ctm);
                }
                "w" => {
                    need(1)?;
                    self.gs.width = nums[0]
                }
                "J" => {
                    need(1)?;
                    self.gs.cap = nums[0] as u8
                }
                "j" => {
                    need(1)?;
                    self.gs.join = nums[0] as u8
                }
                "M" => {
                    need(1)?;
                    self.gs.miter = nums[0]
                }
                "d" => match args.as_slice() {
                    [Tok::Array(a), Tok::Num(p)] => {
                        self.gs.dash = a.clone();
                        self.gs.dash_phase = *p;
                    }
                    _ => return Err(format!("d with operands {args:?}")),
                },
                "gs" => match args.as_slice() {
                    [Tok::Name(n)] => {
                        if let Some(v) = n.strip_prefix("pgf@ca") {
                            self.gs.fill.alpha = v.parse::<f64>().map_err(|_| format!("gs /{n}"))?;
                        } else if let Some(v) = n.strip_prefix("pgf@CA") {
                            self.gs.stroke.alpha = v.parse::<f64>().map_err(|_| format!("gs /{n}"))?;
                        } else {
                            return Err(format!("graphics state /{n}"));
                        }
                    }
                    _ => return Err(format!("gs with operands {args:?}")),
                },
                // colors
                "g" | "G" | "rg" | "RG" | "k" | "K" => {
                    let n = match op.as_str() {
                        "g" | "G" => 1,
                        "rg" | "RG" => 3,
                        _ => 4,
                    };
                    need(n)?;
                    let fill = op.chars().next().unwrap().is_ascii_lowercase();
                    Self::set_color(fill, &mut self.gs, &nums)?;
                    if fill {
                        self.gs.fill_space = n;
                    } else {
                        self.gs.stroke_space = n;
                    }
                }
                "cs" | "CS" => {
                    let n = match args.as_slice() {
                        [Tok::Name(s)] if s == "DeviceGray" => 1,
                        [Tok::Name(s)] if s == "DeviceRGB" => 3,
                        [Tok::Name(s)] if s == "DeviceCMYK" => 4,
                        _ => return Err(format!("color space {args:?}")),
                    };
                    let fill = op == "cs";
                    // a new color space starts at its initial color, black
                    let black: &[f64] = match n {
                        1 => &[0.0],
                        3 => &[0.0, 0.0, 0.0],
                        _ => &[0.0, 0.0, 0.0, 1.0],
                    };
                    Self::set_color(fill, &mut self.gs, black)?;
                    if fill {
                        self.gs.fill_space = n;
                    } else {
                        self.gs.stroke_space = n;
                    }
                }
                "sc" | "SC" | "scn" | "SCN" => {
                    let fill = op.starts_with('s');
                    let n = if fill { self.gs.fill_space } else { self.gs.stroke_space };
                    need(n)?;
                    Self::set_color(fill, &mut self.gs, &nums)?;
                }
                other => return Err(format!("operator {other}")),
            }
            args.clear();
        }
        if !args.is_empty() {
            return Err(format!("operands without operator: {args:?}"));
        }
        Ok(())
    }

    fn current_point(&self) -> Option<(f64, f64)> {
        let mut start = None;
        let mut cur = None;
        for s in &self.path {
            match s {
                Seg::M { x, y } => {
                    start = Some((*x, *y));
                    cur = start;
                }
                Seg::L { x, y } | Seg::C { x, y, .. } => cur = Some((*x, *y)),
                Seg::Z => cur = start,
            }
        }
        cur
    }

    /// `after`: the stack's top after the command in LuaTeX's order (capture pages), which wins
    /// over the replay in list order (rows and `other` interleave on the page).
    fn color_stack(&mut self, stack: i64, cmd: Option<i64>, data: &str, after: Option<&str>, at: ItemRef) -> Result<(), String> {
        // LuaTeX: 0 set (replace the top), 1 push, 2 pop (the new top is written), 3 current.
        // Stack 0 (the color package's) starts at black; another stack's base is unknown.
        let st = self
            .colors
            .stacks
            .entry(stack)
            .or_insert_with(|| if stack == 0 { vec!["0 g 0 G".to_string()] } else { Vec::new() });
        let emit = match cmd {
            Some(0) => {
                st.pop();
                st.push(data.to_string());
                Some(data.to_string())
            }
            Some(1) => {
                st.push(data.to_string());
                Some(data.to_string())
            }
            Some(2) => {
                st.pop();
                if st.is_empty() {
                    match after {
                        Some(a) => st.push(a.to_string()),
                        None => return Err(format!("color stack {stack} popped below its base")),
                    }
                }
                st.last().cloned()
            }
            Some(3) => st.last().cloned(),
            _ => return Err(format!("color stack command {cmd:?}")),
        };
        let emit = match (emit, after) {
            // the true top, where the replay in list order lost track of it
            (Some(e), Some(a)) if !a.is_empty() && e != a => {
                if let Some(top) = st.last_mut() {
                    *top = a.to_string();
                }
                Some(a.to_string())
            }
            (e, _) => e,
        };
        if let Some(d) = emit {
            self.run(&d, at, true)?;
        }
        Ok(())
    }
}

/// A PDF object, enough for shading functions.
#[derive(Debug, Clone, PartialEq)]
enum Obj {
    Num(f64),
    Name(String),
    Bool(bool),
    Array(Vec<Obj>),
    Dict(BTreeMap<String, Obj>),
}

impl Obj {
    fn num(&self) -> Option<f64> {
        if let Obj::Num(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    fn nums(&self) -> Option<Vec<f64>> {
        match self {
            Obj::Array(a) => a.iter().map(|o| o.num()).collect(),
            _ => None,
        }
    }
}

fn parse_obj(toks: &[String], i: &mut usize) -> Result<Obj, String> {
    let t = toks.get(*i).ok_or("unexpected end")?.clone();
    *i += 1;
    Ok(match t.as_str() {
        "<<" => {
            let mut d = BTreeMap::new();
            loop {
                let k = toks.get(*i).ok_or("unterminated dictionary")?.clone();
                if k == ">>" {
                    *i += 1;
                    break;
                }
                let key = k.strip_prefix('/').ok_or(format!("dictionary key {k}"))?.to_string();
                *i += 1;
                d.insert(key, parse_obj(toks, i)?);
            }
            Obj::Dict(d)
        }
        "[" => {
            let mut a = Vec::new();
            loop {
                if toks.get(*i).ok_or("unterminated array")? == "]" {
                    *i += 1;
                    break;
                }
                a.push(parse_obj(toks, i)?);
            }
            Obj::Array(a)
        }
        "true" => Obj::Bool(true),
        "false" => Obj::Bool(false),
        n if n.starts_with('/') => Obj::Name(n[1..].to_string()),
        n => Obj::Num(n.parse().map_err(|_| format!("token {n:?}"))?),
    })
}

fn pdf_tokens(s: &str) -> Vec<String> {
    let mut spaced = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' | ']' => {
                spaced.push(' ');
                spaced.push(c);
                spaced.push(' ');
            }
            '<' | '>' if chars.peek() == Some(&c) => {
                chars.next();
                spaced.push(' ');
                spaced.push(c);
                spaced.push(c);
                spaced.push(' ');
            }
            '/' => {
                spaced.push(' ');
                spaced.push('/');
            }
            _ => spaced.push(c),
        }
    }
    spaced.split_ascii_whitespace().map(str::to_string).collect()
}

/// A PDF function of one input as piecewise linear stops over its domain: type 2 with N = 1
/// (linear between C0 and C1) and type 3 stitching of those (pgf's color specifications).
fn function_stops(f: &Obj, t0: f64, t1: f64) -> Result<Vec<(f64, Vec<f64>)>, String> {
    let Obj::Dict(d) = f else { return Err("function is not a dictionary".into()) };
    let ty = d.get("FunctionType").and_then(Obj::num).ok_or("FunctionType")?;
    let domain = d.get("Domain").and_then(Obj::nums).ok_or("Domain")?;
    let eval2 = |d: &BTreeMap<String, Obj>, x: f64| -> Result<Vec<f64>, String> {
        let c0 = d.get("C0").and_then(Obj::nums).unwrap_or(vec![0.0]);
        let c1 = d.get("C1").and_then(Obj::nums).unwrap_or(vec![1.0]);
        let n = d.get("N").and_then(Obj::num).ok_or("N")?;
        if n != 1.0 {
            return Err(format!("exponential function with N = {n}"));
        }
        Ok(c0.iter().zip(c1.iter()).map(|(a, b)| a + x * (b - a)).collect())
    };
    let clamp = |x: f64, lo: f64, hi: f64| x.max(lo.min(hi)).min(hi.max(lo));
    match ty as i64 {
        2 => {
            // linear in its own input: two stops at t0 and t1
            let (a, b) = (clamp(t0, domain[0], domain[1]), clamp(t1, domain[0], domain[1]));
            Ok(vec![(t0, eval2(d, a)?), (t1, eval2(d, b)?)])
        }
        3 => {
            let fs = match d.get("Functions") {
                Some(Obj::Array(a)) => a.clone(),
                _ => return Err("Functions".into()),
            };
            let bounds = d.get("Bounds").and_then(Obj::nums).ok_or("Bounds")?;
            let encode = d.get("Encode").and_then(Obj::nums).ok_or("Encode")?;
            if bounds.len() + 1 != fs.len() || encode.len() != 2 * fs.len() {
                return Err("stitching function sizes".into());
            }
            let mut edges = vec![domain[0]];
            edges.extend(bounds.iter().copied());
            edges.push(domain[1]);
            let mut out = Vec::new();
            for (k, sub) in fs.iter().enumerate() {
                let (a, b) = (edges[k], edges[k + 1]);
                if b <= a {
                    continue;
                }
                let Obj::Dict(sd) = sub else { return Err("sub-function".into()) };
                if sd.get("FunctionType").and_then(Obj::num) != Some(2.0) {
                    return Err("stitched function is not exponential".into());
                }
                let sdom = sd.get("Domain").and_then(Obj::nums).ok_or("Domain")?;
                let (e0, e1) = (encode[2 * k], encode[2 * k + 1]);
                let enc = |t: f64| clamp(e0 + (t - a) / (b - a) * (e1 - e0), sdom[0], sdom[1]);
                out.push((a, eval2(sd, enc(a))?));
                out.push((b, eval2(sd, enc(b))?));
            }
            Ok(out)
        }
        t => Err(format!("function type {t}")),
    }
}

impl Interp {
    /// A pgf shading form at its place: `index x top width height depth {spec}`.
    fn shade(&mut self, detail: &str, at: ItemRef) -> Result<(), String> {
        let mut parts = detail.splitn(7, ' ');
        let mut num = || -> Result<i64, String> {
            parts.next().and_then(|t| t.parse().ok()).ok_or_else(|| format!("shading detail {detail:?}"))
        };
        let (_idx, x, top, w, h, d) = (num()?, num()?, num()?, num()?, num()?, num()?);
        let spec: serde_json::Value = serde_json::from_str(parts.next().ok_or("shading spec")?).map_err(|e| e.to_string())?;
        let field = |k: &str| spec.get(k).and_then(|v| v.as_str()).map(str::to_string).ok_or(format!("shading spec {k}"));
        let radial = match field("kind")?.as_str() {
            "axial" => false,
            "radial" => true,
            k => return Err(format!("shading kind {k}")),
        };
        let space = field("space")?;
        let ncomp = match space.trim() {
            "/DeviceGray" => 1,
            "/DeviceRGB" => 3,
            "/DeviceCMYK" => 4,
            s => return Err(format!("shading color space {s}")),
        };
        let nums = |s: &str| -> Result<Vec<f64>, String> { s.split_ascii_whitespace().map(|t| t.parse::<f64>().map_err(|_| format!("number {t:?}"))).collect() };
        let coords = nums(&field("coords")?)?;
        let domain = nums(&field("domain")?)?;
        if coords.len() != if radial { 6 } else { 4 } || domain.len() != 2 {
            return Err("shading coords or domain".into());
        }
        let ext: Vec<bool> = field("extend")?.split_ascii_whitespace().map(|t| t == "true").collect();
        let toks = pdf_tokens(&field("function")?);
        let f = parse_obj(&toks, &mut 0)?;
        let (t0, t1) = (domain[0], domain[1]);
        let stops = function_stops(&f, t0, t1)?
            .into_iter()
            .map(|(t, c)| {
                if c.len() != ncomp {
                    return Err(format!("function gives {} components for {space}", c.len()));
                }
                Ok(((t - t0) / (t1 - t0), Color { comps: c, alpha: 1.0 }))
            })
            .collect::<Result<Vec<_>, String>>()?;
        // LuaTeX places the form at its reference point (the baseline): q, a move there, Do, Q
        let baseline = top + h;
        let (px, py) = self.to_pdf(x, baseline);
        let ctm = concat(&translate(px - self.pos.0, py - self.pos.1), &self.gs.ctm);
        let k = SP_PER_BP;
        self.ops.push(GfxOp::Shade {
            at,
            ctm: concat(&ctm, &pdf_to_dl(self.page_h)),
            bbox: [0.0, -(d as f64) / k, w as f64 / k, h as f64 / k],
            radial,
            coords,
            extend: [ext.first().copied().unwrap_or(false), ext.get(1).copied().unwrap_or(false)],
            stops,
            alpha: self.gs.fill.alpha,
        });
        Ok(())
    }
}

/// Interpret the literals of a page display list. `Ok` only when every literal, color stack
/// operation and transform on the page is understood.
pub fn native_graphics(dl: &DisplayList) -> Result<NativePage, Unsupported> {
    let page_h = dl.page_height.ok_or(Unsupported { at: None, what: "no page height".into() })? as f64 / SP_PER_BP;
    let mut it = Interp {
        page_h,
        gs: GState::default(),
        stack: Vec::new(),
        pos: (0.0, 0.0),
        path: Vec::new(),
        clip: None,
        ops: Vec::new(),
        transforms: Vec::new(),
        colors: ColorStacks {
            stacks: dl
                .color_base
                .iter()
                .filter_map(|(k, v)| Some((k.parse::<i64>().ok()?, v.clone())))
                .collect(),
        },
        depth: 0,
    };
    // `other` comes first in the page's content, then the rows in order
    let rows: Vec<(Option<usize>, &[Item])> = std::iter::once((None, dl.other.as_slice()))
        .chain(dl.lines.iter().enumerate().map(|(k, l): (usize, &Line)| (Some(k), l.items.as_slice())))
        .collect();
    for (line, items) in rows {
        let base_depth = it.depth;
        for (k, item) in items.iter().enumerate() {
            let at = ItemRef { line, item: k };
            let fail = |what: String| Unsupported { at: Some(at), what };
            match item {
                Item::Literal { mode, data, at: pos } => match (mode, pos) {
                    (0, Some((x, y))) => {
                        it.move_to(*x, *y);
                        it.run(data, at, false).map_err(fail)?;
                    }
                    (1, _) => it.run(data, at, false).map_err(fail)?,
                    (0, None) => return Err(fail("literal without a position".into())),
                    (-1, _) => return Err(fail("\\special".into())),
                    (m, _) => return Err(fail(format!("literal mode {m}"))),
                },
                Item::Color { stack, cmd, data, after } => {
                    it.color_stack(*stack, *cmd, data, after.as_deref(), at).map_err(fail)?
                }
                Item::Matrix { op, x, y, data } => {
                    it.move_to(*x, *y);
                    match op.as_str() {
                        "save" => it.run("q", at, false).map_err(fail)?,
                        "restore" => it.run("Q", at, false).map_err(fail)?,
                        "set" => {
                            let v: Vec<f64> = data.split_ascii_whitespace().filter_map(|t| t.parse().ok()).collect();
                            if v.len() != 4 {
                                return Err(fail(format!("matrix {data:?}")));
                            }
                            it.run(&format!("{} {} {} {} 0 0 cm", v[0], v[1], v[2], v[3]), at, false).map_err(fail)?;
                        }
                        o => return Err(fail(format!("matrix op {o}"))),
                    }
                }
                Item::Glyph { .. } | Item::Rule { .. } | Item::Image { .. } => {
                    let m = it.output_map();
                    if !is_identity(&m) {
                        it.transforms.push((at, m));
                    }
                }
                Item::Math { .. } => {}
                Item::Unsupported { kind, detail } if kind == "shading" => {
                    let d = detail.as_str().unwrap_or("");
                    it.shade(d, at).map_err(fail)?;
                }
                Item::Unsupported { kind, .. } => return Err(fail(format!("unsupported item {kind}"))),
            }
        }
        if !it.path.is_empty() || it.clip.is_some() {
            return Err(Unsupported { at: None, what: "path left open at the end of a row".into() });
        }
        if it.depth != base_depth {
            return Err(Unsupported { at: None, what: "q without Q in a row".into() });
        }
    }
    Ok(NativePage { ops: it.ops, transforms: it.transforms })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(items: Vec<Item>) -> DisplayList {
        DisplayList {
            kind: "page".into(),
            lines: vec![Line { items, ..Default::default() }],
            flags: serde_json::json!({"literal": 1}),
            page_height: Some((800.0 * SP_PER_BP) as Sp),
            ..Default::default()
        }
    }

    fn lit(data: &str, x: Sp, y: Sp) -> Item {
        Item::Literal { mode: 0, data: data.into(), at: Some((x, y)) }
    }

    fn dl_point(op: &GfxOp, x: f64, y: f64) -> (f64, f64) {
        match op {
            GfxOp::Paint { ctm, .. } | GfxOp::Clip { ctm, .. } => apply(ctm, x, y),
            _ => panic!("no ctm"),
        }
    }

    #[test]
    fn a_line_lands_at_its_literal_position() {
        // a pgf picture at (100 bp, 50 bp from the top): red line from the origin to (10, 10)
        let (x, y) = ((100.0 * SP_PER_BP) as Sp, (50.0 * SP_PER_BP) as Sp);
        let p = page(vec![lit("q 0.4 w", x, y), lit("1 0 0 rg 1 0 0 RG 0 0 m 10 10 l S", x, y), lit("Q", x, y)]);
        let n = native_graphics(&p).unwrap();
        let paint = n.ops.iter().find(|o| matches!(o, GfxOp::Paint { .. })).unwrap();
        let (sx, sy) = dl_point(paint, 0.0, 0.0);
        assert!((sx - x as f64).abs() < 1.0 && (sy - y as f64).abs() < 1.0, "{sx} {sy}");
        let (ex, ey) = dl_point(paint, 10.0, 10.0);
        // y grows down in the display list
        assert!((ex - (x as f64 + 10.0 * SP_PER_BP)).abs() < 1.0 && (ey - (y as f64 - 10.0 * SP_PER_BP)).abs() < 1.0);
        let GfxOp::Paint { stroke: Some(s), fill: None, .. } = paint else { panic!() };
        assert_eq!(s.color, Color { comps: vec![1.0, 0.0, 0.0], alpha: 1.0 });
        assert_eq!(s.width, 0.4);
        assert!(n.transforms.is_empty());
    }

    #[test]
    fn literals_at_another_position_move_relative_to_the_current_transform() {
        // LuaTeX moves inside a rotated scope are rotated too (what the PDF does)
        let (x, y) = ((100.0 * SP_PER_BP) as Sp, (100.0 * SP_PER_BP) as Sp);
        let p = page(vec![
            lit("q 0 1 -1 0 0 0 cm", x, y),
            lit("0 0 m 1 0 l S", x + (10.0 * SP_PER_BP) as Sp, y),
            lit("Q", x, y),
        ]);
        let n = native_graphics(&p).unwrap();
        let paint = n.ops.iter().find(|o| matches!(o, GfxOp::Paint { .. })).unwrap();
        // the move of 10 bp to the right happens in the rotated space: it goes up instead
        let (sx, sy) = dl_point(paint, 0.0, 0.0);
        assert!((sx - x as f64).abs() < 1.0, "{sx}");
        assert!((sy - (y as f64 - 10.0 * SP_PER_BP)).abs() < 1.0, "{sy}");
    }

    #[test]
    fn glyphs_in_a_scaled_scope_get_a_transform() {
        let (x, y) = ((100.0 * SP_PER_BP) as Sp, (100.0 * SP_PER_BP) as Sp);
        let glyph = Item::Glyph { font: 1, char: 65, index: None, x, y, width: 1000, expansion: 0 };
        let p = page(vec![lit("q 2 0 0 2 0 0 cm", x, y), glyph.clone(), lit("Q", x, y), glyph]);
        let n = native_graphics(&p).unwrap();
        assert_eq!(n.transforms.len(), 1, "only the glyph inside the scope");
        let (r, m) = n.transforms[0];
        assert_eq!(r, ItemRef { line: Some(0), item: 1 });
        // scaled about the scope's origin, which is the glyph's position here: it stays put
        let (gx, gy) = apply(&m, x as f64, y as f64);
        assert!((gx - x as f64).abs() < 1e-3 && (gy - y as f64).abs() < 1e-3);
        let (gx, _) = apply(&m, x as f64 + 1000.0, y as f64);
        assert!((gx - (x as f64 + 2000.0)).abs() < 1e-3);
    }

    #[test]
    fn unknown_things_are_errors_never_partial() {
        let (x, y) = (1000, 1000);
        for bad in ["/Sh sh", "/Fm0 Do", "BT /F1 10 Tf (a) Tj ET", "/GS0 gs", "/Pattern cs /P0 scn", "q"] {
            let p = page(vec![lit(bad, x, y)]);
            assert!(native_graphics(&p).is_err(), "{bad}");
        }
        let p = page(vec![Item::Literal { mode: 0, data: "q".into(), at: None }]);
        assert!(native_graphics(&p).is_err());
        let p = page(vec![Item::Literal { mode: -1, data: "x".into(), at: Some((0, 0)) }]);
        assert!(native_graphics(&p).is_err());
    }

    #[test]
    fn pgf_stitching_function_becomes_linear_stops() {
        let f = "<< /FunctionType 3 /Domain [0.0 100.0] /Functions [ << /FunctionType 2 /Domain [0.0 100.0] /C0 [1 0 0] /C1 [1 0 0] /N 1 >> << /FunctionType 2 /Domain [0.0 100.0] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> ] /Bounds [ 25.0 ] /Encode [0 1 0 1] >>";
        let obj = parse_obj(&pdf_tokens(f), &mut 0).unwrap();
        let stops = function_stops(&obj, 0.0, 100.0).unwrap();
        assert_eq!(stops[0], (0.0, vec![1.0, 0.0, 0.0]));
        assert_eq!(stops[2].0, 25.0);
        // the second piece runs its input 0..1 over 25..100
        assert_eq!(stops[3], (100.0, vec![0.0, 0.0, 1.0]));
        let bad = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 2 >>";
        assert!(function_stops(&parse_obj(&pdf_tokens(bad), &mut 0).unwrap(), 0.0, 1.0).is_err());
    }

    #[test]
    fn opacity_clip_dash_and_color_stack() {
        let (x, y) = (1000, 1000);
        let p = page(vec![
            Item::Color { stack: 0, cmd: Some(1), data: "0 0 1 rg 0 0 1 RG".into(), after: None },
            lit("q /pgf@ca0.5 gs [3 1] 0 d 0 0 10 10 re W n 0 0 5 5 re B Q", x, y),
            Item::Color { stack: 0, cmd: Some(2), data: String::new(), after: None },
        ]);
        let n = native_graphics(&p).unwrap();
        assert!(matches!(n.ops[0], GfxOp::Save { .. }));
        assert!(matches!(n.ops[1], GfxOp::Clip { even_odd: false, .. }));
        let GfxOp::Paint { fill: Some(f), stroke: Some(s), .. } = &n.ops[2] else { panic!("{:?}", n.ops[2]) };
        assert_eq!(*f, Color { comps: vec![0.0, 0.0, 1.0], alpha: 0.5 });
        assert_eq!(s.color, Color { comps: vec![0.0, 0.0, 1.0], alpha: 1.0 });
        assert_eq!(Color { comps: vec![0.0, 0.0, 0.4, 0.0], alpha: 1.0 }.rgb(), [1.0, 1.0, 0.6]);
        assert_eq!(s.dash, vec![3.0, 1.0]);
        assert!(matches!(n.ops[3], GfxOp::Restore { .. }));
    }
}
