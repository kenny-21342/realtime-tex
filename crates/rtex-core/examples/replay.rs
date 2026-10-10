//! Latency and correctness of live editing, measured the way an editor sees it.
//!
//! ```text
//! cargo run --release -p rtex-core --example replay -- ops  --ops OPS.jsonl --root ROOT --out DIR
//!                                                     [--main main.tex] [--cases CASES.json]
//! cargo run --release -p rtex-core --example replay -- type --project P --main M --out DIR
//!                                                     [--units 12] [--word quick] [--cadence-ms 120]
//!                                                     [--seed 1] [--settle-each] [--files a.tex,b.tex]
//! ```
//!
//! `ops` replays an edit script in the format of the TeX stress test's edit suite
//! (`suites/edits/snapshots/<name>/ops.jsonl`: one step per line; ranges in 0-based lines and
//! code-point columns; `create`, `delete_file`, `binary_from` relative to ROOT). Step 0 builds
//! the base project and opens the session; every later step's range edits go to the session's
//! buffers (and to disk), file operations go to disk, then a clean pass is requested. After each
//! step the files on disk are checked against the script's SHA-256s. With `--cases` (the suite's
//! `cases.json`) each step also reports the expectation recorded for the reference engine.
//!
//! `check` opens a session on a copy of a project, waits (`--timeout` s, default 120) for the
//! first run to end and reports its status, pages and error locations (what an editor's problem
//! list shows): scripts/broken_errors.py runs it over the stress test's `broken` suite. With
//! `--build DIR` the session's build directory is DIR, kept across runs: a second run is a
//! reopened session (`first_layout_ms`).
//!
//! `type` copies a project, then types ` WORD` one character at a time at the end of a word in
//! N paragraphs spread over the project's files, CADENCE ms apart (continuous typing: background
//! passes run while typing). With `--settle-each`, a clean pass is awaited after every word.
//!
//! For every edit: routing, host-observed latency of the fast result (apply_edit to event
//! arrival) and the engine's own time. For every settle: time from the last edit to the layout,
//! compile status, pages, and the verdict of the last result served for each edited paragraph
//! against the settled layout (`rtex_core::replay`, as in tests/mutation.rs). Writes
//! DIR/report.json and prints a summary; exits 1 if any served result was wrong.
//! REPLAY_EVENTS=1 prints the session's events at every settle. Experiment switches:
//! REPLAY_NO_PICCACHE=1, REPLAY_COLD_BACKGROUND=1 (no standby engine), REPLAY_BUDGET_MS=<ms>.
use anyhow::{anyhow, bail, Context, Result};
use rtex_core::replay::{edit_at, has_picture_code, safe_words, Observer, Rng, Verdict};
use rtex_core::session::CompileStatus;
use rtex_core::{Edit, ParaId, Session, SessionConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const OPEN_TIMEOUT: Duration = Duration::from_secs(1800);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(1800);
const FAST_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> Result<()> {
    env_logger::init();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mode = a.first().cloned().unwrap_or_default();
    let mut opt: HashMap<String, String> = HashMap::new();
    let mut i = 1;
    while i < a.len() {
        let k = a[i]
            .strip_prefix("--")
            .ok_or_else(|| anyhow!("unexpected argument {}", a[i]))?
            .to_string();
        if k == "settle-each" {
            opt.insert(k, "1".into());
            i += 1;
        } else {
            opt.insert(
                k,
                a.get(i + 1)
                    .cloned()
                    .ok_or_else(|| anyhow!("--{} needs a value", a[i]))?,
            );
            i += 2;
        }
    }
    let get = |k: &str| opt.get(k).cloned().ok_or_else(|| anyhow!("missing --{k}"));
    let out = PathBuf::from(get("out")?);
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    let mut run = Run::default();
    let report = match mode.as_str() {
        "ops" => replay_ops(
            &mut run,
            &PathBuf::from(get("ops")?),
            &PathBuf::from(get("root")?),
            &opt.get("main").cloned().unwrap_or("main.tex".into()),
            opt.get("cases").map(PathBuf::from),
            &out,
        )?,
        "type" => type_words(
            &mut run,
            &PathBuf::from(get("project")?),
            &get("main")?,
            &opt,
            &out,
        )?,
        "check" => check(&PathBuf::from(get("project")?), &get("main")?, &opt, &out)?,
        _ => bail!("usage: replay ops|type ... (see the header of examples/replay.rs)"),
    };
    std::fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    run.print_summary();
    if run.wrong > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// One applied edit, waiting for its results.
struct Applied {
    label: String,
    file: String,
    edit_id: u64,
    rev: u64,
    sent: Instant,
    routed: String,
    reasons: Vec<String>,
    apply_us: u128,
    /// Paragraphs the edit changed or created.
    touched: Vec<ParaId>,
}

#[derive(Default)]
struct Run {
    /// Host-observed latency of fast results (ms), per edit (the slowest paragraph of the edit).
    fast_ms: Vec<f64>,
    /// The engine's own time for those results (ms).
    engine_ms: Vec<f64>,
    routed: BTreeMap<String, usize>,
    /// Fast-routed edits whose result did not arrive within FAST_TIMEOUT.
    fast_missing: usize,
    /// Fast-routed edits the session then sent to the background (a probe that failed, a unit
    /// over budget): BackgroundScheduled instead of a result.
    fast_demoted: usize,
    settle_ms: Vec<f64>,
    verdicts: BTreeMap<&'static str, usize>,
    wrong: usize,
    open: Value,
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((s.len() - 1) as f64 * p).round() as usize]
}

fn stats(v: &[f64]) -> Value {
    json!({"n": v.len(), "p50": pct(v, 0.5), "p95": pct(v, 0.95), "max": pct(v, 1.0)})
}

impl Run {
    fn summary(&self) -> Value {
        json!({
            "open": self.open,
            "edits_by_route": self.routed,
            "fast_result_ms": stats(&self.fast_ms),
            "fast_engine_ms": stats(&self.engine_ms),
            "fast_missing": self.fast_missing,
            "fast_demoted": self.fast_demoted,
            "settle_ms": stats(&self.settle_ms),
            "verdicts": self.verdicts,
        })
    }
    fn print_summary(&self) {
        let f = |name: &str, v: &[f64]| {
            println!(
                "  {name:28} n={:<4} p50 {:8.2}  p95 {:8.2}  max {:8.2}",
                v.len(),
                pct(v, 0.5),
                pct(v, 0.95),
                pct(v, 1.0)
            )
        };
        println!("replay summary");
        println!("  open: {}", self.open);
        println!(
            "  edits by route: {:?}  (fast, then sent to the background: {}; fast results missing: {})",
            self.routed, self.fast_demoted, self.fast_missing
        );
        f("fast result, host (ms)", &self.fast_ms);
        f("fast result, engine (ms)", &self.engine_ms);
        f("edit -> settled layout (ms)", &self.settle_ms);
        println!("  verdicts: {:?}", self.verdicts);
    }

    /// Open a session on `project` and wait for its first run to end.
    fn open(&mut self, project: &Path, main: &str, build: &Path) -> Result<(Session, Observer)> {
        let t0 = Instant::now();
        let mut cfg = SessionConfig::new(project, main.to_string());
        cfg.build_dir = build.to_path_buf();
        // experiment switches: REPLAY_NO_PICCACHE, REPLAY_COLD_BACKGROUND, REPLAY_BUDGET_MS
        if std::env::var("REPLAY_NO_PICCACHE").is_ok() {
            cfg.picture_cache = false;
        }
        if std::env::var("REPLAY_COLD_BACKGROUND").is_ok() {
            cfg.warm_background = false;
        }
        if let Some(ms) = std::env::var("REPLAY_BUDGET_MS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            cfg.fast_budget = Duration::from_millis(ms);
        }
        let s = Session::open(cfg)?;
        let mut o = Observer::new();
        let opened_ms = t0.elapsed().as_secs_f64() * 1e3;
        if !o.pump(&s, OPEN_TIMEOUT, |o| o.layouts > 0) {
            bail!("{}", o.dump(&s, "no first layout"));
        }
        let first_ms = t0.elapsed().as_secs_f64() * 1e3;
        if !o.pump(&s, OPEN_TIMEOUT, |o| o.ended) {
            bail!("{}", o.dump(&s, "the first run did not end"));
        }
        self.open = json!({
            "session_open_ms": opened_ms, "first_layout_ms": first_ms,
            "first_run_ended_ms": t0.elapsed().as_secs_f64() * 1e3,
            "converged": o.converged, "pages": o.pages_total, "compile": o.compile,
            "eligible_units": o.eligible.len(),
        });
        Ok((s, o))
    }

    /// Apply one edit and wait for its fast result (when routed fast).
    fn apply(
        &mut self,
        s: &Session,
        o: &mut Observer,
        label: &str,
        file: &str,
        edit: Edit,
    ) -> Result<(Applied, Value)> {
        let sent = Instant::now();
        let r = s.apply_edit(file, edit)?;
        let apply_us = sent.elapsed().as_micros();
        *self.routed.entry(r.routed.clone()).or_default() += 1;
        let a = Applied {
            label: label.to_string(),
            file: file.to_string(),
            edit_id: r.edit_id,
            rev: r.source_revision,
            sent,
            routed: r.routed.clone(),
            reasons: r.reasons.clone(),
            apply_us,
            touched: r
                .outcome
                .touched
                .iter()
                .chain(r.outcome.added.iter())
                .copied()
                .collect(),
        };
        let mut rec = json!({
            "label": a.label, "file": a.file, "edit_id": a.edit_id, "routed": a.routed,
            "reasons": a.reasons, "apply_us": a.apply_us, "host_us": r.host_us,
        });
        if a.routed == "fast" {
            let eid = a.edit_id;
            let got = o.pump(s, FAST_TIMEOUT, |o| {
                o.updates.keys().any(|(e, _)| *e == eid) || o.background.contains_key(&eid)
            });
            let got = got && o.updates.keys().any(|(e, _)| *e == eid);
            let demoted = o.background.contains_key(&eid);
            if got {
                // all paragraphs of the edit answer together; take what arrived for this edit
                for e in s.poll(Duration::from_millis(0)) {
                    o.absorb(e);
                }
                let mine: Vec<_> = o
                    .updates
                    .iter()
                    .filter(|((e, _), _)| *e == eid)
                    .map(|(_, v)| v)
                    .collect();
                let host = mine
                    .iter()
                    .map(|v| (v.at - sent).as_secs_f64() * 1e3)
                    .fold(0.0, f64::max);
                let engine = mine
                    .iter()
                    .map(|v| v.timing.total_us as f64 / 1e3)
                    .fold(0.0, f64::max);
                let statuses: Vec<&str> = mine.iter().map(|v| v.status.as_str()).collect();
                self.fast_ms.push(host);
                self.engine_ms.push(engine);
                rec["fast_ms"] = json!(host);
                rec["engine_ms"] = json!(engine);
                rec["statuses"] = json!(statuses);
            } else if demoted {
                self.fast_demoted += 1;
                rec["fast_ms"] = Value::Null;
                rec["demoted"] = json!(true);
            } else {
                self.fast_missing += 1;
                rec["fast_ms"] = Value::Null;
            }
        }
        Ok((a, rec))
    }

    /// Serve paragraph `p` of `file` again without changing the text (insert a space at its end,
    /// then delete it) and judge the second result against the current layout.
    fn reserve(
        &mut self,
        s: &Session,
        o: &mut Observer,
        file: &str,
        p: ParaId,
    ) -> Result<Option<(rtex_core::replay::Judgement, Value)>> {
        let text = s.document_text(file).unwrap_or_default();
        let Some(sp) = s.spans(file).into_iter().find(|sp| sp.id == p) else {
            return Ok(None);
        };
        let at = sp.range.start + text[sp.range.clone()].trim_end().len();
        let ins = s.apply_edit(
            file,
            Edit {
                start_byte: at,
                end_byte: at,
                text: " ".into(),
            },
        )?;
        let del = s.apply_edit(
            file,
            Edit {
                start_byte: at,
                end_byte: at + 1,
                text: String::new(),
            },
        )?;
        let rec = json!({"routed": [ins.routed, del.routed.clone()]});
        if del.routed != "fast" {
            return Ok(None);
        }
        let eid = del.edit_id;
        if !o.pump(s, FAST_TIMEOUT, |o| o.updates.contains_key(&(eid, p))) {
            return Ok(None);
        }
        let served = o.updates[&(eid, p)].clone();
        Ok(Some((o.judge(&served), rec)))
    }

    /// Request a clean pass covering `edits`, then judge the last result served per paragraph.
    fn settle(&mut self, s: &Session, o: &mut Observer, edits: &[Applied]) -> Result<Value> {
        let rev = edits
            .iter()
            .map(|a| a.rev)
            .max()
            .unwrap_or_else(|| s.versions().source_revision);
        let last_sent = edits
            .iter()
            .map(|a| a.sent)
            .max()
            .unwrap_or_else(Instant::now);
        if !o.settle(s, rev, SETTLE_TIMEOUT) {
            bail!(
                "{}",
                o.dump(s, &format!("no settled layout for revision {rev}"))
            );
        }
        let settle_ms = (o.layout_at.unwrap() - last_sent).as_secs_f64() * 1e3;
        if std::env::var("REPLAY_EVENTS").is_ok() {
            for l in o.log.drain(..) {
                println!("      | {l}");
            }
        }
        self.settle_ms.push(settle_ms);
        // per paragraph, the last edit that touched it: only a result served for that edit shows
        // the paragraph's final text (a later keystroke may have gone to the background path)
        let ids: Vec<u64> = edits.iter().map(|a| a.edit_id).collect();
        let mut last: BTreeMap<ParaId, &Applied> = BTreeMap::new();
        for a in edits {
            for p in &a.touched {
                last.insert(*p, a);
            }
        }
        for (e, p) in o.updates.keys() {
            if let Some(a) = edits.iter().find(|a| a.edit_id == *e) {
                if !last.contains_key(p) {
                    last.insert(*p, a);
                }
            }
        }
        let mut verdicts = vec![];
        for (p, a) in &last {
            let Some(served) = o.updates.get(&(a.edit_id, *p)).cloned() else {
                let v = if a.routed == "fast" {
                    "no-update"
                } else {
                    "last-edit-not-fast"
                };
                *self.verdicts.entry(v).or_default() += 1;
                verdicts.push(json!({"par": format!("{p:?}"), "edit_id": a.edit_id, "verdict": v}));
                continue;
            };
            let j = o.judge(&served);
            let mut name = j.verdict.name();
            let mut details = j.details;
            let mut reserve = Value::Null;
            if j.verdict == Verdict::Mismatch {
                // served with what the fast path knew then (labels, counters of the previous pass);
                // serve the same text again now: equal to the pass means the first result was
                // provisional, not wrong
                match self.reserve(s, o, &a.file, *p)? {
                    Some((again, r)) if again.verdict == Verdict::Match => {
                        name = "provisional";
                        reserve = r;
                    }
                    Some((again, r)) => {
                        details.push(format!(
                            "served again with the current context: {}",
                            again.verdict.name()
                        ));
                        details.extend(again.details);
                        reserve = r;
                    }
                    None => details.push("could not serve the paragraph again".into()),
                }
            }
            *self.verdicts.entry(name).or_default() += 1;
            let excerpt = s
                .spans(&a.file)
                .into_iter()
                .find(|sp| sp.id == *p)
                .and_then(|sp| {
                    s.document_text(&a.file)
                        .map(|t| t[sp.range].chars().take(400).collect::<String>())
                });
            if name == "MISMATCH" {
                self.wrong += 1;
                println!(
                    "  MISMATCH par {p:?} after edit '{}' in {}:",
                    a.label, a.file
                );
                println!("      source: {:?}", excerpt.as_deref().unwrap_or(""));
                for d in &details {
                    println!("      {d}");
                }
            }
            verdicts.push(json!({"par": format!("{p:?}"), "edit_id": a.edit_id, "verdict": name, "status": served.status,
                                 "reasons": served.reasons, "details": details, "reserve": reserve,
                                 "source": if name == "match" { None } else { excerpt }}));
        }
        o.updates.retain(|(e, _), _| !ids.contains(e));
        let errors: Vec<Value> = o
            .diagnostics
            .values()
            .flatten()
            .filter(|d| d.severity == "error")
            .take(5)
            .map(|d| json!({"location": d.file.as_ref().map(|f| format!("{f}:{}", d.line.unwrap_or(0))), "message": d.message}))
            .collect();
        Ok(json!({
            "settle_ms": settle_ms, "converged": o.converged, "pages": o.pages_total,
            "status": status_name(o.compile.as_ref()), "errors": errors, "verdicts": verdicts,
        }))
    }
}

fn status_name(c: Option<&CompileStatus>) -> &'static str {
    match c {
        Some(CompileStatus::Ok) => "ok",
        Some(CompileStatus::CompiledWithErrors { .. }) => "error",
        Some(CompileStatus::Failed) => "fatal",
        None => "none",
    }
}

