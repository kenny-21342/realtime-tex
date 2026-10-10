//! Minimal reproductions of failures found by the mutation test on real documents. Each test
//! opens a session on a small inline document and checks the layout run ends the way
//! docs/CONVERGENCE.md says. Skipped without lualatex.

use rtex_core::session::CompileStatus;
use rtex_core::texlive::TexLive;
use rtex_core::{Convergence, Event, Session, SessionConfig};
use std::time::{Duration, Instant};

fn open(name: &str, main: &str) -> Option<Session> {
    if let Err(e) = TexLive::discover() {
        eprintln!("SKIP: {e}");
        return None;
    }
    let root = std::env::temp_dir().join(format!("rtex-regr-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.tex"), main).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    Some(Session::open(cfg).unwrap())
}

/// Layout updates until one ends the run (anything but a provisional `Converging`), then any
/// that arrive in the next `quiet` (a finished run must not be followed by another one nobody
/// asked for).
fn final_layout(s: &Session, secs: u64, quiet: Duration) -> (CompileStatus, Convergence, Vec<String>, usize) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut errors = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "no layout ended the run within {secs} s");
        for e in s.poll(Duration::from_millis(100)) {
            match e {
                Event::Diagnostics { items, .. } => errors.extend(
                    items.into_iter().filter(|d| d.severity == "error").map(|d| d.message),
                ),
                Event::LayoutUpdate { compile, convergence, .. } => {
                    let provisional = matches!(&convergence, Convergence::Converging { reasons, .. }
                        if reasons.iter().any(|r| r == "another pass is running"));
                    if !provisional {
                        let mut later = 0;
                        let until = Instant::now() + quiet;
                        while Instant::now() < until {
                            later += s
                                .poll(Duration::from_millis(100))
                                .iter()
                                .filter(|e| matches!(e, Event::LayoutUpdate { .. }))
                                .count();
                        }
                        return (compile, convergence, errors, later);
                    }
                }
                _ => {}
            }
        }
    }
}

/// A document with an error that every pass repeats used to end in `Converging` with no pass
/// scheduled: hosts (and the mutation test) waited for a pass that never ran.
#[test]
fn a_persistent_error_ends_the_run() {
    let Some(s) = open(
        "error",
        "\\documentclass{article}\n\\begin{document}\n\\section{A}\\label{a}\nSee \\ref{a}.\n\n\\undefinedmacro\n\nText.\n\\end{document}\n",
    ) else {
        return;
    };
    let (compile, convergence, errors, later) = final_layout(&s, 120, Duration::from_secs(3));
    assert!(matches!(compile, CompileStatus::CompiledWithErrors { .. }), "{compile:?}");
    assert!(!errors.is_empty());
    assert!(
        matches!(convergence, Convergence::PassLimitReached { .. }),
        "a run that cannot get rid of its errors must end, got {convergence:?}"
    );
    assert_eq!(s.convergence(), Some(convergence));
    assert_eq!(later, 0, "no further layout without an edit");
    s.close();
}

/// pgf node names are global: a picture can use a node named in an earlier picture. When the
/// picture cache drew the first picture from an earlier PDF its body did not run, the name was
/// never defined, and a later picture the cache does not take (here: it holds a `\ref`) failed
/// with "No shape named `p' is known" on every pass after the first (the common case: a
/// pgfplots axis named in one picture and used to place the next one).
#[test]
fn a_node_named_in_one_picture_and_used_in_another() {
    let Some(s) = open(
        "picnames",
        "\\documentclass{article}\n\\usepackage{tikz}\n\\begin{document}\n\\section{A}\\label{a}\nSee \\ref{a}.\n\n\\begin{tikzpicture}\n\\node[draw] (p) {P};\n\\end{tikzpicture}\n\n\\begin{tikzpicture}\n\\draw (p.east) -- ++(1,0) node[right] {\\ref{a}};\n\\end{tikzpicture}\n\nText.\n\\end{document}\n",
    ) else {
        return;
    };
    let (compile, convergence, errors, _) = final_layout(&s, 180, Duration::from_millis(500));
    assert_eq!(compile, CompileStatus::Ok, "errors: {errors:?}");
    assert_eq!(convergence, Convergence::Converged);
    s.close();
}

