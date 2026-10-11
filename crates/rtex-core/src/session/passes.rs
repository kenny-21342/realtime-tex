//! Background passes: the scheduler thread, standby engines, per-pass directories, the pass
//! loop with the picture cache, and delivering a pass as a layout. PDF export.

use super::*;

// ---------------------------------------------------------------------------------------------
// Background thread: debounced full passes with capture, layout installation, exports.
// ---------------------------------------------------------------------------------------------
pub(super) fn background_thread(s: Arc<Shared>) {
    warm_start(&s);
    prepare_standby(&s);
    while let Ok(cmd) = s.bg_signal.1.recv() {
        match cmd {
            BgCmd::Quit => break,
            BgCmd::Export(job, out) => run_export(&s, job, out),
            BgCmd::Pass => {
                if s.bg_paused.load(Ordering::SeqCst) {
                    s.bg_pending_while_paused.store(true, Ordering::SeqCst);
                    continue;
                }
                // the first pass runs at once; later ones are debounced (drain Pass commands until quiet)
                let debounce = if s.layout.lock().layout_version == 0 {
                    Duration::from_millis(1)
                } else {
                    s.cfg.debounce
                };
                loop {
                    match s.bg_signal.1.recv_timeout(debounce) {
                        Ok(BgCmd::Pass) => continue,
                        Ok(BgCmd::Quit) => return,
                        Ok(BgCmd::Export(job, out)) => {
                            run_export(&s, job, out);
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                if s.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                run_background_pass(&s);
                prepare_standby(&s);
            }
        }
    }
    if let Some(w) = s.standby.lock().take() {
        w.kill();
    }
}

pub(super) fn standby_dir(s: &Shared, n: usize) -> PathBuf {
    s.cfg.build_dir.join(format!("src-body-{n}"))
}

/// Output directory of the pass that runs from `src-body-{n}`. Two alternate: a standby
/// (lualatex up to the end of the preamble) opens its log, and with some preambles its PDF,
/// as soon as it starts, so it must never share a directory with the pass that is running.
pub(super) fn pass_dir(s: &Shared, n: usize) -> PathBuf {
    s.cfg.build_dir.join("bg").join(format!("pass-{n}"))
}

/// The errors of a pass that stopped before writing its capture: the newest `<job>.log` in the
/// pass directories written since `since` that reports an error (a standby opens its own log in
/// the other directory as soon as it starts). File names as in `deliver_layout`.
fn failed_pass_diagnostics(s: &Shared, since: std::time::SystemTime) -> Vec<Diagnostic> {
    let jobname = jobname_of(&s.cfg.main_file);
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> =
        [pass_dir(s, 0), pass_dir(s, 1), s.cfg.build_dir.join("bg")]
            .iter()
            .filter_map(|d| {
                let p = d.join(format!("{jobname}.log"));
                let m = std::fs::metadata(&p).ok()?.modified().ok()?;
                (m >= since).then_some((m, p))
            })
            .collect();
    logs.sort_by_key(|l| std::cmp::Reverse(l.0));
    for (_, log) in logs {
        let mut items = parse_log(&log);
        if items.iter().any(|d| d.severity == "error") {
            locate_standby_diagnostics(s, &mut items);
            return items;
        }
    }
    Vec::new()
}

/// The directory of the last finished pass (seeded from disk on the first call: the newer
/// of the two pass directories, or the pre-pass-directory layout `build/bg` itself).
pub(super) fn last_pass_dir(s: &Shared) -> Option<PathBuf> {
    let mut slot = s.last_pass_dir.lock();
    if slot.is_none() {
        let jobname = jobname_of(&s.cfg.main_file);
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for d in [pass_dir(s, 0), pass_dir(s, 1), s.cfg.build_dir.join("bg")] {
            if let Ok(m) = std::fs::metadata(d.join(format!("{jobname}.aux"))) {
                if let Ok(t) = m.modified() {
                    if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                        best = Some((t, d));
                    }
                }
            }
        }
        *slot = best.map(|(_, d)| d);
    }
    slot.clone()
}

/// The slot (0 or 1) a new standby takes: the one that does not hold the last pass.
/// A standby engine for `slot`: the session's project and sources, the slot's body snapshot
/// and pass directories.
pub(super) fn spawn_standby(
    s: &Shared,
    texts: &BTreeMap<String, String>,
    unit_envs: &str,
    slot: usize,
) -> Result<WarmEngine> {
    WarmEngine::spawn(crate::background::StandbySpec {
        tl: &s.tl,
        project: &s.cfg.project_root,
        files: texts,
        main: &s.cfg.main_file,
        src_dir: &standby_dir(s, slot),
        out_dir: &pass_dir(s, slot),
        instrumented: true,
        unit_envs,
    })
}

pub(super) fn standby_slot(s: &Shared) -> usize {
    match last_pass_dir(s) {
        Some(d) if d.ends_with("pass-0") => 1,
        _ => 0,
    }
}

pub(super) fn jobname_of(main: &str) -> String {
    Path::new(main)
        .file_stem()
        .and_then(|x| x.to_str())
        .unwrap_or("main")
        .to_string()
}

/// Start a standby engine for the next background pass (while the user types, it loads the
/// current preamble). Replaces a standby whose preamble is outdated; keeps a matching one.
pub(super) fn prepare_standby(s: &Shared) {
    if !s.cfg.warm_background || s.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let (texts, _, _) = snapshot(s);
    let Some(pre_hash) = standby_preamble_hash(&texts, &s.cfg.main_file) else {
        return;
    };
    let mut slot = s.standby.lock();
    if let Some(w) = slot.as_mut() {
        if w.preamble_hash == pre_hash && w.is_alive() {
            return;
        }
    }
    if let Some(w) = slot.take() {
        w.kill();
    }
    let unit_envs = s.policy.lock().unit_envs_env();
    let n = standby_slot(s);
    match spawn_standby(s, &texts, &unit_envs, n) {
        Ok(w) => *slot = Some(w),
        Err(e) => {
            s.events
                .send(Event::Diagnostics {
                    source: "background".into(),
                    items: vec![Diagnostic {
                        severity: "warning".into(),
                        file: None,
                        line: None,
                        message: format!("standby engine: {e}"),
                        context: None,
                    }],
                })
                .ok();
        }
    }
}

pub(super) fn texts_of(files: &BTreeMap<String, FileBuf>) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(n, fb)| (n.clone(), fb.text.clone()))
        .collect()
}

