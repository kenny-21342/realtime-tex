//! Reflow previews arrive before a slow suffix finishes; the final PDF stays authoritative.
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use std::time::{Duration, Instant};

fn settled(s: &Session, revision: u64) {
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(
            Instant::now() < until,
            "no settled layout: {:?}",
            s.convergence()
        );
        for event in s.poll(Duration::from_millis(100)) {
            if matches!(event, Event::LayoutUpdate { convergence: Convergence::Converged, versions, .. } if versions.source_revision >= revision)
            {
                return;
            }
        }
    }
}

#[test]
fn a_reflow_page_is_previewed_before_the_document_finishes_and_then_matches_the_clean_pdf() {
    let Ok(tl) = rtex_core::texlive::TexLive::discover() else {
        return;
    };
    let root = std::env::temp_dir().join(format!("rtex-page-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text = "\\documentclass{article}\n\\usepackage[paperwidth=240pt,paperheight=320pt,margin=20pt]{geometry}\n\\pdfvariable suppressoptionalinfo 767\n\\begin{document}\nAnchor words in a paragraph that can grow to wrap across additional lines.\n\nFollowing text moves when the paragraph grows.\n\n\\newpage\n\\directlua{local t=os.clock()+0.8; while os.clock()<t do end}\nSlow suffix.\n\\end{document}\n";
    std::fs::write(root.join("main.tex"), text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(100);
    cfg.fast_budget = Duration::from_secs(5);
    let s = Session::open(cfg).unwrap();
    settled(&s, 1);
    let at = text.find("Anchor").unwrap() + "Anchor".len();
    let mut edit = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at,
                text: " extra words".repeat(20),
            },
        )
        .unwrap();
    assert_eq!(edit.routed, "fast");
    let until = Instant::now() + Duration::from_secs(120);
    let mut preview = None;
    let mut complete = false;
    let mut edited_during_suffix = false;
    while Instant::now() < until && !complete {
        for event in s.poll(Duration::from_millis(50)) {
            match event {
                Event::PagePreview {
                    source_revision,
                    page,
                    placements,
                    ..
                } if source_revision == edit.source_revision && page.page == 1 => {
                    assert!(placements.iter().any(|p| p.lines > 3));
                    if !edited_during_suffix {
                        // Supersede a run after its affected page shipped, while the suffix is
                        // still running. Its truncated aux family must not seed the next run.
                        edit = s
                            .apply_edit(
                                "main.tex",
                                Edit {
                                    start_byte: at,
                                    end_byte: at,
                                    text: " second edit".repeat(5),
                                },
                            )
                            .unwrap();
                        edited_during_suffix = true;
                    } else {
                        preview = Some((Instant::now(), page));
                    }
                }
                Event::LayoutUpdate {
                    versions,
                    convergence: Convergence::Converged,
                    ..
                } if versions.source_revision == edit.source_revision => complete = true,
                _ => {}
            }
        }
    }
    assert!(
        complete && edited_during_suffix,
        "the latest source must converge after a superseded run"
    );
    let (at, page) = preview.expect("the affected page should be delivered as a preview");
    assert!(
        at.elapsed() >= Duration::from_millis(400),
        "the preview must precede the slow suffix, not be emitted after completion"
    );
    let final_text = s.document_text("main.tex").unwrap();
    let clean = root.join("clean");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::write(clean.join("main.tex"), final_text).unwrap();
    let fresh = rtex_core::background::run_pass(
        &tl,
        &clean,
        "main.tex",
        &root.join("clean-out"),
        5,
        rtex_core::background::BibTool::None,
        true,
    )
    .unwrap();
    assert_eq!(page.dl.lines, fresh.capture.page(1).unwrap().lines);
    assert_eq!(
        std::fs::read(root.join("build/bg/main.pdf")).unwrap(),
        std::fs::read(fresh.capture.pdf).unwrap()
    );
    s.close();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_error_in_a_later_page_resets_earlier_previews_without_losing_diagnostic_locations() {
    if rtex_core::texlive::TexLive::discover().is_err() {
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-preview-error-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text = "\\documentclass{article}\n\\begin{document}\nAnchor words in an ordinary paragraph.\n\n\\newpage\n\\directlua{local t=os.clock()+0.6; while os.clock()<t do end}\n\\IfFileExists{failure.flag}{\\undefinedpreviewmacro}{}\nSuffix text.\n\\end{document}\n";
    std::fs::write(root.join("main.tex"), text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.fast_budget = Duration::from_secs(5);
    let s = Session::open(cfg).unwrap();
    settled(&s, 1);
    std::fs::write(root.join("failure.flag"), "").unwrap();
    let at = text.find("Anchor").unwrap() + "Anchor".len();
    let edit = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at,
                text: " extra words".repeat(20),
            },
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(120);
    let (mut seen, mut reset, mut located, mut ended) = (false, false, false, false);
    while Instant::now() < until && !ended {
        for event in s.poll(Duration::from_millis(50)) {
            match event {
                Event::PagePreview {
                    source_revision, ..
                } if source_revision == edit.source_revision => {
                    assert!(!reset);
                    seen = true;
                }
                Event::PagePreviewReset { .. } => reset = true,
                Event::Diagnostics { items, .. } => {
                    located |= items
                        .iter()
                        .any(|d| d.severity == "error" && d.file.is_some() && d.line == Some(7))
                }
                Event::LayoutUpdate {
                    versions,
                    convergence: Convergence::PassLimitReached { .. },
                    ..
                } if versions.source_revision == edit.source_revision => ended = true,
                _ => {}
            }
        }
    }
    assert!(
        seen && reset && located && ended,
        "preview {seen}, reset {reset}, diagnostic {located}, ended {ended}"
    );
    s.close();
    let _ = std::fs::remove_dir_all(root);
}