/// A pass that stops on a fatal error before `\\end{document}` (here TeX's limit of 100 errors)
/// writes no capture and no PDF, only a partial file. The capture of an earlier pass in the same
/// directory used to be taken for it: a layout "with errors" showing stale pages, the
/// half-written PDF as the fallback for degraded pages (stress-test replay, typing into
/// `\\pdfextension info`). It is a failure: the previous layout stays.
#[test]
fn a_fatal_error_after_earlier_passes_is_a_failure() {
    let doc = "\\documentclass{article}\n\\begin{document}\nFirst page.\n\\clearpage\nSecond page.\n\\end{document}\n";
    let Some(s) = open("fatal", doc) else {
        return;
    };
    // two good runs: both pass directories hold a capture
    let (compile, ..) = final_layout(&s, 120, Duration::from_millis(500));
    assert_eq!(compile, CompileStatus::Ok);
    let at = doc.find("Second page.").unwrap();
    s.apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "Again. ".into() }).unwrap();
    s.request_layout();
    let (compile, ..) = final_layout(&s, 120, Duration::from_millis(500));
    assert_eq!(compile, CompileStatus::Ok);
    let at = s.document_text("main.tex").unwrap().find("\\end{document}").unwrap();
    s.apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "\\undefinedmacro\n".repeat(101) }).unwrap();
    s.request_layout();
    let (compile, convergence, errors, _) = final_layout(&s, 120, Duration::from_secs(1));
    assert_eq!(compile, CompileStatus::Failed, "errors: {errors:?}");
    match convergence {
        Convergence::PassLimitReached { reasons, .. } => {
            assert!(reasons.iter().any(|r| r.contains("fatal")), "{reasons:?}")
        }
        c => panic!("a failed pass ends the run, got {c:?}"),
    }
    s.close();
}

/// A paragraph that appends to a global expl3 sequence and prints it changes the live server's
/// state on every compile, through a variable its source never names (the leak check compares
/// the meanings of the names a unit mentions). Its probe matched the pass, so the edits after it
/// were served with the items repeated (stress-test replay, `\skpush`/`\sklist`). The probe now
/// compiles the snapshot twice: the second result differs, the unit is demoted.
#[test]
fn hidden_global_state_demotes_the_unit() {
    let doc = "\\documentclass{article}\n\\ExplSyntaxOn\n\\seq_new:N \\g_t_seq\n\\NewDocumentCommand\\push{m}{\\seq_gput_right:Nn \\g_t_seq {#1}}\n\\NewDocumentCommand\\items{}{\\seq_use:Nn \\g_t_seq {,~}}\n\\ExplSyntaxOff\n\\begin{document}\n\\push{alpha}\\push{beta} The items are \\items.\n\nAnother paragraph.\n\\end{document}\n";
    let Some(s) = open("hidden-state", doc) else {
        return;
    };
    let (compile, ..) = final_layout(&s, 120, Duration::from_millis(500));
    assert_eq!(compile, CompileStatus::Ok);
    let at = doc.find("The items").unwrap();
    let r = s
        .apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "Now ".into() })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut served = vec![];
    let mut demoted = vec![];
    while Instant::now() < deadline && served.is_empty() && demoted.is_empty() {
        for e in s.poll(Duration::from_millis(100)) {
            match e {
                Event::ParagraphUpdate { edit_id, status, dl, .. } if edit_id == r.edit_id && status == "ok" => {
                    served.push(dl.lines.len())
                }
                Event::BackgroundScheduled { edit_id, reasons, .. } if edit_id == r.edit_id => demoted.extend(reasons),
                _ => {}
            }
        }
    }
    assert!(served.is_empty(), "served live although its compile changes hidden state: {served:?} rows");
    assert!(demoted.iter().any(|r| r.contains("compiled again")), "{demoted:?}");
    s.close();
}

