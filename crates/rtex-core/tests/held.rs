//! Held pictures (docs/how-it-works.md, "Held pictures"): while an edit leaves a picture with
//! errors, layouts keep the picture's last good drawing; its errors are still reported.
//! Skipped without lualatex.

use rtex_core::session::{CompileStatus, HeldPicture, PageUpdate};
use rtex_core::texlive::TexLive;
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use std::time::{Duration, Instant};

const TRIANGLE: &str = "\\documentclass{article}\n\\usepackage{tikz}\n\\begin{document}\nBefore the picture.\n\n\\begin{tikzpicture}\n\\draw[fill=blue!20] (0,0) -- (3,0) -- (1.5,2) -- cycle;\n\\end{tikzpicture}\n\nAfter the picture.\n\\end{document}\n";

fn open(name: &str, main: &str) -> Option<(Session, String)> {
    if let Err(e) = TexLive::discover() {
        eprintln!("SKIP: {e}");
        return None;
    }
    let root = std::env::temp_dir().join(format!("rtex-held-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.tex"), main).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    Some((Session::open(cfg).unwrap(), main.to_string()))
}

struct Layout {
    compile: CompileStatus,
    convergence: Convergence,
    pages: Vec<PageUpdate>,
    held: Vec<HeldPicture>,
}

/// The layout that ends the run (not a provisional one), and nothing after it for `quiet`.
fn final_layout(s: &Session) -> Layout {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(Instant::now() < deadline, "no layout ended the run");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate {
                compile,
                convergence,
                pages_changed,
                held,
                ..
            } = e
            {
                let provisional = matches!(&convergence, Convergence::Converging { reasons, .. }
                    if reasons.iter().any(|r| r == "another pass is running"));
                let stale = matches!(convergence, Convergence::Stale { .. });
                if !provisional && !stale {
                    return Layout {
                        compile,
                        convergence,
                        pages: pages_changed,
                        held,
                    };
                }
            }
        }
    }
}

/// Replace `old` by `new` in the tracked text and in the session.
fn edit(s: &Session, text: &mut String, old: &str, new: &str) {
    let at = text
        .find(old)
        .unwrap_or_else(|| panic!("{old:?} not in the text"));
    s.apply_edit(
        "main.tex",
        Edit {
            start_byte: at,
            end_byte: at + old.len(),
            text: new.into(),
        },
    )
    .unwrap();
    text.replace_range(at..at + old.len(), new);
    s.request_layout();
}

/// The points of the page's native fills/strokes, as JSON (to compare drawings).
fn drawn_paths(p: &PageUpdate) -> Vec<String> {
    let native =
        serde_json::to_value(p.native.as_ref().expect("the page is drawn natively")).unwrap();
    native["ops"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|op| op.get("path").map(|path| path.to_string()))
        .collect()
}

#[test]
fn a_broken_picture_keeps_its_last_good_drawing() {
    let Some((s, mut text)) = open("triangle", TRIANGLE) else {
        return;
    };
    let first = final_layout(&s);
    assert_eq!(first.compile, CompileStatus::Ok);
    let good = drawn_paths(&first.pages[0]);
    assert_eq!(good.len(), 1, "{good:?}");

    // a half-typed coordinate: TeX reads the rest of the document into it
    edit(&s, &mut text, "(1.5,2)", "(1.5,");
    let broken = final_layout(&s);
    assert!(
        matches!(broken.compile, CompileStatus::CompiledWithErrors { .. }),
        "{:?}",
        broken.compile
    );
    assert_ne!(broken.convergence, Convergence::Converged);
    assert_eq!(broken.held.len(), 1, "{:?}", broken.held);
    assert_eq!((broken.held[0].line, broken.held[0].end_line), (6, 8));
    // only the picture changed: the pages shown stay
    assert!(broken.pages.is_empty(), "{} pages", broken.pages.len());

    // text edited elsewhere meanwhile: a held run shows it, with the last good picture
    edit(
        &s,
        &mut text,
        "After the picture.",
        "After the picture, edited.",
    );
    let held_run = final_layout(&s);
    assert_eq!(held_run.held.len(), 1);
    assert!(matches!(
        held_run.compile,
        CompileStatus::CompiledWithErrors { .. }
    ));
    assert_eq!(held_run.pages.len(), 1);
    assert_eq!(
        drawn_paths(&held_run.pages[0]),
        good,
        "the last good drawing"
    );

    // the coordinate completed: the new picture
    edit(&s, &mut text, "(1.5,", "(1.5,3)");
    let fixed = final_layout(&s);
    assert_eq!(fixed.compile, CompileStatus::Ok);
    assert!(fixed.held.is_empty());
    assert_eq!(fixed.convergence, Convergence::Converged);
    let new = drawn_paths(&fixed.pages[0]);
    assert_eq!(new.len(), 1);
    assert_ne!(new, good, "the edited triangle");
    s.close();
}

#[test]
fn a_new_broken_picture_is_not_held() {
    let doc = "\\documentclass{article}\n\\usepackage{tikz}\n\\begin{document}\nBefore.\n\nAfter.\n\\end{document}\n";
    let Some((s, mut text)) = open("new", doc) else {
        return;
    };
    assert_eq!(final_layout(&s).compile, CompileStatus::Ok);
    edit(
        &s,
        &mut text,
        "Before.\n",
        "Before.\n\n\\begin{tikzpicture}\n\\draw (0,0) -- (1.5,;\n\\end{tikzpicture}\n",
    );
    let l = final_layout(&s);
    assert!(matches!(
        l.compile,
        CompileStatus::CompiledWithErrors { .. }
    ));
    assert!(l.held.is_empty(), "no last good version: {:?}", l.held);
    s.close();
}

#[test]
fn holding_can_be_turned_off() {
    if TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-held-{}-off", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.tex"), TRIANGLE).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    cfg.hold_broken_pictures = false;
    let s = Session::open(cfg).unwrap();
    let mut text = TRIANGLE.to_string();
    assert_eq!(final_layout(&s).compile, CompileStatus::Ok);
    edit(&s, &mut text, "(1.5,2)", "(1.5,");
    let l = final_layout(&s);
    assert!(l.held.is_empty());
    assert!(!l.pages.is_empty(), "the broken pass is shown");
    s.close();
}

#[test]
fn export_during_a_hold_is_the_real_build() {
    let Some((s, mut text)) = open("export", TRIANGLE) else {
        return;
    };
    assert_eq!(final_layout(&s).compile, CompileStatus::Ok);
    edit(&s, &mut text, "(1.5,2)", "(1.5,");
    assert_eq!(final_layout(&s).held.len(), 1);
    let out = std::env::temp_dir().join(format!("rtex-held-{}-export.pdf", std::process::id()));
    let job = s.export_pdf(&out);
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = 'wait: loop {
        assert!(Instant::now() < deadline, "no export result");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::PdfExported { job_id, status, .. } = e {
                if job_id == job {
                    break 'wait status;
                }
            }
        }
    };
    assert!(
        !matches!(status, CompileStatus::Ok),
        "the export compiles the broken source: {status:?}"
    );
    s.close();
}
