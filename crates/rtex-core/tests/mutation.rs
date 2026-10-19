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
//!   RTEX_MUTATION_KINDS    edit families, comma-separated (default `text`; `all` for every one):
//!                          text      words, emphasis, inline math and sentences in paragraphs
//!                          env       the same text edits inside lists, theorems and other
//!                                    block environments
//!                          math      a digit or a term inside existing inline math
//!                          picture   a node label or a coordinate in TikZ picture code
//!                          option    a number in an environment's option list
//!                          preamble  a new unused definition before \begin{document} (restarts
//!                                    the live engine; the edits after it test the restarted one)
//! Skipped without lualatex, except when RTEX_MUTATION_PROJECT is set: then a missing TeX Live is
//! an error (a skip would otherwise report `ok` for a run that tested nothing).
//! On a timeout the test prints the session state and the last events it saw.

use rtex_core::fixtures::{generate, FontSet, Variant};
use rtex_core::replay::{has_picture_code, safe_words, Observer, Rng, Verdict};
use rtex_core::{Edit, ParaId, Session, SessionConfig};
use rtex_dl::Item;
use std::time::Duration;

fn env_or<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

/// Edit families (`RTEX_MUTATION_KINDS`); `preamble` is applied between batches, not to a unit.
const FAMILIES: &[&str] = &["text", "env", "math", "picture", "option", "preamble"];

fn families() -> Vec<&'static str> {
    let v = std::env::var("RTEX_MUTATION_KINDS").unwrap_or_else(|_| "text".into());
    if v.trim() == "all" {
        return FAMILIES.to_vec();
    }
    let out: Vec<&'static str> = FAMILIES
        .iter()
        .copied()
        .filter(|f| v.split(',').any(|x| x.trim() == *f))
        .collect();
    assert!(
        !out.is_empty(),
        "RTEX_MUTATION_KINDS={v}: none of {FAMILIES:?}"
    );
    out
}

/// Does a unit of layout kind `kind` with source `par` belong to `family`?
fn in_family(family: &str, kind: &str, par: &str) -> bool {
    let picture = has_picture_code(par);
    match family {
        "text" => kind == "par" && !picture,
        "env" => kind.starts_with("env:") && !picture,
        "math" => !picture && inline_math(par).next().is_some(),
        "picture" => picture,
        "option" => option_numbers(par).next().is_some(),
        _ => false,
    }
}

/// Byte ranges of the contents of `$…$` (not `$$`), without commands that open environments.
fn inline_math(par: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let b = par.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() {
            if b[i] == b'\\' {
                i += 2;
                continue;
            }
            if b[i] == b'$' && b.get(i + 1) != Some(&b'$') && (i == 0 || b[i - 1] != b'$') {
                let s = i + 1;
                let e = s + par[s..].find('$')?;
                i = e + 1;
                if e > s && !par[s..e].contains("\\begin") && !par[s..e].contains('%') {
                    return Some((s, e));
                }
                continue;
            }
            i += 1;
        }
        None
    })
}

/// Numbers in `\begin{env}[…]` option lists (`scale=0.8`, `width=3cm`): byte ranges.
fn option_numbers(par: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let re = regex::Regex::new(r"\\begin\{[A-Za-z*]+\}\[([^\]\n]*)\]").unwrap();
    let num = regex::Regex::new(r"=\s*(\d+(?:\.\d+)?)").unwrap();
    let mut out = Vec::new();
    for c in re.captures_iter(par) {
        let opts = c.get(1).unwrap();
        for n in num.captures_iter(opts.as_str()) {
            let m = n.get(1).unwrap();
            out.push((opts.start() + m.start(), opts.start() + m.end()));
        }
    }
    out.into_iter()
}

/// A number edited a little: `0.8` -> `0.9`, `3` -> `4` (never zero, never negative).
fn nudge(n: &str) -> String {
    match n.split_once('.') {
        Some((i, f)) => {
            let last = f.as_bytes()[f.len() - 1];
            let d = if last == b'9' { b'1' } else { last + 1 };
            format!("{i}.{}{}", &f[..f.len() - 1], d as char)
        }
        None => (n.parse::<u64>().unwrap_or(1) + 1).to_string(),
    }
}