fn sha256_hex(p: &Path) -> Option<String> {
    std::fs::read(p)
        .ok()
        .map(|b| hex::encode(Sha256::digest(&b)))
}

fn write_file(p: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, bytes).with_context(|| format!("writing {}", p.display()))
}

fn pos(v: &Value) -> Result<(usize, usize)> {
    Ok((
        v[0].as_u64().ok_or_else(|| anyhow!("bad position"))? as usize,
        v[1].as_u64().ok_or_else(|| anyhow!("bad position"))? as usize,
    ))
}

fn replay_ops(
    run: &mut Run,
    ops: &Path,
    root: &Path,
    main: &str,
    cases: Option<PathBuf>,
    out: &Path,
) -> Result<Value> {
    let name = ops
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("script")
        .to_string();
    let steps: Vec<Value> = std::fs::read_to_string(ops)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    // the reference engine's expectation per step, when the suite manifest is given
    let expect: Vec<Value> = match cases {
        Some(c) => {
            let m: Value = serde_json::from_str(&std::fs::read_to_string(c)?)?;
            m["cases"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|k| k["id"] == name.as_str())
                .map(|k| {
                    k["steps"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| json!({"engine": k["engine"], "expect": s["expect"]}))
                        .collect()
                })
                .unwrap_or_default()
        }
        None => vec![],
    };
    let project = out.join("project");
    std::fs::create_dir_all(&project)?;
    let mut records = vec![];
    let mut session: Option<(Session, Observer)> = None;
    for (k, step) in steps.iter().enumerate() {
        let label = step["label"].as_str().unwrap_or("").to_string();
        let mut applied = vec![];
        let mut edit_recs = vec![];
        for op in step["ops"].as_array().cloned().unwrap_or_default() {
            let file = op["file"]
                .as_str()
                .ok_or_else(|| anyhow!("op without file"))?
                .to_string();
            let disk = project.join(&file);
            if let Some(t) = op.get("create") {
                write_file(&disk, t.as_str().unwrap_or("").as_bytes())?;
            } else if op.get("delete_file").is_some() {
                std::fs::remove_file(&disk)?;
            } else if let Some(src) = op.get("binary_from") {
                write_file(
                    &disk,
                    &std::fs::read(root.join(src.as_str().unwrap_or("")))?,
                )?;
            } else {
                let (start, end) = (pos(&op["start"])?, pos(&op["end"])?);
                let new = op["text"].as_str().unwrap_or("");
                let tracked = session.as_ref().and_then(|(s, _)| s.document_text(&file));
                match (tracked, session.as_mut()) {
                    (Some(text), Some((s, o))) => {
                        let e = edit_at(&text, start, end, new)
                            .ok_or_else(|| anyhow!("step {k}: range out of bounds in {file}"))?;
                        let (a, rec) = run.apply(s, o, &label, &file, e)?;
                        write_file(&disk, s.document_text(&file).unwrap_or_default().as_bytes())?;
                        applied.push(a);
                        edit_recs.push(rec);
                    }
                    _ => {
                        // a file the session does not track (a .bib, a file not yet \input): disk only
                        let text = std::fs::read_to_string(&disk)?;
                        let e = edit_at(&text, start, end, new)
                            .ok_or_else(|| anyhow!("step {k}: range out of bounds in {file}"))?;
                        let mut t = text;
                        t.replace_range(e.start_byte..e.end_byte, &e.text);
                        write_file(&disk, t.as_bytes())?;
                        edit_recs.push(json!({"label": label, "file": file, "routed": "disk"}));
                    }
                }
            }
        }
        let files_ok = step["files"]
            .as_object()
            .map(|m| {
                m.iter()
                    .all(|(f, h)| sha256_hex(&project.join(f)).as_deref() == h.as_str())
            })
            .unwrap_or(true);
        let mut rec =
            json!({"step": k, "label": label, "files_match_script": files_ok, "edits": edit_recs});
        if let Some(e) = expect.get(k) {
            rec["reference"] = e.clone();
        }
        if let Some((s, o)) = session.as_mut() {
            if applied.is_empty() {
                // file operations or a rebuild with no edit: still ask for a pass
                let rev = s.versions().source_revision;
                let t = Instant::now();
                if !o.settle(s, rev, SETTLE_TIMEOUT) {
                    bail!("{}", o.dump(s, "no settled layout"));
                }
                rec["settled"] = json!({"settle_ms": t.elapsed().as_secs_f64() * 1e3, "status": status_name(o.compile.as_ref()), "pages": o.pages_total, "converged": o.converged});
            } else {
                rec["settled"] = run.settle(s, o, &applied)?;
            }
        } else {
            session = Some(run.open(&project, main, &out.join("build"))?);
            let (_, o) = session.as_ref().unwrap();
            rec["settled"] = json!({"status": status_name(o.compile.as_ref()), "pages": o.pages_total, "converged": o.converged});
        }
        println!(
            "[{name} {k}] {label:40} {}  files {}  ref {}",
            rec["settled"]
                .to_string()
                .chars()
                .take(160)
                .collect::<String>(),
            if files_ok { "ok" } else { "DIFFER" },
            rec.get("reference")
                .map(|r| r["expect"]["status"].to_string())
                .unwrap_or_default()
        );
        records.push(rec);
    }
    if let Some((s, _)) = session {
        s.close();
    }
    Ok(json!({"mode": "ops", "script": name, "summary": run.summary(), "steps": records}))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let p = e.path();
        let t = to.join(e.file_name());
        let ft = e.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&p)?;
            let _ = std::os::unix::fs::symlink(target, &t);
        } else if ft.is_dir() {
            if e.file_name() != "build" && e.file_name() != ".git" {
                copy_dir(&p, &t)?;
            }
        } else {
            std::fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

fn type_words(
    run: &mut Run,
    src: &Path,
    main: &str,
    opt: &HashMap<String, String>,
    out: &Path,
) -> Result<Value> {
    let n_units: usize = opt
        .get("units")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(12);
    let word = opt.get("word").cloned().unwrap_or_else(|| "quick".into());
    let cadence = Duration::from_millis(
        opt.get("cadence-ms")
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(120),
    );
    let mut rng = Rng::new(opt.get("seed").map(|v| v.parse()).transpose()?.unwrap_or(1));
    let settle_each = opt.contains_key("settle-each");
    let project = out.join("project");
    copy_dir(src, &project)?;
    let (s, mut o) = run.open(&project, main, &out.join("build"))?;
    // candidate paragraphs per file: body paragraphs with at least three safe words
    let files: Vec<String> = match opt.get("files") {
        Some(f) => f.split(',').map(String::from).collect(),
        None => {
            let mut v = vec![main.to_string()];
            v.extend(
                s.layout_units()
                    .into_iter()
                    .filter_map(|u| u.file)
                    .map(|f| f.trim_start_matches("./").to_string()),
            );
            v.sort();
            v.dedup();
            v.into_iter()
                .filter(|f| s.document_text(f).is_some())
                .collect()
        }
    };
    let mut cands: Vec<(String, ParaId)> = vec![];
    for f in &files {
        let text = s.document_text(f).unwrap_or_default();
        let mut mine: Vec<(String, ParaId)> = s
            .spans(f)
            .into_iter()
            .filter(|sp| o.kinds.get(&sp.id).map(|k| k == "par").unwrap_or(false))
            .filter(|sp| {
                let par = &text[sp.range.clone()];
                !has_picture_code(par) && safe_words(par).len() >= 3
            })
            .map(|sp| (f.clone(), sp.id))
            .collect();
        // a few per file, spread over the file (enough for `units` when there are few files)
        let per_file = n_units.div_ceil(files.len().max(1)).max(3);
        while mine.len() > per_file {
            mine.remove(rng.below(mine.len()));
        }
        cands.extend(mine);
    }
    while cands.len() > n_units {
        cands.remove(rng.below(cands.len()));
    }
    let mut words = vec![];
    let mut pending: Vec<Applied> = vec![];
    for (file, id) in &cands {
        let text = s.document_text(file).unwrap_or_default();
        let Some(sp) = s.spans(file).into_iter().find(|sp| sp.id == *id) else {
            continue;
        };
        let par = &text[sp.range.clone()];
        let ws = safe_words(par);
        let (_, end) = ws[rng.below(ws.len())];
        let mut at = sp.range.start + end;
        let mut keys = vec![];
        for ch in format!(" {word}").chars() {
            let t = Instant::now();
            let label = format!("{file}: type {ch:?}");
            let (a, rec) = run.apply(
                &s,
                &mut o,
                &label,
                file,
                Edit {
                    start_byte: at,
                    end_byte: at,
                    text: ch.to_string(),
                },
            )?;
            at += ch.len_utf8();
            pending.push(a);
            keys.push(rec);
            let spent = t.elapsed();
            if spent < cadence {
                // keep absorbing events (background passes run meanwhile)
                o.pump(&s, cadence - spent, |_| false);
            }
        }
        let mut rec = json!({"file": file, "par": format!("{id:?}"), "keys": keys});
        if settle_each {
            rec["settled"] = run.settle(&s, &mut o, &pending)?;
            pending.clear();
        }
        println!(
            "[type] {file:40} {} keys{}",
            rec["keys"].as_array().map(|k| k.len()).unwrap_or(0),
            rec.get("settled")
                .map(|v| format!("  settled {}", v["settle_ms"]))
                .unwrap_or_default()
        );
        words.push(rec);
    }
    let final_settle = if pending.is_empty() {
        Value::Null
    } else {
        run.settle(&s, &mut o, &pending)?
    };
    s.close();
    Ok(
        json!({"mode": "type", "project": src, "main": main, "word": word, "cadence_ms": cadence.as_millis() as u64,
              "settle_each": settle_each, "summary": run.summary(), "words": words, "final_settle": final_settle}),
    )
}

fn check(src: &Path, main: &str, opt: &HashMap<String, String>, out: &Path) -> Result<Value> {
    let timeout = Duration::from_secs(
        opt.get("timeout")
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(120),
    );
    let project = out.join("project");
    copy_dir(src, &project)?;
    let t0 = Instant::now();
    let mut cfg = SessionConfig::new(&project, main.to_string());
    // `--build DIR` keeps the build directory across runs (a reopened session); OUT is cleared
    cfg.build_dir = opt
        .get("build")
        .map(PathBuf::from)
        .unwrap_or_else(|| out.join("build"));
    let s = match Session::open(cfg) {
        Ok(s) => s,
        // a project rtex cannot open (a source file that is not UTF-8) is a result too
        Err(e) => {
            let report = json!({"status": "open-failed", "errors": [{"file": null, "line": null, "message": format!("{e:#}")}],
                                "wall_ms": t0.elapsed().as_millis() as u64});
            println!("{report}");
            return Ok(report);
        }
    };
    let mut o = Observer::new();
    // the first layout (a reopened build directory's saved one comes before any pass)
    o.pump(&s, timeout, |o| o.layouts > 0);
    let first_layout_ms = (o.layouts > 0).then(|| t0.elapsed().as_millis() as u64);
    let ended = o.pump(&s, timeout.saturating_sub(t0.elapsed()), |o| o.ended);
    let errors: Vec<Value> = o
        .diagnostics
        .values()
        .flatten()
        .filter(|d| d.severity == "error")
        .map(|d| json!({"file": d.file, "line": d.line, "message": d.message}))
        .collect();
    let report = json!({
        "status": if ended { status_name(o.compile.as_ref()) } else { "timeout" },
        "converged": o.converged, "pages": o.pages_total, "layouts": o.layouts,
        "wall_ms": t0.elapsed().as_millis() as u64, "first_layout_ms": first_layout_ms, "errors": errors,
        "engine": s.versions().engine_generation,
    });
    println!("{report}");
    s.close();
    Ok(report)
}