/// The preamble the fast server loads: the main file's preamble with `\input`ted files inlined
/// from the buffers, plus the document's body setup statements (`crate::server_preamble`).
pub(super) fn effective_preamble(texts: &BTreeMap<String, String>, main: &str) -> String {
    crate::server_preamble(texts, main)
}

/// The preamble as the standby engine hashes it (`write_body_snapshot`): inputs inlined, no
/// body setup (the standby compiles the whole body itself).
pub(super) fn standby_preamble_hash(texts: &BTreeMap<String, String>, main: &str) -> Option<u64> {
    texts
        .get(main)
        .and_then(|t| crate::split_preamble(t))
        .map(|(p, _)| crate::document::hash_str(&crate::document::expand_inputs(p, texts)))
}

pub(super) fn preamble_input_set(
    texts: &BTreeMap<String, String>,
    main: &str,
) -> std::collections::BTreeSet<String> {
    texts
        .get(main)
        .and_then(|t| crate::split_preamble(t))
        .map(|(p, _)| crate::document::transitive_inputs(p, texts))
        .unwrap_or_default()
}

pub(super) fn snapshot(s: &Shared) -> (BTreeMap<String, String>, Vec<SnapshotSpan>, Revision) {
    let files = s.files.lock();
    let rev = s.source_revision.load(Ordering::SeqCst);
    let policy = s.policy.lock();
    let inputted = s.inputted.lock();
    let mut texts = BTreeMap::new();
    let mut spans = Vec::new();
    for (name, fb) in files.iter() {
        texts.insert(name.clone(), fb.text.clone());
        spans.extend(crate::layout::snapshot_spans_of(
            fb,
            name,
            &policy,
            inputted.contains(name),
        ));
    }
    (texts, spans, rev)
}

pub(super) fn run_background_pass(s: &Shared) {
    s.bg_running.store(true, Ordering::SeqCst);
    run_background_pass_inner(s);
    s.bg_running.store(false, Ordering::SeqCst);
}

/// A pass of a multi-pass run that took long enough to be worth showing before the run is
/// stable (TikZ-heavy documents: 45 s per pass, three passes to converge).
pub(super) const PROVISIONAL_LAYOUT_AFTER: Duration = Duration::from_secs(2);

pub(super) fn run_background_pass_inner(s: &Shared) {
    let (texts, spans, rev) = snapshot(s);
    run_pass(s, &texts, spans, rev, &[], &[]);
}