/// A mutation of a family other than `text`, or None when the unit offers none.
fn mutate_family(
    family: &str,
    rng: &mut Rng,
    par: &str,
    base: usize,
) -> Option<(&'static str, Edit)> {
    let edit = |a: usize, b: usize, t: String| Edit {
        start_byte: base + a,
        end_byte: base + b,
        text: t,
    };
    match family {
        "env" => {
            // text edits on the environment's own words (items, theorem bodies, captions)
            let (name, e) = mutate_text(rng, par, base, true)?;
            let name = match name {
                "insert-word" => "env-insert-word",
                "delete-word" => "env-delete-word",
                "typo" => "env-typo",
                "emph" => "env-emph",
                "bold" => "env-bold",
                "inline-math" => "env-inline-math",
                other => other,
            };
            Some((name, e))
        }
        "math" => {
            let segs: Vec<_> = inline_math(par).collect();
            let (s, e) = segs[rng.below(segs.len())];
            let digits: Vec<usize> = (s..e)
                .filter(|&k| par.as_bytes()[k].is_ascii_digit())
                .collect();
            if !digits.is_empty() && rng.below(2) == 0 {
                let k = digits[rng.below(digits.len())];
                let d = par.as_bytes()[k];
                let nd = if d == b'9' { b'1' } else { d + 1 };
                Some(("math-digit", edit(k, k + 1, (nd as char).to_string())))
            } else {
                Some(("math-term", edit(e, e, "+1".into())))
            }
        }
        "picture" => {
            // a node label's word, or a number in a coordinate
            let label = regex::Regex::new(r"node(?:\s*\[[^\]]*\])?(?:\s*\([^)]*\))?(?:\s*at\s*\([^)]*\))?\s*\{([^{}\\$]*[a-z]{3,}[^{}\\$]*)\}").unwrap();
            let coord = regex::Regex::new(r"\((-?\d+(?:\.\d+)?)\s*,\s*-?\d+(?:\.\d+)?\)").unwrap();
            let labels: Vec<_> = label
                .captures_iter(par)
                .map(|c| c.get(1).unwrap())
                .collect();
            let coords: Vec<_> = coord
                .captures_iter(par)
                .map(|c| c.get(1).unwrap())
                .collect();
            if !labels.is_empty() && (coords.is_empty() || rng.below(2) == 0) {
                let m = labels[rng.below(labels.len())];
                let w = regex::Regex::new(r"[a-z]{3,}").unwrap();
                let word = w.find(m.as_str())?;
                let (s, e) = (m.start() + word.start(), m.start() + word.end());
                let mut v: Vec<u8> = par[s..e].bytes().collect();
                v.swap(1, 2);
                Some(("picture-label", edit(s, e, String::from_utf8(v).unwrap())))
            } else if !coords.is_empty() {
                let m = coords[rng.below(coords.len())];
                Some(("picture-coord", edit(m.start(), m.end(), nudge(m.as_str()))))
            } else {
                None
            }
        }
        "option" => {
            let nums: Vec<_> = option_numbers(par).collect();
            let (s, e) = nums[rng.below(nums.len())];
            Some(("option-number", edit(s, e, nudge(&par[s..e]))))
        }
        _ => None,
    }
}

/// A random mutation of the paragraph at `base` (absolute byte offset of `par`): (name, edit).
fn mutate(rng: &mut Rng, par: &str, base: usize) -> Option<(&'static str, Edit)> {
    mutate_text(rng, par, base, false)
}