/// A pdflscape landscape page carries `/Rotate 90` in its page attributes: viewers show it turned.
/// The page list used to say nothing, so hosts drew it upright (stress-test document, p20). It now
/// carries `rotate`. Its rows are also typeset inside a rotated box: they keep the box's own
/// coordinates and no record ties them to the matrix, so the page was drawn with the table off
/// the page while it claimed to be exact. Such rows flag the page (`transformed_rows`).
#[test]
fn landscape_pages_say_they_are_rotated() {
    let Some(s) = open(
        "landscape",
        "\\documentclass{article}\n\\usepackage{pdflscape}\n\\begin{document}\nPortrait page.\n\\begin{landscape}\nA wide table goes here.\n\\end{landscape}\nPortrait again.\n\\end{document}\n",
    ) else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut rotate = std::collections::BTreeMap::new();
    let mut done = false;
    while !done {
        assert!(Instant::now() < deadline, "no converged layout");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate { pages_changed, convergence, .. } = e {
                for p in pages_changed {
                    let flagged = p.dl.flags_map().contains_key("transformed_rows");
                    rotate.insert(p.page, (p.dl.rotate, flagged, p.exact));
                }
                done |= matches!(convergence, Convergence::Converged);
            }
        }
    }
    assert_eq!(
        rotate.into_iter().collect::<Vec<_>>(),
        vec![(1, (0, false, true)), (2, (90, true, false)), (3, (0, false, true))]
    );
    s.close();
}

/// `\paragraph` runs into its text: heading and text, over several source lines, are one TeX
/// paragraph, and the capture gives all its rows to the heading's unit. The heading's span used
/// to end with its line, so an edit there was served as a one-row fragment (stress-test
/// document, `\paragraph{Depth 4.}`).
#[test]
fn runin_heading_is_served_with_its_text() {
    let doc = "\\documentclass{article}\n\\begin{document}\nSome text before the heading.\n\n\\paragraph{Run-in heading.} This paragraph starts on the heading's line and continues\nover several source lines, so that heading and text are set as one paragraph with more\nthan one row in the output, which the fast path must reproduce exactly.\n\nClosing paragraph.\n\\end{document}\n";
    let Some(s) = open("runin", doc) else {
        return;
    };
    let mut o = rtex_core::replay::Observer::new();
    assert!(o.pump(&s, Duration::from_secs(120), |o| o.ended), "no first layout");
    let at = doc.find("starts on").unwrap();
    let r = s
        .apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "quickly ".into() })
        .unwrap();
    assert_eq!(r.routed, "fast", "{:?}", r.reasons);
    let eid = r.edit_id;
    assert!(o.pump(&s, Duration::from_secs(20), |o| o.updates.keys().any(|(e, _)| *e == eid)), "no fast result");
    assert!(o.settle(&s, r.source_revision, Duration::from_secs(120)), "no settled layout");
    let served: Vec<_> = o.updates.iter().filter(|((e, _), _)| *e == eid).map(|(_, v)| v.clone()).collect();
    for sv in &served {
        let j = o.judge(sv);
        assert!(
            matches!(j.verdict, rtex_core::replay::Verdict::Match | rtex_core::replay::Verdict::DeclinedStatus),
            "{:?}: {:?}",
            j.verdict,
            j.details
        );
    }
    s.close();
}

/// Material LaTeX adds after `shipout/before` (the shipout/background and /foreground hooks:
/// eso-pic, pdfpages' inserted pages, watermarks) is not in the captured page, which used to be
/// reported exact without it (stress-test document, an `\includepdf` page drawn empty). Such a
/// page is flagged `shipout_extras`: Degraded, drawn from its PDF.
#[test]
fn shipout_background_material_degrades_the_page() {
    let Some(s) = open(
        "esopic",
        "\\documentclass{article}\n\\usepackage{eso-pic}\n\\begin{document}\n\\AddToShipoutPictureBG*{\\put(50,50){\\rule{2cm}{2cm}}}\nFirst page, with a background square.\n\\newpage\nSecond page, plain.\n\\end{document}\n",
    ) else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut pages = std::collections::BTreeMap::new();
    let mut done = false;
    while !done {
        assert!(Instant::now() < deadline, "no converged layout");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate { pages_changed, convergence, .. } = e {
                for p in pages_changed {
                    pages.insert(p.page, (p.dl.flags_map().contains_key("shipout_extras"), p.exact));
                }
                done |= matches!(convergence, Convergence::Converged);
            }
        }
    }
    assert_eq!(pages.into_iter().collect::<Vec<_>>(), vec![(1, (true, false)), (2, (false, true))]);
    s.close();
}