/// A run of passes over a snapshot, published as a layout. With `held` pictures (a held run,
/// `hold`): their bodies are blanked and the manifest offers their last good drawings; the run
/// publishes only a clean layout that took every held picture from the cache, and returns
/// whether it did (otherwise the caller publishes the run it is holding for). A normal run
/// always publishes and returns true.
fn run_pass(
    s: &Shared,
    source_texts: &BTreeMap<String, String>,
    spans: Vec<SnapshotSpan>,
    rev: Revision,
    held: &[hold::HeldPic],
    held_diagnostics: &[Diagnostic],
) -> bool {
    let t0 = Instant::now();
    let holding = !held.is_empty();
    let texts = if holding {
        hold::blank(source_texts, held)
    } else {
        source_texts.clone()
    };
    // pictures are held in warm runs only: they alternate between two pass directories, so the
    // run being held for keeps its files while the held run writes the other directory
    let hold_on =
        !holding && s.cfg.hold_broken_pictures && s.cfg.picture_cache && s.cfg.warm_background;
    let snap_dir = snapshot_dir(&s.cfg.build_dir);
    // a pass that cannot run is still a layout result: hosts see compile = Failed instead of a
    // pass that never ends
    let started = std::time::SystemTime::now();
    let stop = |ran: Duration| -> Option<String> {
        if ran >= s.cfg.pass_timeout {
            return Some(format!(
                "LaTeX did not finish within {} s (an endless loop?); stopped",
                s.cfg.pass_timeout.as_secs()
            ));
        }
        let slowest = Duration::from_millis(s.slowest_pass_ms.load(Ordering::SeqCst));
        (!slowest.is_zero()
            && ran > (slowest * 2).max(Duration::from_secs(5))
            && s.source_revision.load(Ordering::SeqCst) != rev)
            .then(|| {
                format!(
                    "LaTeX did not finish within {} s, twice its longest run so far; stopped for the edited sources",
                    ran.as_secs()
                )
            })
    };
    let ran_to_end = |cap: &crate::capture::CaptureResult| {
        s.slowest_pass_ms
            .fetch_max(cap.wall.as_millis() as u64, Ordering::SeqCst);
    };
    let failed_with = |msg: String, items: Vec<Diagnostic>| {
        if holding {
            log::debug!("held run failed: {msg}");
            return;
        }
        s.events
            .send(Event::Diagnostics {
                source: "background".into(),
                items,
            })
            .ok();
        *s.convergence.lock() = Some(Convergence::PassLimitReached {
            passes: 0,
            reasons: vec![msg.clone()],
        });
        s.events
            .send(Event::LayoutUpdate {
                versions: Versions {
                    source_revision: rev,
                    ..Default::default()
                },
                compile: CompileStatus::Failed,
                convergence: Convergence::PassLimitReached {
                    passes: 0,
                    reasons: vec![msg],
                },
                passes: 0,
                pages_changed: vec![],
                pages_total: 0,
                placements: vec![],
                eligible_paragraphs: vec![],
                pdf_fallback: None,
                wall_ms: t0.elapsed().as_millis() as u64,
                held: vec![],
            })
            .ok();
    };
    let failed = |msg: String| {
        let item = Diagnostic {
            severity: "error".into(),
            file: None,
            line: None,
            message: msg.clone(),
            context: None,
        };
        failed_with(msg, vec![item])
    };
    if let Err(e) = write_snapshot(&s.cfg.project_root, &texts, &snap_dir) {
        failed(format!("snapshot: {e}"));
        return !holding;
    }
    let out_dir = s.cfg.build_dir.join("bg");
    let unit_envs = s.policy.lock().unit_envs_env();
    // picture cache: pictures whose source and surroundings are unchanged come from an earlier
    // pass's PDF; the manifest is refreshed before every pass of the run (a pass's own
    // drawings serve the next one)
    let _ = std::fs::create_dir_all(&out_dir);
    let mut pics: Vec<crate::piccache::PictureRef> = if s.cfg.picture_cache {
        let pre_hash = crate::piccache::preamble_hash(&texts, &s.cfg.main_file);
        crate::piccache::scan_pictures(&texts, &s.cfg.main_file, pre_hash)
    } else {
        Vec::new()
    };
    // a held picture's (blanked) body is replaced by its last good drawing
    for p in &mut pics {
        if let Some(h) = held.iter().find(|h| h.key == p.key) {
            p.hash = h.last_good.hash;
            p.cacheable = true;
        }
    }
    let pic_ids = hold::identities(&pics, &spans);
    let held_ids: std::collections::BTreeSet<hold::PicId> = held.iter().map(|h| h.id).collect();
    let errors_of = |cap: &crate::capture::CaptureResult| -> Vec<(String, i64)> {
        parse_log(&cap.log)
            .into_iter()
            .filter(|d| d.severity == "error")
            .filter_map(|d| Some((d.file?, d.line?)))
            .collect()
    };
    // the pictures a normal run's pass would hold
    let to_hold = |errors: &[(String, i64)]| -> Vec<hold::HeldPic> {
        if !hold_on || errors.is_empty() {
            return Vec::new();
        }
        hold::decide(&texts, &pics, &pic_ids, errors, &s.hold.lock().last_good)
    };
    let pic_cache =
        std::sync::Mutex::new(crate::piccache::PicCache::open(&out_dir.join("pic-cache")));
    // the drawings of the pictures the manifest offers: a pass that takes them from the cache
    // gets them back in its page lists (native drawing of cached pictures)
    let pic_fragments = std::sync::Mutex::new(BTreeMap::new());
    // written into the directory of the pass about to run (the capture reads it from there)
    let refresh_manifest = |dir: &Path| {
        let manifest = dir.join("pic-manifest.json");
        if pics.is_empty() {
            let _ = std::fs::remove_file(&manifest);
            *pic_fragments.lock().unwrap() = BTreeMap::new();
            return;
        }
        let mut cache = pic_cache.lock().unwrap();
        if let Err(e) = cache.write_manifest(&pics, &manifest) {
            log::warn!("picture cache manifest: {e:#}");
        }
        *pic_fragments.lock().unwrap() = cache.fragments(&pics);
    };
    let absorb = |cap: &crate::capture::CaptureResult| {
        // a pass that ended on a fatal error has no (complete) PDF to take pictures from
        if pics.is_empty() || cap.fatal() {
            return;
        }
        let mut errors = errors_of(cap);
        // a picture about to be held is neither cached in its broken version nor taken as its
        // last good one (a runaway's errors come after its \end)
        for h in to_hold(&errors) {
            errors.push((h.file.clone(), h.line as i64));
        }
        let mut cache = pic_cache.lock().unwrap();
        cache.pin(s.hold.lock().last_good.values().map(|g| g.hash));
        if let Err(e) = cache.absorb(
            &pics,
            &cap.json.recorded_pics(),
            &cap.json.pic_mismatch,
            &errors,
            &cap.pdf,
        ) {
            log::warn!("picture cache: {e:#}");
        }
        // last good versions: pictures drawn (or taken from the cache) with no error from their
        // \begin to just after their \end (a runaway reports the line after it)
        let mut hs = s.hold.lock();
        for (p, id) in pics.iter().zip(&pic_ids) {
            let Some(id) = id else { continue };
            let Some((file, begin)) = p
                .key
                .rsplit_once(':')
                .and_then(|(f, l)| Some((f, l.parse::<i64>().ok()?)))
            else {
                continue;
            };
            let near = errors.iter().any(|(f, l)| {
                f.trim_start_matches("./") == file && (begin..=p.end_line as i64 + 2).contains(l)
            });
            if held_ids.contains(id) || near || !p.cacheable || !cache.has_entry(p.hash) {
                continue;
            }
            if let Some(text) = hold::picture_text(&texts, p) {
                hs.last_good.insert(
                    *id,
                    hold::LastGood {
                        hash: p.hash,
                        env: p.env.clone(),
                        text,
                    },
                );
            }
        }
        // pictures no longer in the sources
        let live_ids: std::collections::BTreeSet<&hold::PicId> = pic_ids.iter().flatten().collect();
        hs.last_good.retain(|id, _| live_ids.contains(id));
    };
    // the aux family every pass of this run starts from
    let aux_dir = last_pass_dir(s).unwrap_or_else(|| pass_dir(s, 0));
    // before a pass runs in its directory: the previous pass's aux family and the manifest
    let prepare_dir = |dir: &Path| {
        // the directory's capture is about to change: it no longer matches its sources hash
        let _ = std::fs::remove_file(dir.join(SOURCES_HASH_FILE));
        if let Some(from) = last_pass_dir(s) {
            if let Err(e) = crate::background::copy_aux_family(&from, dir) {
                log::warn!("aux family: {e:#}");
            }
        }
        refresh_manifest(dir);
    };
    let finished = |cap: &crate::capture::CaptureResult| {
        *s.last_pass_dir.lock() = Some(cap.out_dir.clone());
    };
    let spans_for_provisional = spans.clone();
    let mut pass_started = Instant::now();
    let mut on_pass = |cap: &crate::capture::CaptureResult, pass: u32| {
        // the host sees the provisional layout before the cache absorbs the pass's pictures
        // (a PDF round trip)
        if pass_started.elapsed() >= PROVISIONAL_LAYOUT_AFTER
            && !holding
            && to_hold(&errors_of(cap)).is_empty()
        {
            s.hold.lock().shown_clean = None;
            deliver_layout(
                s,
                t0,
                cap,
                pass,
                false,
                spans_for_provisional.clone(),
                rev,
                true,
                None,
            );
        }
        let t_absorb = Instant::now();
        absorb(cap);
        log::debug!(
            "background pass {pass}: picture cache absorb {} ms",
            t_absorb.elapsed().as_millis()
        );
        pass_started = Instant::now();
    };
    let result = if s.cfg.warm_background {
        // Every pass of the loop runs in a standby engine: the one prepared while the user
        // typed, then the one started when the previous pass was released (its preamble loads
        // while the body is typeset). Two snapshot directories alternate so a loading standby
        // never rewrites the files a running one reads.
        // the standby's own hash (write_body_snapshot): the preamble with its \input files
        // inlined; the raw preamble text never matched one that \inputs files
        let pre_hash = standby_preamble_hash(&texts, &s.cfg.main_file);
        let mut runner =
            |_pass: u32| -> Result<crate::capture::CaptureResult> {
                let ready =
                    {
                        let mut slot = s.standby.lock();
                        match slot.take() {
                            Some(mut w) => {
                                if Some(w.preamble_hash) == pre_hash && w.is_alive() {
                                    Some(w)
                                } else {
                                    log::debug!(
                                "background pass: standby discarded (preamble {}, alive {})",
                                if Some(w.preamble_hash) == pre_hash { "same" } else { "changed" },
                                w.is_alive()
                            );
                                    w.kill();
                                    None
                                }
                            }
                            None => {
                                log::debug!("background pass: no standby");
                                None
                            }
                        }
                    };
                let warm = ready.is_some();
                if warm { &s.bg_warm } else { &s.bg_cold }.fetch_add(1, Ordering::SeqCst);
                let t_pass = Instant::now();
                let w = match ready {
                    Some(w) => w,
                    None => {
                        let slot = standby_slot(s);
                        spawn_standby(s, &texts, &unit_envs, slot)?
                    }
                };
                // this pass starts from the last one's aux family, then the next standby starts
                // in the other slot (whose previous results are consumed by now)
                prepare_dir(w.out_dir());
                let other = if w.src_dir.ends_with("src-body-0") {
                    1
                } else {
                    0
                };
                if let Ok(next) = spawn_standby(s, &texts, &unit_envs, other) {
                    *s.standby.lock() = Some(next);
                }
                let t_run = Instant::now();
                let mut cap = w.run_until(&s.cfg.project_root, &texts, &s.cfg.main_file, &stop)?;
                ran_to_end(&cap);
                log::debug!(
                    "background pass: {} standby, waited/spawned {} ms, engine run {} ms",
                    if warm { "warm" } else { "cold" },
                    (t_run - t_pass).as_millis(),
                    t_run.elapsed().as_millis()
                );
                cap.pic_fragments = pic_fragments.lock().unwrap().clone();
                finished(&cap);
                Ok(cap)
            };
        run_pass_with_runner(
            crate::background::PassPlan {
                tl: &s.tl,
                snapshot_dir: &standby_dir(s, 0),
                main: &s.cfg.main_file,
                aux_dir: &aux_dir,
                max_passes: s.cfg.max_passes,
                bib: s.cfg.bib_tool,
            },
            &mut runner,
            &mut on_pass,
        )
    } else {
        // plain passes: a fresh lualatex per pass in pass-0 (sequential, so one directory)
        let dir = pass_dir(s, 0);
        let mut runner = |_pass: u32| -> Result<crate::capture::CaptureResult> {
            prepare_dir(&dir);
            let mut cap = crate::capture::run_capture_until(
                &s.tl,
                &snap_dir,
                &s.cfg.main_file,
                &dir,
                true,
                &unit_envs,
                &stop,
            )?;
            ran_to_end(&cap);
            cap.pic_fragments = pic_fragments.lock().unwrap().clone();
            finished(&cap);
            Ok(cap)
        };
        run_pass_with_runner(
            crate::background::PassPlan {
                tl: &s.tl,
                snapshot_dir: &snap_dir,
                main: &s.cfg.main_file,
                aux_dir: &aux_dir,
                max_passes: s.cfg.max_passes,
                bib: s.cfg.bib_tool,
            },
            &mut runner,
            &mut on_pass,
        )
    };
    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            // LaTeX stopped before the end of the document (no capture): its log still says
            // where; the error list is the log's, as for any pass
            let mut items = failed_pass_diagnostics(s, started);
            if let Some(stopped) = e.downcast_ref::<crate::capture::Stopped>() {
                // stopped by pass_timeout: that is the reason, whatever the log says so far
                items.insert(
                    0,
                    Diagnostic {
                        severity: "error".into(),
                        file: None,
                        line: None,
                        message: stopped.0.clone(),
                        context: None,
                    },
                );
                failed_with(stopped.0.clone(), items);
                return !holding;
            }
            match items.iter().find(|d| d.severity == "error") {
                Some(first) => {
                    let at = match (&first.file, first.line) {
                        (Some(f), Some(l)) => format!(" ({f}:{l})"),
                        _ => String::new(),
                    };
                    let msg = format!("fatal error, LaTeX stopped: {}{at}", first.message);
                    failed_with(msg, items);
                }
                None => failed(format!("background pass: {e:#}")),
            }
            return !holding;
        }
    };
    let outcome_cap = outcome.capture;
    if holding {
        // the held run counts when it is clean and took every held picture from the cache (a
        // picture it drew is recorded; its blanked body would show nothing)
        let recorded = outcome_cap.json.recorded_pics();
        let took_all = held.iter().all(|h| {
            !recorded.contains_key(&h.key) && !outcome_cap.json.pic_mismatch.contains(&h.key)
        });
        let clean = !outcome_cap.fatal()
            && outcome_cap.json.pages > 0
            && !parse_log(&outcome_cap.log)
                .iter()
                .any(|d| d.severity == "error");
        if !(took_all && clean) {
            log::debug!(
                "held run not usable (took every held picture: {took_all}, clean: {clean})"
            );
            return false;
        }
        absorb(&outcome_cap);
        let pictures = held_pictures(s, held, rev);
        deliver_layout(
            s,
            t0,
            &outcome_cap,
            outcome.passes,
            outcome.aux_stable,
            hold::depicted_spans(spans, source_texts, held),
            rev,
            false,
            Some((pictures, held_diagnostics.to_vec())),
        );
        return true;
    }
    let holds = to_hold(&errors_of(&outcome_cap));
    absorb(&outcome_cap);
    if !holds.is_empty() {
        let mut diagnostics = parse_log(&outcome_cap.log);
        locate_standby_diagnostics(s, &mut diagnostics);
        // what the held layout depicts: the held pictures' last good texts in place
        let depicted = sources_hash(&hold::restore(&texts, &holds));
        if s.hold.lock().shown_clean == Some(depicted) {
            // only the held pictures changed since the pages shown: they stay
            let pictures = held_pictures(s, &holds, rev);
            publish_unchanged_layout(s, t0, outcome.passes, rev, diagnostics, pictures);
            return true;
        }
        if run_pass(s, &texts, spans.clone(), rev, &holds, &diagnostics) {
            s.hold.lock().shown_clean = Some(depicted);
            return true;
        }
        log::debug!("hold abandoned: the held run was not clean");
    }
    let status = deliver_layout(
        s,
        t0,
        &outcome_cap,
        outcome.passes,
        outcome.aux_stable,
        spans,
        rev,
        false,
        None,
    );
    {
        let mut hs = s.hold.lock();
        hs.since.clear();
        hs.shown_clean = (status == CompileStatus::Ok).then(|| sources_hash(&texts));
    }
    // the sources this capture was made from: a later session on the same build directory
    // shows it at once when they have not changed (warm_start)
    if outcome_cap.json.pages > 0 && !outcome_cap.fatal() {
        log::debug!(
            "sources hash of {} files written to {}",
            texts.len(),
            outcome_cap.out_dir.display()
        );
        let _ = std::fs::write(
            outcome_cap.out_dir.join(SOURCES_HASH_FILE),
            format!("{:016x}\n", sources_hash(&texts)),
        );
    }
    true
}

