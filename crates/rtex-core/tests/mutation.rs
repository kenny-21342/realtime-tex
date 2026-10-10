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
use rtex_core::replay::{has_picture_code, safe_words, Observer, Rng, Verdict};
use rtex_core::{Edit, ParaId, Session, SessionConfig};
use rtex_dl::Item;
use std::time::Duration;

fn env_or<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// A random mutation of the paragraph at `base` (absolute byte offset of `par`): (name, edit).
fn mutate(rng: &mut Rng, par: &str, base: usize) -> Option<(&'static str, Edit)> {
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
                Some(mut served) => {
                    if std::env::var("RTEX_MUTATION_SABOTAGE").is_ok() && served.status == "ok" {
                        if let Some(Item::Glyph { x, .. }) = served.dl.lines.iter_mut().flat_map(|l| l.items.iter_mut()).find(|i| matches!(i, Item::Glyph { .. })) {
                            *x += 1;
                        }
                    }
                    let j = st.judge(&served);
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