/// A repeated `\includegraphics` reuses the image luatex.def saved the first time and saves
/// nothing, so `\lastsavedimageresourceindex` names the last image saved, another file: its
/// index was recorded as that file (or not at all), and hosts had no source, or the wrong one,
/// for it (stress-test document: an image included 16 times). And that number counts box
/// resources too (TikZ saves its shadings as box resources at load), while IMAGE items carry the
/// image's own index: with TikZ loaded no image of a page had a source.
#[test]
fn repeated_images_keep_their_files() {
    const PNG: &[u8] = TINY_PNG;
    let doc = "\\documentclass{article}\n\\usepackage{graphicx,tikz}\n\\begin{document}\n\\includegraphics[width=1cm]{a.png}\n\\includegraphics[width=1cm]{b.png}\n\\includegraphics[width=1cm]{a.png}\n\\end{document}\n";
    if TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-regr-{}-images", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.tex"), doc).unwrap();
    std::fs::write(project.join("a.png"), PNG).unwrap();
    std::fs::write(project.join("b.png"), PNG).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    let s = Session::open(cfg).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut files = vec![];
    while files.is_empty() {
        assert!(Instant::now() < deadline, "no converged layout");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate { pages_changed, convergence, .. } = e {
                if matches!(convergence, Convergence::Converged) {
                    for p in pages_changed {
                        let items = p.dl.other.iter().chain(p.dl.lines.iter().flat_map(|l| l.items.iter()));
                        for it in items {
                            if let rtex_dl::Item::Image { index, .. } = it {
                                files.push(p.dl.images.get(&index.to_string()).map(|i| i.file.clone()));
                            }
                        }
                    }
                }
            }
        }
    }
    let f = |n: &str| Some(n.to_string());
    assert_eq!(files, vec![f("a.png"), f("b.png"), f("a.png")]);
    s.close();
}

/// A 1x1 PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00,
    0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18,
    0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// The live server keeps the images it saved: from the second compile of a paragraph on, its
/// images are reused and `\lastsavedimageresourceindex` names another one; the result named the
/// wrong files (same cause as `repeated_images_keep_their_files`).
#[test]
fn live_results_name_reused_images() {
    let doc = "\\documentclass{article}\n\\usepackage{graphicx,tikz}\n\\begin{document}\nTwo images \\includegraphics[width=1cm]{a.png} and \\includegraphics[width=1cm]{b.png} in a paragraph.\n\nAnother paragraph.\n\\end{document}\n";
    if TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-regr-{}-live-images", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.tex"), doc).unwrap();
    std::fs::write(project.join("a.png"), TINY_PNG).unwrap();
    std::fs::write(project.join("b.png"), TINY_PNG).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    let s = Session::open(cfg).unwrap();
    let mut o = rtex_core::replay::Observer::new();
    assert!(o.pump(&s, Duration::from_secs(120), |o| o.ended), "no first layout");
    let mut files = vec![];
    // a word, then the two images swapped: the second compile reuses both, b.png first
    for step in 0..2 {
        let text = s.document_text("main.tex").unwrap();
        let edit = if step == 0 {
            let at = text.find("images").unwrap();
            rtex_core::Edit { start_byte: at, end_byte: at, text: "new ".into() }
        } else {
            let (a, b) = (text.find("{a.png}").unwrap(), text.find("{b.png}").unwrap());
            rtex_core::Edit { start_byte: a, end_byte: b + 7, text: "{b.png} and \\includegraphics[width=1cm]{a.png}".into() }
        };
        let r = s.apply_edit("main.tex", edit).unwrap();
        assert_eq!(r.routed, "fast", "{:?}", r.reasons);
        let eid = r.edit_id;
        assert!(o.pump(&s, Duration::from_secs(20), |o| o.updates.keys().any(|(e, _)| *e == eid)), "no fast result");
        let sv = o.updates.iter().find(|((e, _), _)| *e == eid).map(|(_, v)| v.clone()).unwrap();
        files = sv
            .dl
            .lines
            .iter()
            .flat_map(|l| l.items.iter())
            .filter_map(|it| match it {
                rtex_dl::Item::Image { index, .. } => Some(sv.dl.images.get(&index.to_string()).map(|i| i.file.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
    }
    let f = |n: &str| Some(n.to_string());
    assert_eq!(files, vec![f("b.png"), f("a.png")], "the second compile's images");
    s.close();
}

/// A standby engine loads the preamble while the user types, so a background pass costs the
/// body only. The pass compared the standby's preamble hash (its `\input` files inlined) with
/// one of the raw preamble text: for a preamble that `\input`s a file they never matched, and
/// every pass started from scratch (the stress-test document: 19-20 s a pass instead of 14-15).
#[test]
fn a_preamble_with_inputs_uses_the_standby() {
    if TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-regr-{}-standby", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("main.tex"),
        "\\documentclass{article}\n\\input{setup}\n\\begin{document}\nA paragraph to edit.\n\nAnother one.\n\\end{document}\n",
    )
    .unwrap();
    std::fs::write(project.join("setup.tex"), "\\usepackage{amsmath}\n").unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    let s = Session::open(cfg).unwrap();
    let mut o = rtex_core::replay::Observer::new();
    assert!(o.pump(&s, Duration::from_secs(120), |o| o.ended), "no first layout");
    let before = s.background_passes();
    let at = s.document_text("main.tex").unwrap().find("to edit").unwrap();
    let r = s
        .apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "quickly ".into() })
        .unwrap();
    // give the standby time to load its preamble before the pass is asked for
    std::thread::sleep(Duration::from_secs(3));
    assert!(o.settle(&s, r.source_revision, Duration::from_secs(120)), "no settled layout");
    let after = s.background_passes();
    assert!(after.0 > before.0, "no warm pass after the edit: before {before:?}, after {after:?}");
    s.close();
}