/// The held pictures as a layout reports them, with the revision each was first held at.
fn held_pictures(s: &Shared, held: &[hold::HeldPic], rev: Revision) -> Vec<HeldPicture> {
    let mut hs = s.hold.lock();
    hs.since.retain(|id, _| held.iter().any(|h| h.id == *id));
    held.iter()
        .map(|h| HeldPicture {
            par_id: h.id.0,
            file: h.file.clone(),
            line: h.line,
            end_line: h.end_line,
            since_revision: *hs.since.entry(h.id).or_insert(rev),
        })
        .collect()
}

/// Next to a finished run's capture: the hash of the sources it compiled (`sources_hash`).
const SOURCES_HASH_FILE: &str = "rtex-sources";

/// The texts a pass compiles, and the rtex version that captured them.
fn sources_hash(texts: &BTreeMap<String, String>) -> u64 {
    let mut all = String::from(env!("CARGO_PKG_VERSION"));
    for (name, text) in texts {
        all.push('\0');
        all.push_str(name);
        all.push('\0');
        all.push_str(text);
    }
    crate::document::hash_str(&all)
}

/// A session reopened on a build directory whose last finished run compiled the very sources
/// it has now shows that run's layout before its first pass, as a provisional layout (the pass
/// runs anyway and replaces it): no blank preview for the length of a pass, and live edits
/// right away. Files other than the tracked sources (figures, `.bib`) are not hashed; the
/// first pass brings their changes.
fn warm_start(s: &Shared) {
    let (texts, spans, rev) = snapshot(s);
    let h = format!("{:016x}", sources_hash(&texts));
    let jobname = jobname_of(&s.cfg.main_file);
    for dir in [pass_dir(s, 0), pass_dir(s, 1)] {
        let Ok(saved) = std::fs::read_to_string(dir.join(SOURCES_HASH_FILE)) else {
            continue;
        };
        if saved.trim() != h {
            log::debug!(
                "warm start: {} holds a run of other sources ({} files now)",
                dir.display(),
                texts.len()
            );
            continue;
        }
        let t0 = Instant::now();
        match crate::capture::load_capture(&dir, &jobname) {
            Ok(cap) if cap.json.pages > 0 && !cap.fatal() => {
                log::debug!(
                    "warm start: the previous session's layout from {}",
                    dir.display()
                );
                *s.last_pass_dir.lock() = Some(dir.clone());
                deliver_layout(s, t0, &cap, 1, true, spans, rev, true, None);
                return;
            }
            Ok(_) => {}
            Err(e) => log::debug!("warm start: {}: {e:#}", dir.display()),
        }
    }
}

