//! Edit-mutation correctness: random edits are applied to a session, and every paragraph the
//! fast path served (status "ok") is compared, row by row and glyph by glyph, with the same
//! paragraph in the layout the next full background pass produces for the edited text. The fast
//! path may decline an edit (routed to the background, or `ok_degraded`); it must never serve a
//! result that the clean pass contradicts.
//!
//! Defaults to a small generated project. Environment:
//!   RTEX_MUTATION_PROJECT  project directory to mutate (a copy is made; the original is untouched)
//!   RTEX_MUTATION_MAIN     main file (default main.tex)
//!   RTEX_MUTATION_EDITS    number of edits (default 10)
//!   RTEX_MUTATION_BATCH    max edits per pass, on distinct paragraphs (default 3)
//!   RTEX_MUTATION_SEED     seed (default 1)
//!   RTEX_MUTATION_REPORT   write the per-edit verdicts as JSON to this path
//!   RTEX_MUTATION_WAIT     seconds to wait for a clean pass to converge (default 900)
//!   RTEX_MUTATION_SABOTAGE=1  corrupt one glyph of every served result before comparing: the
//!                          test must then fail (negative control for the comparison itself)
//! Skipped without lualatex, except when RTEX_MUTATION_PROJECT is set: then a missing TeX Live is
//! an error (a skip would otherwise report `ok` for a run that tested nothing).
//! On a timeout the test prints the session state and the last events it saw.