/// TeX stops reading a file after the line that holds `\endinput`. A paragraph ending in it
/// (with the file's junk after it, which TeX never reads) was a fast-path unit: compiled in the
/// live server, `\endinput` ended the server's own input and the watchdog killed it after 5 s
/// (stress-test document, an `\input` file at depth 4).
#[test]
fn endinput_never_reaches_the_live_server() {
    if TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-regr-{}-endinput", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("main.tex"),
        "\\documentclass{article}\n\\begin{document}\nFirst paragraph.\n\n\\input{part}\n\nLast paragraph.\n\\end{document}\n",
    )
    .unwrap();
    std::fs::write(
        project.join("part.tex"),
        "A paragraph of the part that ends\nthe file here.\n\\endinput\nNever read: \\undefinedmacro.\n",
    )
    .unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    let s = Session::open(cfg).unwrap();
    let mut o = rtex_core::replay::Observer::new();
    assert!(o.pump(&s, Duration::from_secs(120), |o| o.ended), "no first layout");
    let gen = s.versions().engine_generation;
    let at = s.document_text("part.tex").unwrap().find("ends").unwrap();
    let r = s
        .apply_edit("part.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "quickly ".into() })
        .unwrap();
    assert_ne!(r.routed, "fast", "a span with \\endinput went to the live server");
    // the junk after \endinput is no unit either
    let junk = s.document_text("part.tex").unwrap().find("Never").unwrap();
    assert!(s.spans("part.tex").iter().all(|sp| !sp.range.contains(&junk)));
    assert!(o.settle(&s, r.source_revision, Duration::from_secs(120)), "no settled layout");
    assert_eq!(s.versions().engine_generation, gen, "the live engine was restarted");
    s.close();
}

/// `makeindex` runs between passes as bibtex does: the index of a document that prints one
/// (`\makeindex`, `\index`, `\printindex`) used to be missing from every layout.
#[test]
fn the_index_is_built() {
    let Some(s) = open(
        "index",
        "\\documentclass{article}\n\\usepackage{makeidx}\n\\makeindex\n\\begin{document}\nAlpha\\index{alpha} and beta\\index{beta}.\n\\printindex\n\\end{document}\n",
    ) else {
        return;
    };
    let mut o = rtex_core::replay::Observer::new();
    assert!(o.pump(&s, Duration::from_secs(120), |o| o.ended), "no layout");
    assert!(o.converged, "{}", o.dump(&s, "did not converge"));
    // the index starts a page of its own (article's theindex), with the entries on it
    assert_eq!(o.pages_total, 2, "{}", o.dump(&s, "no index page"));
    let page = &o.pages[&2];
    let chars: String = page
        .lines
        .iter()
        .flat_map(|l| l.items.iter())
        .chain(page.other.iter())
        .filter_map(|it| match it {
            rtex_dl::Item::Glyph { char, .. } => char::from_u32(*char as u32),
            _ => None,
        })
        .collect();
    assert!(chars.contains("alpha") && chars.contains("beta"), "index page text: {chars}");
    s.close();
}