pub(super) fn layout_failed(s: &Shared, t0: Instant, rev: Revision, msg: String) {
    s.events
        .send(Event::Diagnostics {
            source: "background".into(),
            items: vec![Diagnostic {
                severity: "error".into(),
                file: None,
                line: None,
                message: msg.clone(),
                context: None,
            }],
        })
        .ok();
    *s.convergence.lock() = Some(Convergence::PassLimitReached {
        passes: 0,
        reasons: vec![msg.clone()],
    });
    s.events
        .send(Event::LayoutUpdate {
            versions: Versions {
                source_revision: rev,
                ..Default::default()
            },
            compile: CompileStatus::Failed,
            convergence: Convergence::PassLimitReached {
                passes: 0,
                reasons: vec![msg],
            },
            passes: 0,
            pages_changed: vec![],
            pages_total: 0,
            placements: vec![],
            eligible_paragraphs: vec![],
            pdf_fallback: None,
            wall_ms: t0.elapsed().as_millis() as u64,
            held: vec![],
        })
        .ok();
}

/// Install a pass's capture as the current layout and tell the host. `provisional`: a pass of a
/// run that is not stable yet (another pass follows); the layout is usable, its convergence is
/// `Converging`.
#[allow(clippy::too_many_arguments)]
pub(super) fn deliver_layout(
    s: &Shared,
    t0: Instant,
    cap: &crate::capture::CaptureResult,
    passes: u32,
    aux_stable: bool,
    spans: Vec<SnapshotSpan>,
    rev: Revision,
    provisional: bool,
    held: Option<(Vec<HeldPicture>, Vec<Diagnostic>)>,
) -> CompileStatus {
    let t = Instant::now();
    let status = deliver_layout_inner(
        s,
        t0,
        cap,
        passes,
        aux_stable,
        spans,
        rev,
        provisional,
        held,
    );
    log::debug!(
        "background pass {passes}: layout delivered in {} ms ({} pages, provisional {provisional})",
        t.elapsed().as_millis(),
        cap.json.pages
    );
    status
}