use rtex_core::fixtures::{generate, FontSet, Variant};
use rtex_core::layout::Fragment;
use rtex_core::session::{ParagraphPlacement, Versions};
use rtex_core::{Convergence, Edit, Event, ParaId, Session, SessionConfig};
use rtex_dl::{DisplayList, Item, Line};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn env_or<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Lowercase ASCII words delimited by single spaces, outside math, braces and brackets (option
/// lists: `\addplot[mark size=4pt]`), not part of a control sequence: editing them cannot break
/// the paragraph's syntax.
fn safe_words(par: &str) -> Vec<(usize, usize)> {
    let b = par.as_bytes();
    let (mut depth, mut brackets, mut dollars) = (0i32, 0i32, 0usize);
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
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        if c.is_ascii_lowercase() && depth == 0 && brackets <= 0 && dollars % 2 == 0 && (i == 0 || b[i - 1] == b' ')
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

/// A random mutation of the paragraph at `base` (absolute byte offset of `par`): (name, edit).
fn mutate(rng: &mut Rng, par: &str, base: usize) -> Option<(&'static str, Edit)> {
    // picture code in a paragraph (an inline tikzpicture or plot) is not text: a word there is
    // a key or a coordinate name
    if ["\\begin{tikzpicture}", "\\tikz", "\\begin{axis}", "\\addplot", "\\draw", "\\begin{circuitikz}"]
        .iter()
        .any(|k| par.contains(k))
    {
        return None;
    }
    let words = safe_words(par);
    if words.len() < 3 {
        return None;
    }
    let (s, e) = words[rng.below(words.len())];
    let w = &par[s..e];
    let edit = |a: usize, b: usize, t: String| Edit {
        start_byte: base + a,
        end_byte: base + b,
        text: t,
    };
    Some(match rng.below(7) {
        0 => {
            let (os, oe) = words[rng.below(words.len())];
            ("insert-word", edit(e, e, format!(" {}", &par[os..oe])))
        }
        1 => ("delete-word", edit(s, e + 1.min(par.len() - e), String::new())),
        2 if w.len() >= 4 => {
            let k = 1 + rng.below(w.len() - 2);
            let mut v: Vec<u8> = w.bytes().collect();
            v.swap(k, k - 1);
            ("typo", edit(s, e, String::from_utf8(v).unwrap()))
        }
        3 => ("emph", edit(s, e, format!("\\emph{{{w}}}"))),
        4 => ("bold", edit(s, e, format!("\\textbf{{{w}}}"))),
        5 => ("inline-math", edit(e, e, " $x^2+1$".into())),
        _ => {
            // repeat the first plain sentence at the end: changes the line count
            let end = par.find(". ")? + 1;
            let first = &par[..end];
            if first.contains(['\\', '$', '{', '%']) {
                return None;
            }
            let tail = par.trim_end().len();
            ("repeat-sentence", edit(tail, tail, format!(" {first}")))
        }
    })
}

struct State {
    pages: BTreeMap<i64, DisplayList>,
    placements: HashMap<ParaId, Vec<Fragment>>,
    kinds: HashMap<ParaId, String>,
    eligible: Vec<ParaId>,
    versions: Option<Versions>,
    converged: bool,
    /// The run ended (`Converged` or `PassLimitReached`): no further pass comes on its own.
    ended: bool,
    // keyed by (edit, paragraph): one edit can touch several spans (a split), each answers on its own
    updates: HashMap<(u64, ParaId), (ParaId, String, Vec<String>, bool, DisplayList)>,
    layouts: u32,
    /// The last events, one line each, with the time since the test started.
    log: std::collections::VecDeque<String>,
    t0: Instant,
}

impl State {
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
    fn dump(&self, s: &Session, what: &str) -> String {
        format!(
            "{what}\n  state: layouts {} converged {} versions {:?} session versions {:?}\n  last events:\n    {}",
            self.layouts,
            self.converged,
            self.versions,
            s.versions(),
            self.log.iter().cloned().collect::<Vec<_>>().join("\n    ")
        )
    }
    fn absorb(&mut self, e: Event) {
        self.note(&e);
        match e {
            Event::LayoutUpdate {
                versions,
                convergence,
                pages_changed,
                placements,
                eligible_paragraphs,
                ..
            } => {
                for p in pages_changed {
                    self.pages.insert(p.page, p.dl);
                }
                let pl: Vec<ParagraphPlacement> = placements;
                self.placements = pl.iter().map(|p| (p.par_id, p.fragments.clone())).collect();
                self.kinds = pl.iter().map(|p| (p.par_id, p.kind.clone())).collect();
                self.eligible = eligible_paragraphs;
                self.converged = matches!(convergence, Convergence::Converged);
                self.ended = matches!(convergence, Convergence::Converged | Convergence::PassLimitReached { .. });
                self.versions = Some(versions);
                self.layouts += 1;
            }
            Event::ParagraphUpdate {
                par_id,
                edit_id,
                status,
                reasons,
                pagination_stale,
                dl,
                ..
            } => {
                self.updates
                    .insert((edit_id, par_id), (par_id, status, reasons, pagination_stale, dl));
            }
            _ => {}
        }
    }
    fn pump(&mut self, s: &Session, timeout: Duration, mut until: impl FnMut(&State) -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if until(self) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            for e in s.poll(Duration::from_millis(200)) {
                self.absorb(e);
            }
        }
    }
    /// The rows of `id` in the current page lists, found by their placement coordinates.
    fn rows(&self, id: ParaId) -> Option<Vec<(&Line, &DisplayList)>> {
        let mut out = Vec::new();
        for f in self.placements.get(&id)? {
            let page = self.pages.get(&f.page)?;
            for (k, (x, y)) in f.xs.iter().zip(f.baselines.iter()).enumerate() {
                let _ = k;
                let l = page.lines.iter().find(|l| l.unit != 0 && l.x == *x && l.y == *y)?;
                out.push((l, page));
            }
        }
        Some(out)
    }
}

/// Differences between the served display list and the rows of the clean pass (empty = equal).
/// True when each pass row equals, in order, one of the fast rows (`diff` on single rows).
fn every_pass_row_is_a_fast_row(fast: &DisplayList, rows: &[(&Line, &DisplayList)]) -> bool {
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

fn diff(fast: &DisplayList, rows: &[(&Line, &DisplayList)]) -> Vec<String> {
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

#[test]
fn random_edits_are_never_served_wrong() {
    if let Err(e) = rtex_core::texlive::TexLive::discover() {
        assert!(
            std::env::var("RTEX_MUTATION_PROJECT").is_err(),
            "RTEX_MUTATION_PROJECT is set but TeX Live was not found: {e:#}"
        );
        eprintln!("SKIP: no TeX Live");
        return;
    }
    let wait = Duration::from_secs(env_or("RTEX_MUTATION_WAIT", 900));
    let root = std::env::temp_dir().join(format!("rtex-mutation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let main: String = env_or("RTEX_MUTATION_MAIN", "main.tex".to_string());
    match std::env::var("RTEX_MUTATION_PROJECT") {
        Ok(src) => copy_dir(std::path::Path::new(&src), &project),
        Err(_) => generate(3, Variant::Mixed, FontSet::Pagella, 5, &project).unwrap(),
    }
    let (n_edits, max_batch, seed): (usize, usize, u64) = (
        env_or("RTEX_MUTATION_EDITS", 10),
        env_or("RTEX_MUTATION_BATCH", 3),
        env_or("RTEX_MUTATION_SEED", 1),
    );
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let mut cfg = SessionConfig::new(&project, main.clone());
    if std::env::var("RTEX_MUTATION_NO_PICCACHE").is_ok() {
        cfg.picture_cache = false;
    }
    cfg.build_dir = root.join("build");
    let s = Session::open(cfg).unwrap();
    let mut st = State {
        pages: BTreeMap::new(),
        placements: HashMap::new(),
        kinds: HashMap::new(),
        eligible: vec![],
        versions: None,
        converged: false,
        ended: false,
        updates: HashMap::new(),
        layouts: 0,
        log: Default::default(),
        t0: Instant::now(),
    };
    if !st.pump(&s, wait, |st| st.layouts > 0) {
        panic!("{}", st.dump(&s, "no first layout"));
    }
    let mut report: Vec<serde_json::Value> = Vec::new();
    let (mut matched, mut wrong, mut declined, mut no_update, mut skipped, mut attribution_only) = (0, 0, 0, 0, 0, 0);
    let mut done = 0;
    let mut guard = 0;
    while done < n_edits && guard < n_edits * 6 {
        guard += 1;
        // a batch of edits on distinct eligible paragraphs
        let batch = 1 + rng.below(max_batch).min(n_edits - done - 1);
        let mut applied: Vec<(&'static str, ParaId, u64, String)> = Vec::new();
        let mut used: Vec<ParaId> = Vec::new();
        for _ in 0..batch {
            let doc = s.document_text(&main).unwrap();
            let spans = s.spans(&main);
            let cands: Vec<_> = spans
                .iter()
                .filter(|sp| st.eligible.contains(&sp.id) && !used.contains(&sp.id))
                .filter(|sp| st.kinds.get(&sp.id).map(|k| k == "par").unwrap_or(false))
                .collect();
            if cands.is_empty() {
                break;
            }
            let sp = cands[rng.below(cands.len())];
            let Some((name, edit)) = mutate(&mut rng, &doc[sp.range.clone()], sp.range.start) else {
                skipped += 1;
                continue;
            };
            let excerpt = doc[sp.range.clone()].chars().take(60).collect::<String>();
            match s.apply_edit(&main, edit) {
                Ok(r) => {
                    used.push(sp.id);
                    applied.push((name, sp.id, r.edit_id, format!("{} | routed {} {:?}", excerpt.replace('\n', " "), r.routed, r.reasons)));
                    if r.routed != "fast" {
                        // declined at routing: nothing to compare
                    }
                }
                Err(e) => panic!("apply_edit: {e}"),
            }
        }
        if applied.is_empty() {
            continue;
        }
        // collect the fast results, then force a clean pass and wait for it to converge
        st.pump(&s, Duration::from_secs(60), |st| {
            applied.iter().all(|(_, id, eid, note)| st.updates.contains_key(&(*eid, *id)) || !note.contains("routed fast"))
        });
        let rev = s.versions().source_revision;
        let before = st.layouts;
        s.request_layout();
        let ok = st.pump(&s, wait, |st| {
            st.layouts > before && st.ended && st.versions.as_ref().map(|v| v.source_revision >= rev).unwrap_or(false)
        });
        if ok && !st.converged {
            panic!("{}", st.dump(&s, "the clean pass ended without converging"));
        }
        if !ok {
            panic!("{}", st.dump(&s, &format!("the clean pass did not converge (waited {} s for source revision {rev:?}, layouts before request {before})", wait.as_secs())));
        }
        for (name, id, eid, note) in &applied {
            done += 1;
            let verdict;
            let mut details: Vec<String> = vec![];
            match st.updates.remove(&(*eid, *id)) {
                None if note.contains("routed fast") => {
                    no_update += 1;
                    verdict = "no-update";
                }
                None => {
                    declined += 1;
                    verdict = "declined-at-routing";
                }
                Some((_, status, reasons, _stale, dl)) => {
                    if status != "ok" {
                        declined += 1;
                        verdict = "declined-status";
                        details = reasons;
                        details.push(format!("status {status}"));
                    } else if let Some(rows) = st.rows(*id) {
                        let mut dl = dl;
                        if std::env::var("RTEX_MUTATION_SABOTAGE").is_ok() {
                            if let Some(Item::Glyph { x, .. }) = dl.lines.iter_mut().flat_map(|l| l.items.iter_mut()).find(|i| matches!(i, Item::Glyph { .. })) {
                                *x += 1;
                            }
                        }
                        let d = diff(&dl, &rows);
                        if d.is_empty() {
                            matched += 1;
                            verdict = "match";
                        } else if rows.len() < dl.lines.len() && every_pass_row_is_a_fast_row(&dl, &rows) {
                            // capture attributes the text after a display formula to the next unit:
                            // the rows it does attribute are all equal, the rest cannot be compared
                            attribution_only += 1;
                            verdict = "attribution-only";
                            details = d;
                        } else {
                            wrong += 1;
                            verdict = "MISMATCH";
                            details = d;
                        }
                    } else {
                        no_update += 1;
                        verdict = "paragraph-not-found-after-pass";
                    }
                }
            }
            println!("[{done:3}] {verdict:12} {name:15} {note}");
            for d in &details {
                println!("        {d}");
            }
            report.push(serde_json::json!({"n": done, "verdict": verdict, "mutation": name, "note": note, "details": details}));
        }
    }
    println!(
        "mutation summary: {done} edits: {matched} served and equal to the clean pass, {wrong} MISMATCH, {attribution_only} attribution-only (rows present all equal, rest not comparable), {declined} declined, {no_update} without comparable result, {skipped} not mutable"
    );
    if let Ok(p) = std::env::var("RTEX_MUTATION_REPORT") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&report).unwrap());
    }
    s.close();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(wrong, 0, "the fast path served results the clean pass contradicts");
    assert!(matched + declined > 0, "no edit could be judged");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        let t = to.join(e.file_name());
        if p.is_dir() {
            if e.file_name() != "build" {
                copy_dir(&p, &t);
            }
        } else {
            std::fs::copy(&p, &t).unwrap();
        }
    }
}