/// Text edits; `in_env`: the unit is a block environment (its first and last lines are the
/// `\begin`/`\end`, so the sentence repeat, which appends at the end, is not used).
fn mutate_text(
    rng: &mut Rng,
    par: &str,
    base: usize,
    in_env: bool,
) -> Option<(&'static str, Edit)> {
    if has_picture_code(par) {
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
        1 => (
            "delete-word",
            edit(s, e + 1.min(par.len() - e), String::new()),
        ),
        2 if w.len() >= 4 => {
            let k = 1 + rng.below(w.len() - 2);
            let mut v: Vec<u8> = w.bytes().collect();
            v.swap(k, k - 1);
            ("typo", edit(s, e, String::from_utf8(v).unwrap()))
        }
        3 => ("emph", edit(s, e, format!("\\emph{{{w}}}"))),
        4 => ("bold", edit(s, e, format!("\\textbf{{{w}}}"))),
        5 => ("inline-math", edit(e, e, " $x^2+1$".into())),
        _ if in_env => return None,
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
    let mut rng = Rng::new(seed);
    let fams = families();
    let unit_fams: Vec<&str> = fams.iter().copied().filter(|f| *f != "preamble").collect();
    let mut preamble_edits = 0usize;
    // picture and option edits mostly give `ok_degraded` results (PDF literals): judge their
    // drawing code too instead of declining them
    let drawings = fams.contains(&"picture") || fams.contains(&"option");
    let mut cfg = SessionConfig::new(&project, main.clone());
    if std::env::var("RTEX_MUTATION_NO_PICCACHE").is_ok() {
        cfg.picture_cache = false;
    }
    cfg.build_dir = root.join("build");
    let s = Session::open(cfg).unwrap();
    let mut st = Observer::new();
    if !st.pump(&s, wait, |st| st.layouts > 0) {
        panic!("{}", st.dump(&s, "no first layout"));
    }
    let mut report: Vec<serde_json::Value> = Vec::new();
    let (mut matched, mut wrong, mut declined, mut no_update, mut skipped, mut attribution_only) =
        (0, 0, 0, 0, 0, 0);
    let mut done = 0;
    let mut guard = 0;
    while done < n_edits && guard < n_edits * 6 {
        guard += 1;
        // a batch of edits on distinct eligible paragraphs
        let batch = 1 + rng.below(max_batch).min(n_edits - done - 1);
        let mut applied: Vec<(&'static str, ParaId, u64, String)> = Vec::new();
        let mut used: Vec<ParaId> = Vec::new();
        if fams.contains(&"preamble") && rng.below(6) == 0 {
            // an unused definition: the live engine restarts with the new preamble, and the
            // edits of this batch are served (or declined) by the restarted engine
            let doc = s.document_text(&main).unwrap();
            if let Some(at) = doc.find("\\begin{document}") {
                preamble_edits += 1;
                let def = format!(
                    "\\providecommand{{\\rtexmut{}}}{{{preamble_edits}}}\n",
                    "x".repeat(preamble_edits)
                );
                s.apply_edit(
                    &main,
                    Edit {
                        start_byte: at,
                        end_byte: at,
                        text: def,
                    },
                )
                .unwrap();
                println!(
                    "[ preamble ] a new definition before \\begin{{document}} ({preamble_edits})"
                );
            }
        }
        for _ in 0..batch {
            let doc = s.document_text(&main).unwrap();
            let spans = s.spans(&main);
            // the family of this edit (no draw with `text` alone: earlier seeds keep their edits)
            let family = if unit_fams.len() > 1 {
                unit_fams[rng.below(unit_fams.len())]
            } else {
                unit_fams.first().copied().unwrap_or("text")
            };
            let cands: Vec<_> = spans
                .iter()
                .filter(|sp| !used.contains(&sp.id))
                .filter(|sp| {
                    let kind = st.kinds.get(&sp.id).map(String::as_str).unwrap_or("");
                    if family == "text" {
                        // the original selection: allow-listed plain paragraphs
                        st.eligible.contains(&sp.id) && kind == "par"
                    } else {
                        // whatever the session decides for them: served or declined
                        !kind.is_empty() && in_family(family, kind, &doc[sp.range.clone()])
                    }
                })
                .collect();
            if cands.is_empty() {
                if unit_fams.len() > 1 {
                    skipped += 1;
                    continue;
                }
                break;
            }
            let sp = cands[rng.below(cands.len())];
            let par = &doc[sp.range.clone()];
            let mutation = if family == "text" {
                mutate(&mut rng, par, sp.range.start)
            } else {
                mutate_family(family, &mut rng, par, sp.range.start)
            };
            let Some((name, edit)) = mutation else {
                skipped += 1;
                continue;
            };
            let excerpt = doc[sp.range.clone()].chars().take(60).collect::<String>();
            match s.apply_edit(&main, edit) {
                Ok(r) => {
                    used.push(sp.id);
                    applied.push((
                        name,
                        sp.id,
                        r.edit_id,
                        format!(
                            "{} | routed {} {:?}",
                            excerpt.replace('\n', " "),
                            r.routed,
                            r.reasons
                        ),
                    ));
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
            applied.iter().all(|(_, id, eid, note)| {
                st.updates.contains_key(&(*eid, *id)) || !note.contains("routed fast")
            })
        });
        let rev = s.versions().source_revision;
        let before = st.layouts;
        s.request_layout();
        let ok = st.pump(&s, wait, |st| {
            st.layouts > before
                && st.ended
                && st
                    .versions
                    .as_ref()
                    .map(|v| v.source_revision >= rev)
                    .unwrap_or(false)
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
                Some(mut served) => {
                    if std::env::var("RTEX_MUTATION_SABOTAGE").is_ok()
                        && served.status == "ok_degraded"
                    {
                        // the drawing comparison's negative control: one literal's code
                        if let Some(Item::Literal { data, .. }) = served
                            .dl
                            .lines
                            .iter_mut()
                            .flat_map(|l| l.items.iter_mut())
                            .find(|i| matches!(i, Item::Literal { .. }))
                        {
                            data.push_str(" 0 0 m");
                        }
                    }
                    if std::env::var("RTEX_MUTATION_SABOTAGE").is_ok() && served.status == "ok" {
                        if let Some(Item::Glyph { x, .. }) = served
                            .dl
                            .lines
                            .iter_mut()
                            .flat_map(|l| l.items.iter_mut())
                            .find(|i| matches!(i, Item::Glyph { .. }))
                        {
                            *x += 1;
                        }
                    }
                    let j = st.judge_with(&served, drawings);
                    verdict = j.verdict.name();
                    details = j.details;
                    match j.verdict {
                        Verdict::Match => matched += 1,
                        Verdict::AttributionOnly => attribution_only += 1,
                        Verdict::Mismatch => wrong += 1,
                        Verdict::DeclinedStatus => declined += 1,
                        Verdict::NotFoundAfterPass => no_update += 1,
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
        "mutation summary: {done} edits: {matched} served and equal to the clean pass, {wrong} MISMATCH, {attribution_only} attribution-only (rows present all equal, rest not comparable), {declined} declined, {no_update} without comparable result, {skipped} not mutable; {preamble_edits} preamble edits; families {fams:?}"
    );
    if let Ok(p) = std::env::var("RTEX_MUTATION_REPORT") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&report).unwrap());
    }
    s.close();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        wrong, 0,
        "the fast path served results the clean pass contradicts"
    );
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