#[allow(clippy::too_many_arguments)]
fn deliver_layout_inner(
    s: &Shared,
    t0: Instant,
    cap: &crate::capture::CaptureResult,
    passes: u32,
    aux_stable: bool,
    spans: Vec<SnapshotSpan>,
    rev: Revision,
    provisional: bool,
    held: Option<(Vec<HeldPicture>, Vec<Diagnostic>)>,
) -> CompileStatus {
    // a held layout reports the errors of the run it holds for (its own run is clean)
    let (held, mut diagnostics) = match held {
        Some((pictures, diagnostics)) => (pictures, diagnostics),
        None => (Vec::new(), parse_log(&cap.log)),
    };
    if held.is_empty() {
        locate_standby_diagnostics(s, &mut diagnostics);
    }
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count();
    // a fatal error leaves no PDF (at most a partial one) even when pages were shipped: the
    // pass is a failure, the previous layout stays
    let fatal = cap.fatal();
    let compile = if cap.json.pages == 0 || fatal {
        CompileStatus::Failed
    } else if errors > 0 {
        CompileStatus::CompiledWithErrors { count: errors }
    } else {
        CompileStatus::Ok
    };
    if compile == CompileStatus::Failed {
        s.events
            .send(Event::Diagnostics {
                source: "background".into(),
                items: diagnostics,
            })
            .ok();
        s.events
            .send(Event::LayoutUpdate {
                versions: Versions {
                    source_revision: rev,
                    ..Default::default()
                },
                compile: compile.clone(),
                convergence: Convergence::PassLimitReached {
                    passes,
                    reasons: vec![if fatal {
                        "fatal error: no PDF".into()
                    } else {
                        "no pages".into()
                    }],
                },
                passes,
                pages_changed: vec![],
                pages_total: 0,
                placements: vec![],
                eligible_paragraphs: vec![],
                pdf_fallback: None,
                wall_ms: t0.elapsed().as_millis() as u64,
                held: vec![],
            })
            .ok();
        return compile;
    }
    let (changed, versions, placements, eligible, eligible_strict, pdf) = {
        // lock order everywhere: files, then layout, then policy (apply_edit holds the first two
        // together)
        let files = s.files.lock();
        let mut layout = s.layout.lock();
        let changed = match layout.install(cap, spans, rev) {
            Ok(c) => {
                // the PDF hosts render degraded pages from: a copy per layout, since the next
                // pass (started right after this one, a provisional layout's in particular)
                // rewrites the pass PDF while the host reads it
                let v = layout.layout_version;
                let bg = s.cfg.build_dir.join("bg");
                let stable = bg.join(format!("layout-{v}.pdf"));
                match std::fs::copy(&cap.pdf, &stable) {
                    Ok(_) => {
                        layout.pdf = Some(stable.clone());
                        // the two previous copies stay: a host may still be reading the layout
                        // before the one it is replacing
                        if v >= 3 {
                            let _ = std::fs::remove_file(bg.join(format!("layout-{}.pdf", v - 3)));
                        }
                        // build/bg/<jobname>.pdf and .log: the latest layout, for hosts that
                        // name these files themselves (a link to the copy; nothing writes
                        // them in place)
                        let main_pdf = bg.join(format!("{}.pdf", cap.jobname));
                        let _ = std::fs::remove_file(&main_pdf);
                        if std::fs::hard_link(&stable, &main_pdf).is_err() {
                            let _ = std::fs::copy(&stable, &main_pdf);
                        }
                        let _ = std::fs::copy(&cap.log, bg.join(format!("{}.log", cap.jobname)));
                    }
                    Err(e) => log::warn!("layout PDF copy: {e}"),
                }
                c
            }
            Err(e) => {
                drop(layout);
                drop(files);
                layout_failed(s, t0, rev, format!("install layout: {e}"));
                return CompileStatus::Failed;
            }
        };
        let (versions, placements, eligible, eligible_strict) = layout_lists(s, &files, &layout);
        drop(files);
        (
            changed,
            versions,
            placements,
            eligible,
            eligible_strict,
            layout.pdf.clone(),
        )
    };
    // warm the engine: compile the first allow-listed paragraph once so fonts are loaded before
    // the first real keystroke (never an unprobed unit: it could leak state)
    if let Some(id) = eligible_strict.first().copied() {
        let files = s.files.lock();
        let layout = s.layout.lock();
        if let (Some(eu), Some(text)) = (
            layout.unit(id),
            files.values().find_map(|fb| fb.span_text(id)),
        ) {
            let mut p = s.pending.lock();
            if let std::collections::btree_map::Entry::Vacant(e) = p.entry(id) {
                // exercise the font variants and math a body paragraph commonly needs so their
                // font instances are loaded before the first real keystroke
                let warm = format!("{} \\emph{{warm}} \\textbf{{warm}} \\textit{{warm}} \\textsc{{warm}} {{\\small warm}} $x^2_i + \\alpha \\sum \\frac{{1}}{{2}} \\mathbf{{v}}$", text.trim_end_matches('\n'));
                e.insert(FastRequest {
                    warmup: true,
                    par_id: id,
                    edit_id: 0,
                    span_hash: 0,
                    source: warm,
                    pics: String::new(),
                    pics_n: 0,
                    pic_fragments: BTreeMap::new(),
                    seq: eu.uid,
                    ctx: None,
                    versions: Versions {
                        source_revision: rev,
                        context_revision: layout.context_revision,
                        engine_generation: s.engine_generation.load(Ordering::SeqCst),
                        layout_version: layout.layout_version,
                    },
                    context_stale: false,
                    expected_rows: 0,
                    probe: false,
                    pics_required: false,
                });
                s.pending_signal.0.send(()).ok();
            }
        }
    }
    // borrowed contexts, live rows and placements are superseded by the new placements, and
    // facts tagged with an older layout no longer apply
    s.live.lock().on_new_layout(versions.layout_version);
    let current = s.source_revision.load(Ordering::SeqCst);
    let mut reasons = Vec::new();
    if !aux_stable {
        reasons.push("aux family still changing".into());
    }
    if errors > 0 {
        reasons.push(format!("{errors} compile errors"));
    }
    let convergence = if current > rev {
        Convergence::Stale {
            pending_since: rev + 1,
        }
    } else if provisional {
        Convergence::Converging {
            pass: passes,
            reasons: vec!["another pass is running".into()],
        }
    } else if aux_stable && errors == 0 {
        Convergence::Converged
    } else if (!aux_stable && passes >= s.cfg.max_passes) || (aux_stable && errors > 0) {
        // the run is over: either out of passes, or stable with errors that another pass over
        // the same input would repeat. `Converging` here would promise a pass that never runs.
        Convergence::PassLimitReached {
            passes,
            reasons: reasons.clone(),
        }
    } else {
        Convergence::Converging {
            pass: passes,
            reasons: reasons.clone(),
        }
    };
    *s.convergence.lock() = Some(convergence.clone());
    let pages_changed: Vec<PageUpdate> = {
        let layout = s.layout.lock();
        changed
            .iter()
            .filter_map(|n| {
                layout.pages.get(n).map(|dl| PageUpdate {
                    page: *n,
                    exact: dl.is_exact(),
                    hash: layout.page_hashes[n],
                    dl: dl.clone(),
                    native: rtex_dl::gfx::only_literals(dl)
                        .then(|| rtex_dl::gfx::native_graphics(dl).ok())
                        .flatten(),
                })
            })
            .collect()
    };
    let pages_total = cap.json.pages;
    if !diagnostics.is_empty() {
        s.events
            .send(Event::Diagnostics {
                source: "background".into(),
                items: diagnostics,
            })
            .ok();
    }
    // the fallback is named whenever the layout has a degraded page, changed in this
    // layout or not: a host that keeps one PDF per layout needs the current file
    let any_degraded = {
        let layout = s.layout.lock();
        layout.pages.values().any(|dl| !dl.is_exact())
    };
    s.events
        .send(Event::LayoutUpdate {
            versions,
            compile: compile.clone(),
            convergence: convergence.clone(),
            passes,
            pages_changed,
            pages_total,
            placements,
            eligible_paragraphs: eligible,
            pdf_fallback: if any_degraded { pdf } else { None },
            wall_ms: t0.elapsed().as_millis() as u64,
            held,
        })
        .ok();
    if matches!(convergence, Convergence::Stale { .. }) {
        s.bg_signal.0.send(BgCmd::Pass).ok();
    }
    compile
}