/// The error diagnostics in force when the first layout arrives, and that layout's status.
fn first_layout_errors(s: &Session, secs: u64) -> (CompileStatus, Vec<rtex_core::session::Diagnostic>) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut errors = vec![];
    while Instant::now() < deadline {
        for e in s.poll(Duration::from_millis(100)) {
            match e {
                Event::Diagnostics { items, .. } => {
                    errors = items.into_iter().filter(|d| d.severity == "error").collect()
                }
                Event::LayoutUpdate { compile, .. } => return (compile, errors),
                _ => {}
            }
        }
    }
    panic!("no layout within {secs} s");
}

/// A comment that mentions `\begin{document}` above the real one (the stress test's `broken`
/// cases describe their mistake in a header comment) was taken for the start of the body: the
/// preamble ended at the comment, the class and packages were compiled as body text, and every
/// error was reported on the wrong line ("Can be used only in preamble" at the comment).
#[test]
fn a_commented_begin_document_is_not_the_body() {
    let doc = "% Mistake: a typo after \\begin{document}\n\\documentclass{article}\n\\usepackage{amsmath}\n\\begin{document}\nSome text.\n\\begin{theorm}\nA claim.\n\\end{theorm}\n\\end{document}\n";
    let Some(s) = open("commented-begin", doc) else {
        return;
    };
    let (_, errors) = first_layout_errors(&s, 120);
    let first = errors.first().expect("the undefined environment is an error");
    assert!(first.message.contains("Environment theorm undefined"), "{errors:?}");
    assert_eq!(first.line, Some(6), "{errors:?}");
    assert!(first.file.as_deref().is_some_and(|f| f.ends_with("main.tex")), "{errors:?}");
    assert!(!errors.iter().any(|d| d.message.contains("preamble")), "{errors:?}");
    s.close();
}

/// A fatal error (TeX gives up before the end of the document) used to arrive as one diagnostic
/// holding the whole terminal output, with no file or line. It is now read from the pass's log
/// like any other error.
#[test]
fn a_fatal_error_has_a_location() {
    // \left without \right: TeX inserts \right. on every token until it gives up at 100 errors
    let doc = "\\documentclass{article}\n\\begin{document}\nFine.\n\nBrackets $\\left( a + b$ unbalanced.\n\nMore.\n\\end{document}\n";
    let Some(s) = open("fatal-location", doc) else {
        return;
    };
    let (compile, errors) = first_layout_errors(&s, 120);
    assert_eq!(compile, CompileStatus::Failed, "{errors:?}");
    let first = errors.first().expect("an error");
    assert!(first.message.contains("Missing \\right"), "{errors:?}");
    assert_eq!(first.line, Some(5), "{errors:?}");
    assert!(first.file.as_deref().is_some_and(|f| f.ends_with("main.tex")), "{errors:?}");
    assert!(errors.iter().all(|d| d.message.len() < 1000), "{errors:?}");
    s.close();
}

/// A picture whose compile reports an error (a TikZ path without its semicolon) was cached
/// after the first pass; the next pass took it from the cache, never compiled it, and the
/// document came out clean (stress test, `broken/engine-tikz-semicolon`). Pictures with an
/// error in their lines are not cached.
#[test]
fn a_picture_with_an_error_is_not_cached() {
    let doc = "\\documentclass{article}\n\\usepackage{tikz}\n\\begin{document}\nText.\n\n\\begin{tikzpicture}\n  \\draw (0,0) -- (1,1)\n\\end{tikzpicture}\n\\end{document}\n";
    let Some(s) = open("picture-error", doc) else {
        return;
    };
    let (compile, convergence, errors, _) = final_layout(&s, 120, Duration::from_millis(500));
    assert!(matches!(compile, CompileStatus::CompiledWithErrors { .. }), "{compile:?} {convergence:?}");
    assert!(errors.iter().any(|e| e.contains("semicolon")), "{errors:?}");
    // a second run (an edit elsewhere) still compiles the picture
    let at = doc.find("Text.").unwrap();
    s.apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: "More ".into() }).unwrap();
    s.request_layout();
    let (compile, _, errors, _) = final_layout(&s, 120, Duration::from_millis(500));
    assert!(matches!(compile, CompileStatus::CompiledWithErrors { .. }), "{compile:?}");
    assert!(errors.iter().any(|e| e.contains("semicolon")), "{errors:?}");
    s.close();
}

