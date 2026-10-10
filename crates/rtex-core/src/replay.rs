//! Driving a [`Session`] the way an editor does and judging what the fast path served: every
//! paragraph served with status "ok" is compared, row by row and glyph by glyph, with the same
//! paragraph in the layout of the next full background pass. Shared by the mutation test
//! (`tests/mutation.rs`) and the replay harness (`examples/replay.rs`).

use crate::document::{Edit, ParaId};
use crate::layout::Fragment;
use crate::session::{CompileStatus, Convergence, Diagnostic, Event, ParagraphPlacement, Session, Timing, Versions};
use rtex_dl::{DisplayList, Item, Line};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// xorshift64: deterministic, dependency free.
pub struct Rng(pub u64);
impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Lowercase ASCII words delimited by single spaces, outside math, braces and brackets (option
/// lists: `\draw[line width=1pt]`), not part of a control sequence and not right after one (a
/// primitive's keyword: `\pdfextension info {...}`): editing them cannot break the paragraph's
/// syntax. Byte ranges into `par`.
pub fn safe_words(par: &str) -> Vec<(usize, usize)> {
    let b = par.as_bytes();
    let (mut depth, mut brackets, mut dollars) = (0i32, 0i32, 0usize);
    let mut after_cs = false;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'$' => dollars += 1,
            b'\\' => {
                // skip the control sequence / escaped character
                i += 1;
                after_cs = i < b.len() && b[i].is_ascii_alphabetic();
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                continue;
            }
            b' ' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let keyword = std::mem::take(&mut after_cs);
        if c.is_ascii_lowercase()
            && !keyword
            && depth == 0
            && brackets <= 0
            && dollars % 2 == 0
            && (i == 0 || b[i - 1] == b' ')
        {
            let s = i;
            while i < b.len() && b[i].is_ascii_lowercase() {
                i += 1;
            }
            if i - s >= 3 && (i == b.len() || b[i] == b' ' || b[i] == b',' || b[i] == b'.') {
                out.push((s, i));
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Picture code in a paragraph (an inline tikzpicture or plot) is not text: a word there is a
/// key or a coordinate name.
pub fn has_picture_code(par: &str) -> bool {
    ["\\begin{tikzpicture}", "\\tikz", "\\begin{axis}", "\\addplot", "\\draw", "\\begin{circuitikz}"]
        .iter()
        .any(|k| par.contains(k))
}

/// One paragraph result of the fast path, as it arrived.
#[derive(Debug, Clone)]
pub struct Served {
    pub par_id: ParaId,
    pub status: String,
    pub reasons: Vec<String>,
    pub pagination_stale: bool,
    pub dl: DisplayList,
    pub timing: Timing,
    /// When the event was taken from the session (host-observed arrival).
    pub at: Instant,
}

/// The state an editor builds from the session's events.
pub struct Observer {
    pub pages: BTreeMap<i64, DisplayList>,
    pub pages_total: i64,
    pub placements: HashMap<ParaId, Vec<Fragment>>,
    pub kinds: HashMap<ParaId, String>,
    pub eligible: Vec<ParaId>,
    pub versions: Option<Versions>,
    pub compile: Option<CompileStatus>,
    pub converged: bool,
    /// The run ended (`Converged` or `PassLimitReached`): no further pass comes on its own.
    pub ended: bool,
    /// Keyed by (edit, paragraph): one edit can touch several spans (a split), each answers on its own.
    pub updates: HashMap<(u64, ParaId), Served>,
    /// Edits the session sent to the background path (`BackgroundScheduled`), with the arrival time.
    pub background: HashMap<u64, Instant>,
    /// The diagnostics in force per source ("background", "fast:<par>"), kept the way an editor
    /// keeps them: a LayoutUpdate clears the background list unless the pass sent one, and clears
    /// the fast-path lists.
    pub diagnostics: HashMap<String, Vec<Diagnostic>>,
    background_diagnostics_this_pass: bool,
    pub layouts: u32,
    /// Arrival time of the latest LayoutUpdate.
    pub layout_at: Option<Instant>,
    /// The last events, one line each, with the time since `t0`.
    pub log: VecDeque<String>,
    pub t0: Instant,
}

impl Default for Observer {
    fn default() -> Self {
        Observer::new()
    }
}

impl Observer {
    pub fn new() -> Observer {
        Observer {
            pages: BTreeMap::new(),
            pages_total: 0,
            placements: HashMap::new(),
            kinds: HashMap::new(),
            eligible: vec![],
            versions: None,
            compile: None,
            converged: false,
            ended: false,
            updates: HashMap::new(),
            background: HashMap::new(),
            diagnostics: HashMap::new(),
            background_diagnostics_this_pass: false,
            layouts: 0,
            layout_at: None,
            log: VecDeque::new(),
            t0: Instant::now(),
        }
    }

    fn note(&mut self, e: &Event) {
        let line = match e {
            Event::LayoutUpdate { versions, compile, convergence, passes, pages_changed, pages_total, wall_ms, .. } => format!(
                "LayoutUpdate rev {} layout_v {} gen {} compile {:?} convergence {:?} passes {} pages_changed {} of {} wall {} ms",
                versions.source_revision, versions.layout_version, versions.engine_generation, compile, convergence, passes, pages_changed.len(), pages_total, wall_ms
            ),
            Event::ParagraphUpdate { par_id, edit_id, status, reasons, versions, .. } => {
                format!("ParagraphUpdate par {par_id:?} edit {edit_id} status {status} reasons {reasons:?} rev {}", versions.source_revision)
            }
            Event::Diagnostics { source, items } => format!(
                "Diagnostics {source}: {} item(s){}",
                items.len(),
                items.iter().find(|d| d.severity == "error").or(items.first()).map(|d| format!(", first error (or item): {}", serde_json::to_string(d).unwrap_or_default().chars().take(300).collect::<String>())).unwrap_or_default()
            ),
            Event::EngineState { engine_generation, state, reason } => format!("EngineState gen {engine_generation} {state} {reason:?}"),
            Event::BackgroundScheduled { par_id, reasons, edit_id } => format!("BackgroundScheduled par {par_id:?} edit {edit_id} reasons {reasons:?}"),
            Event::PdfExported { job_id, status, converged, passes, .. } => format!("PdfExported job {job_id} {status:?} converged {converged} passes {passes}"),
        };
        self.log.push_back(format!("{:9.3}s {line}", self.t0.elapsed().as_secs_f64()));
        if self.log.len() > 60 {
            self.log.pop_front();
        }
    }

    /// The observer's state and the last events, for a failure message.
    pub fn dump(&self, s: &Session, what: &str) -> String {
        format!(
            "{what}\n  state: layouts {} converged {} versions {:?} session versions {:?}\n  last events:\n    {}",
            self.layouts,
            self.converged,
            self.versions,
            s.versions(),
            self.log.iter().cloned().collect::<Vec<_>>().join("\n    ")
        )
    }

    pub fn absorb(&mut self, e: Event) {
        self.note(&e);
        let now = Instant::now();
        match e {
            Event::LayoutUpdate {
                versions,
                compile,
                convergence,
                pages_changed,
                pages_total,
                placements,
                eligible_paragraphs,
                ..
            } => {
                for p in pages_changed {
                    self.pages.insert(p.page, p.dl);
                }
                self.pages_total = pages_total;
                let pl: Vec<ParagraphPlacement> = placements;
                self.placements = pl.iter().map(|p| (p.par_id, p.fragments.clone())).collect();
                self.kinds = pl.iter().map(|p| (p.par_id, p.kind.clone())).collect();
                self.eligible = eligible_paragraphs;
                self.converged = matches!(convergence, Convergence::Converged);
                self.ended = matches!(convergence, Convergence::Converged | Convergence::PassLimitReached { .. });
                self.versions = Some(versions);
                self.compile = Some(compile);
                self.layouts += 1;
                self.layout_at = Some(now);
                let keep_background = std::mem::take(&mut self.background_diagnostics_this_pass);
                self.diagnostics.retain(|src, _| keep_background && src == "background");
            }
            Event::ParagraphUpdate {
                par_id,
                edit_id,
                status,
                reasons,
                pagination_stale,
                dl,
                timing,
                ..
            } => {
                self.updates.insert(
                    (edit_id, par_id),
                    Served { par_id, status, reasons, pagination_stale, dl, timing, at: now },
                );
            }
            Event::BackgroundScheduled { edit_id, .. } => {
                self.background.entry(edit_id).or_insert(now);
            }
            Event::Diagnostics { source, items } => {
                if source == "background" {
                    self.background_diagnostics_this_pass = true;
                }
                self.diagnostics.insert(source, items);
            }
            _ => {}
        }
    }

    /// Absorb events until `until` holds (true) or `timeout` passes (false).
    pub fn pump(&mut self, s: &Session, timeout: Duration, mut until: impl FnMut(&Observer) -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if until(self) {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            for e in s.poll((end - now).min(Duration::from_millis(200))) {
                self.absorb(e);
            }
        }
    }

    /// Ask for a clean pass and wait until a run that covers source revision `rev` has ended.
    /// Returns false on timeout.
    pub fn settle(&mut self, s: &Session, rev: u64, timeout: Duration) -> bool {
        let before = self.layouts;
        s.request_layout();
        self.pump(s, timeout, |o| {
            o.layouts > before && o.ended && o.versions.as_ref().map(|v| v.source_revision >= rev).unwrap_or(false)
        })
    }

    /// The rows of `id` in the current page lists, found by their placement coordinates.
    pub fn rows(&self, id: ParaId) -> Option<Vec<(&Line, &DisplayList)>> {
        let mut out = Vec::new();
        for f in self.placements.get(&id)? {
            let page = self.pages.get(&f.page)?;
            for (x, y) in f.xs.iter().zip(f.baselines.iter()) {
                let l = page.lines.iter().find(|l| l.unit != 0 && l.x == *x && l.y == *y)?;
                out.push((l, page));
            }
        }
        Some(out)
    }

    /// Judge a served result against the current (settled) layout.
    pub fn judge(&self, served: &Served) -> Judgement {
        if served.status != "ok" {
            let mut d = served.reasons.clone();
            d.push(format!("status {}", served.status));
            return Judgement { verdict: Verdict::DeclinedStatus, details: d };
        }
        let Some(rows) = self.rows(served.par_id) else {
            return Judgement { verdict: Verdict::NotFoundAfterPass, details: vec![] };
        };
        let d = diff(&served.dl, &rows);
        if d.is_empty() {
            Judgement { verdict: Verdict::Match, details: d }
        } else if rows.len() < served.dl.lines.len() && every_pass_row_is_a_fast_row(&served.dl, &rows) {
            // capture attributes the text after a display formula to the next unit: the rows it
            // does attribute are all equal, the rest cannot be compared
            Judgement { verdict: Verdict::AttributionOnly, details: d }
        } else {
            Judgement { verdict: Verdict::Mismatch, details: d }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    Match,
    AttributionOnly,
    Mismatch,
    DeclinedStatus,
    NotFoundAfterPass,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Match => "match",
            Verdict::AttributionOnly => "attribution-only",
            Verdict::Mismatch => "MISMATCH",
            Verdict::DeclinedStatus => "declined-status",
            Verdict::NotFoundAfterPass => "paragraph-not-found-after-pass",
        }
    }
}

pub struct Judgement {
    pub verdict: Verdict,
    pub details: Vec<String>,
}

/// True when each pass row equals, in order, one of the fast rows (`diff` on single rows).
pub fn every_pass_row_is_a_fast_row(fast: &DisplayList, rows: &[(&Line, &DisplayList)]) -> bool {
    let mut at = 0;
    for r in rows {
        let mut found = false;
        while at < fast.lines.len() {
            let mut one = fast.clone();
            one.lines = vec![fast.lines[at].clone()];
            at += 1;
            if diff(&one, std::slice::from_ref(r)).is_empty() {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Differences between the served display list and the rows of the clean pass (empty = equal).
pub fn diff(fast: &DisplayList, rows: &[(&Line, &DisplayList)]) -> Vec<String> {
    let mut d = Vec::new();
    if fast.lines.len() != rows.len() {
        d.push(format!("row count: fast {} vs pass {}", fast.lines.len(), rows.len()));
        return d;
    }
    for (k, (fl, (rl, page))) in fast.lines.iter().zip(rows.iter()).enumerate() {
        if fl.w != rl.w || fl.h != rl.h || fl.d != rl.d || (fl.gs - rl.gs).abs() > 1e-12 {
            d.push(format!("row {} box: fast ({},{},{} gs {}) pass ({},{},{} gs {})", k + 1, fl.w, fl.h, fl.d, fl.gs, rl.w, rl.h, rl.d, rl.gs));
        }
        let (ox, oy) = (rl.x - fl.x, rl.y - fl.y);
        let fg: Vec<&Item> = fl.items.iter().filter(|i| matches!(i, Item::Glyph { .. })).collect();
        let rg: Vec<&Item> = rl.items.iter().filter(|i| matches!(i, Item::Glyph { .. })).collect();
        if fg.len() != rg.len() {
            d.push(format!("row {} glyph count {} vs {}", k + 1, fg.len(), rg.len()));
            continue;
        }
        for (a, b) in fg.iter().zip(rg.iter()) {
            if let (
                Item::Glyph { font: fa, char: ca, index: ia, x: xa, y: ya, width: wa, expansion: ea },
                Item::Glyph { font: fb, char: cb, index: ib, x: xb, y: yb, width: wb, expansion: eb },
            ) = (a, b)
            {
                let font_ok = match (fast.font(*fa), page.font(*fb)) {
                    (Some(x), Some(y)) => x.key() == y.key(),
                    _ => fa == fb,
                };
                if !(font_ok && ca == cb && ia == ib && xa + ox == *xb && ya + oy == *yb && wa == wb && ea == eb)
                    && d.len() < 8
                {
                    d.push(format!("row {} glyph: fast {:?} pass {:?}", k + 1, a, b));
                }
            }
        }
        for (kind, pred) in [
            ("rule", (|i: &&Item| matches!(i, Item::Rule { .. })) as fn(&&Item) -> bool),
            ("image", |i: &&Item| matches!(i, Item::Image { .. })),
        ] {
            let (a, b) = (fl.items.iter().filter(pred).count(), rl.items.iter().filter(pred).count());
            if a != b {
                d.push(format!("row {} {kind} count {a} vs {b}", k + 1));
            }
        }
    }
    d
}

/// Byte offset of a (0-based line, 0-based column in Unicode code points) position in `text`,
/// lines separated by `\n` (the convention of LSP-style edit scripts that count code points).
pub fn byte_offset(text: &str, line: usize, col: usize) -> Option<usize> {
    let mut start = 0;
    for _ in 0..line {
        start += text[start..].find('\n')? + 1;
    }
    let rest = &text[start..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let mut chars = rest[..line_end].char_indices();
    match chars.nth(col) {
        Some((b, _)) => Some(start + b),
        None if rest[..line_end].chars().count() == col => Some(start + line_end),
        None => None,
    }
}

/// An edit replacing the (line, col)–(line, col) range of `text` (code point columns).
pub fn edit_at(text: &str, start: (usize, usize), end: (usize, usize), new: &str) -> Option<Edit> {
    Some(Edit {
        start_byte: byte_offset(text, start.0, start.1)?,
        end_byte: byte_offset(text, end.0, end.1)?,
        text: new.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_point_columns_become_byte_offsets() {
        let t = "ab\nnaïve x\n𠀀z\n";
        assert_eq!(byte_offset(t, 0, 0), Some(0));
        assert_eq!(byte_offset(t, 0, 2), Some(2)); // end of line
        assert_eq!(byte_offset(t, 1, 3), Some(7)); // after 'ï' (2 bytes)
        assert_eq!(byte_offset(t, 2, 1), Some(16)); // after U+20000 (4 bytes)
        assert_eq!(byte_offset(t, 3, 0), Some(t.len())); // the empty line after the final \n
        assert_eq!(byte_offset(t, 0, 3), None);
        assert_eq!(byte_offset(t, 5, 0), None);
    }

    #[test]
    fn safe_words_skip_commands_math_and_options() {
        let p = "the \\emph{word} and $x$ math [opt] plain words. \\pdfextension info {x} done.";
        let w: Vec<&str> = safe_words(p).iter().map(|&(s, e)| &p[s..e]).collect();
        assert_eq!(w, ["the", "and", "math", "plain", "words", "done"]);
    }
}
