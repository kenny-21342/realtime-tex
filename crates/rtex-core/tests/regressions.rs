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