/// The current layout's versions, placements and eligible spans (all and allow-listed), as a
/// `LayoutUpdate` reports them.
fn layout_lists(
    s: &Shared,
    files: &BTreeMap<String, FileBuf>,
    layout: &LayoutStore,
) -> (Versions, Vec<ParagraphPlacement>, Vec<ParaId>, Vec<ParaId>) {
    let versions = Versions {
        source_revision: s.source_revision.load(Ordering::SeqCst),
        context_revision: layout.context_revision,
        engine_generation: s.engine_generation.load(Ordering::SeqCst),
        layout_version: layout.layout_version,
    };
    let mut placements = Vec::new();
    let mut eligible = Vec::new();
    // units whose vocabulary the allow-list fully knows (safe to compile unprobed)
    let mut eligible_strict: Vec<ParaId> = Vec::new();
    let policy = s.policy.lock().clone();
    for (id, idx) in &layout.by_span {
        let eu = &layout.units[*idx];
        let c = &eu.captured;
        let rows: Vec<(i64, i64)> = c.placements.iter().map(|p| (p.x, p.y)).collect();
        let kind = match c.kind.as_str() {
            "par" => "par".to_string(),
            k => format!("{k}:{}", c.name.clone().unwrap_or_default()),
        };
        if let Some((frags, _)) = layout.fragments(*id, &rows) {
            placements.push(ParagraphPlacement {
                par_id: *id,
                fragments: frags,
                lines: eu.rows(),
                kind,
            });
        }
        let has_ctx = eu.has_context();
        // eligible = capture facts clean AND the span's source passes the allow-list with the
        // shape the capture saw (what apply_edit will decide for a one-character edit)
        let source_ok = layout
            .snapshot_spans
            .iter()
            .find(|sp| sp.id == *id)
            .map(|sp| !sp.background_only)
            .unwrap_or(false)
            && files
                .values()
                .find_map(|fb| fb.span_text(*id))
                .map(|text| {
                    let (shape, _) = classify_source(text, &policy);
                    match (&shape, c.kind.as_str()) {
                        (UnitShape::Par, "par") => true,
                        (UnitShape::Env(n), "env") => Some(n.as_str()) == c.name.as_deref(),
                        (UnitShape::Heading(n), "heading") => Some(n.as_str()) == c.name.as_deref(),
                        _ => false,
                    }
                })
                .unwrap_or(false);
        if source_ok
            && check_engine_unit(&c.kind, &c.everypar, has_ctx, eu.rows(), &eu.flags).is_empty()
        {
            eligible.push(*id);
            let strict_ok = files
                .values()
                .find_map(|fb| fb.span_text(*id))
                .map(|text| classify_source_with(text, &policy, false).1.is_empty())
                .unwrap_or(false);
            if strict_ok {
                eligible_strict.push(*id);
            }
        }
    }
    placements.sort_by_key(|p| p.par_id);
    eligible.sort();
    eligible_strict.sort();
    (versions, placements, eligible, eligible_strict)
}