/// LaTeX reads past a package's `\usepackage` line for its optional date: an option clash on
/// the last package is raised at the `\begin{document}` line. The standby reads the preamble
/// from its own file and the error came without a location.
#[test]
fn an_error_at_the_end_of_the_preamble_is_on_begin_document() {
    let doc = "\\documentclass{article}\n\\usepackage[final]{graphicx}\n\\usepackage[draft]{graphicx}\n\\begin{document}\nText.\n\\end{document}\n";
    let Some(s) = open("option-clash", doc) else {
        return;
    };
    let (_, errors) = first_layout_errors(&s, 120);
    let first = errors.first().expect("the option clash is an error");
    assert!(first.message.contains("Option clash"), "{errors:?}");
    assert_eq!((first.file.as_deref(), first.line), (Some("main.tex"), Some(4)), "{errors:?}");
    s.close();
}

/// A document that loops forever (`\def\x{\x}\x`) held the background path: its pass never
/// ended, no layout came, and later edits waited behind it. A pass now stops after
/// `pass_timeout` and the run fails.
#[test]
fn an_endless_loop_stops_the_pass() {
    if TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-regr-{}-endless", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let doc = "\\documentclass{article}\n\\begin{document}\nText.\n\n\\def\\x{\\x}\\x\n\\end{document}\n";
    std::fs::write(project.join("main.tex"), doc).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    cfg.pass_timeout = Duration::from_secs(5);
    let s = Session::open(cfg).unwrap();
    let t0 = Instant::now();
    let (compile, convergence, errors, _) = final_layout(&s, 60, Duration::from_millis(200));
    assert_eq!(compile, CompileStatus::Failed, "{convergence:?} {errors:?}");
    assert!(t0.elapsed() < Duration::from_secs(30), "{:?}", t0.elapsed());
    assert!(errors.iter().any(|e| e.contains("did not finish")), "{errors:?}");
    // the live server never loads the loop (it is no setup statement) and closing is prompt
    s.close();
    assert!(t0.elapsed() < Duration::from_secs(40), "closed after {:?}", t0.elapsed());
}

/// Once a pass has finished, a pass that runs twice as long as the slowest one while the
/// sources change under it is stopped: removing an endless loop gets a layout without waiting
/// for `pass_timeout`.
#[test]
fn removing_an_endless_loop_recovers_quickly() {
    let doc = "\\documentclass{article}\n\\begin{document}\nText.\n\nMore text.\n\\end{document}\n";
    let Some(s) = open("endless-fixed", doc) else {
        return;
    };
    let (compile, ..) = final_layout(&s, 120, Duration::from_millis(200));
    assert_eq!(compile, CompileStatus::Ok);
    let at = doc.find("More text.").unwrap();
    let lp = "\\def\\x{\\x}\\x ";
    s.apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at, text: lp.into() }).unwrap();
    s.request_layout();
    // the looping pass starts; then the loop is removed again
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        s.poll(Duration::from_millis(100));
    }
    s.apply_edit("main.tex", rtex_core::Edit { start_byte: at, end_byte: at + lp.len(), text: String::new() }).unwrap();
    s.request_layout();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(60);
    let mut ok = false;
    while Instant::now() < deadline && !ok {
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate { compile: CompileStatus::Ok, versions, .. } = e {
                ok = versions.source_revision >= s.versions().source_revision;
            }
        }
    }
    assert!(ok, "no clean layout of the fixed document within 60 s");
    assert!(t0.elapsed() < Duration::from_secs(30), "{:?}", t0.elapsed());
    s.close();
}