/// A held layout whose pages are the ones shown (only the held pictures changed since): the
/// run's errors and the held pictures, no page.
fn publish_unchanged_layout(
    s: &Shared,
    t0: Instant,
    passes: u32,
    rev: Revision,
    diagnostics: Vec<Diagnostic>,
    held: Vec<HeldPicture>,
) {
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count();
    let (versions, placements, eligible, pdf, pages_total, any_degraded) = {
        let files = s.files.lock();
        let layout = s.layout.lock();
        let (versions, placements, eligible, _) = layout_lists(s, &files, &layout);
        let any_degraded = layout.pages.values().any(|dl| !dl.is_exact());
        let pages_total = layout.pages.len() as i64;
        (
            versions,
            placements,
            eligible,
            layout.pdf.clone(),
            pages_total,
            any_degraded,
        )
    };
    let current = s.source_revision.load(Ordering::SeqCst);
    let convergence = if current > rev {
        Convergence::Stale {
            pending_since: rev + 1,
        }
    } else {
        Convergence::PassLimitReached {
            passes,
            reasons: vec![
                format!("{errors} compile errors"),
                format!("{} picture(s) shown in their last good version", held.len()),
            ],
        }
    };
    *s.convergence.lock() = Some(convergence.clone());
    s.events
        .send(Event::Diagnostics {
            source: "background".into(),
            items: diagnostics,
        })
        .ok();
    s.events
        .send(Event::LayoutUpdate {
            versions,
            compile: CompileStatus::CompiledWithErrors { count: errors },
            convergence: convergence.clone(),
            passes,
            pages_changed: vec![],
            pages_total,
            placements,
            eligible_paragraphs: eligible,
            pdf_fallback: if any_degraded { pdf } else { None },
            wall_ms: t0.elapsed().as_millis() as u64,
            held,
        })
        .ok();
    if matches!(convergence, Convergence::Stale { .. }) {
        s.bg_signal.0.send(BgCmd::Pass).ok();
    }
}

pub(super) fn run_export(s: &Shared, job: u64, out: PathBuf) {
    let (texts, _spans, _rev) = snapshot(s);
    let snap_dir = s.cfg.build_dir.join("export-src");
    if let Err(e) = write_snapshot(&s.cfg.project_root, &texts, &snap_dir) {
        s.events
            .send(Event::PdfExported {
                job_id: job,
                path: None,
                status: CompileStatus::Failed,
                converged: false,
                passes: 0,
            })
            .ok();
        let _ = e;
        return;
    }
    let out_dir = s.cfg.build_dir.join("export");
    match run_pass_with(
        &s.tl,
        &snap_dir,
        &s.cfg.main_file,
        &out_dir,
        s.cfg.max_passes,
        s.cfg.bib_tool,
        false,
        "",
    ) {
        Ok(o) => {
            let diags = parse_log(&o.capture.log);
            let errors = diags.iter().filter(|d| d.severity == "error").count();
            let fatal = o.capture.fatal();
            let status = if fatal {
                CompileStatus::Failed
            } else if errors > 0 {
                CompileStatus::CompiledWithErrors { count: errors }
            } else {
                CompileStatus::Ok
            };
            let path = if !fatal {
                if let Some(parent) = out.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::copy(&o.capture.pdf, &out)
                    .ok()
                    .map(|_| out.clone())
            } else {
                None
            };
            s.events
                .send(Event::PdfExported {
                    job_id: job,
                    path,
                    status,
                    converged: o.aux_stable && errors == 0,
                    passes: o.passes,
                })
                .ok();
        }
        Err(_) => {
            s.events
                .send(Event::PdfExported {
                    job_id: job,
                    path: None,
                    status: CompileStatus::Failed,
                    converged: false,
                    passes: 0,
                })
                .ok();
        }
    }
}
