//! Adding paragraphs must provide live results and page progress before a slow suffix ends.
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use std::time::{Duration, Instant};

fn settled(s: &Session, revision: u64) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        for e in s.poll(Duration::from_millis(50)) {
            if matches!(e, Event::LayoutUpdate { versions, convergence: Convergence::Converged, .. } if versions.source_revision >= revision)
            {
                return;
            }
        }
    }
    panic!("not settled: {:?}", s.convergence());
}

#[test]
fn new_paragraphs_preview_before_the_suffix_and_match_a_clean_pdf() {
    let Ok(tl) = rtex_core::texlive::TexLive::discover() else {
        return;
    };
    let root = std::env::temp_dir().join(format!("rtex-new-paragraph-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text = "\\documentclass{article}\n\\usepackage[paperwidth=240pt,paperheight=320pt,margin=20pt]{geometry}\n\\pdfvariable suppressoptionalinfo 767\n\\AddToHook{para/begin}{\\directlua{if rtex_serve then local t=os.clock()+0.15; while os.clock()<t do end end}}\n\\begin{document}\nAnchor words.\n\nFollowing text and a later page.\n\n\\newpage\n\\directlua{local t=os.clock()+1; while os.clock()<t do end}\nSlow suffix.\n\\end{document}\n";
    std::fs::write(root.join("main.tex"), text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(100);
    cfg.fast_budget = Duration::from_secs(5);
    let s = Session::open(cfg).unwrap();
    settled(&s, 1);
    let at = text.find("Anchor words.").unwrap() + "Anchor words.".len();
    let edit = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at,
                text: "\n\nBrand new paragraph words.".into(),
            },
        )
        .unwrap();
    assert_eq!(edit.routed, "fast", "{:?}", edit.reasons);
    let id = *edit.outcome.added.first().expect("a brand new source span");
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut live, mut preview, mut complete) = (false, None, false);
    while Instant::now() < deadline && !complete {
        for e in s.poll(Duration::from_millis(20)) {
            match e {
                Event::ParagraphUpdate {
                    par_id, status, dl, ..
                } if par_id == id => {
                    assert_eq!(status, "ok");
                    assert!(!dl.lines.is_empty());
                    live = true;
                }
                Event::PagePreview {
                    placements,
                    page,
                    source_revision,
                    ..
                } if source_revision == edit.source_revision
                    && placements.iter().any(|p| p.par_id == id) =>
                {
                    preview = Some((Instant::now(), page));
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
    assert!(live && complete);
    let (when, page) = preview.expect("the new paragraph must have an early page preview");
    assert!(
        when.elapsed() >= Duration::from_millis(400),
        "preview must precede the slow suffix"
    );
    let clean = root.join("clean");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::write(clean.join("main.tex"), s.document_text("main.tex").unwrap()).unwrap();
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
    assert_eq!(page.dl.lines, fresh.capture.page(page.page).unwrap().lines);
    assert_eq!(
        std::fs::read(root.join("build/bg/main.pdf")).unwrap(),
        std::fs::read(fresh.capture.pdf).unwrap()
    );
    s.close();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_overflowing_new_paragraph_keeps_its_live_box_and_ships_all_rows_before_yielding() {
    let Ok(tl) = rtex_core::texlive::TexLive::discover() else {
        return;
    };
    let root = std::env::temp_dir().join(format!(
        "rtex-new-paragraph-overflow-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text="\\documentclass{article}\n\\usepackage[paperwidth=240pt,paperheight=180pt,margin=20pt]{geometry}\n\\pdfvariable suppressoptionalinfo 767\n\\AddToHook{shipout/before}{\\directlua{local t=os.clock()+0.06; while os.clock()<t do end}}\n\\begin{document}\nAnchor words.\n\n\\newpage\n\\directlua{local t=os.clock()+1; while os.clock()<t do end}\nSlow suffix.\n\\end{document}\n";
    std::fs::write(root.join("main.tex"), text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(100);
    cfg.fast_budget = Duration::from_secs(5);
    let s = Session::open(cfg).unwrap();
    settled(&s, 1);
    let at = text.find("Anchor words.").unwrap() + "Anchor words.".len();
    let added = format!("\n\nNew paragraph{}.", " brandword".repeat(160));
    let first = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at,
                text: added.clone(),
            },
        )
        .unwrap();
    assert_eq!(first.routed, "fast");
    let id = first.outcome.added[0];
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut rows = 0;
    let mut original_rows = 0;
    let mut edited = false;
    let mut revision = first.source_revision;
    let mut complete = false;
    while Instant::now() < deadline && !complete {
        for e in s.poll(Duration::from_millis(1)) {
            match e {
                Event::ParagraphUpdate {
                    par_id,
                    versions,
                    dl,
                    fragments,
                    reasons,
                    ..
                } if par_id == id && versions.source_revision == first.source_revision => {
                    rows = dl.lines.len() as i64;
                    assert!(rows > 10);
                    assert!(fragments.is_empty(), "an off-page guess must not be drawn");
                    assert!(reasons.iter().any(|r| r == "placement pending"));
                }
                Event::PagePreview {
                    source_revision,
                    placements,
                    ..
                } if source_revision == first.source_revision => {
                    if let Some(p) = placements.iter().find(|p| p.par_id == id) {
                        original_rows = original_rows.max(p.lines);
                        if !edited {
                            let pos = at + added.len();
                            revision = s
                                .apply_edit(
                                    "main.tex",
                                    Edit {
                                        start_byte: pos,
                                        end_byte: pos,
                                        text: " extra".into(),
                                    },
                                )
                                .unwrap()
                                .source_revision;
                            edited = true;
                        }
                    }
                }
                Event::BackgroundScheduled {
                    par_id: Some(par),
                    reasons,
                    ..
                } if par == id => assert!(!reasons.iter().any(|r| r.contains("fall off"))),
                Event::LayoutUpdate {
                    versions,
                    convergence: Convergence::Converged,
                    ..
                } if edited && versions.source_revision == revision => complete = true,
                _ => {}
            }
        }
    }
    assert!(complete && edited && rows > 0);
    assert_eq!(
        original_rows, rows,
        "a superseded run must deliver the entire new paragraph, including its last page"
    );
    let clean = root.join("clean");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::write(clean.join("main.tex"), s.document_text("main.tex").unwrap()).unwrap();
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
    assert_eq!(
        std::fs::read(root.join("build/bg/main.pdf")).unwrap(),
        std::fs::read(fresh.capture.pdf).unwrap()
    );
    s.close();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_new_paragraph_does_not_cancel_before_the_following_drawing_is_visible() {
    if rtex_core::texlive::TexLive::discover().is_err() {
        return;
    }
    let root =
        std::env::temp_dir().join(format!("rtex-new-paragraph-drawing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text="\\documentclass{article}\n\\usepackage[paperwidth=240pt,paperheight=180pt,margin=20pt]{geometry}\n\\AddToHook{shipout/before}{\\directlua{local t=os.clock()+0.06; while os.clock()<t do end}}\n\\begin{document}\nAnchor words.\n\n\\begin{center}\n\\rule{160pt}{70pt}\n\\end{center}\n\n\\newpage\n\\directlua{local t=os.clock()+1; while os.clock()<t do end}\nSlow suffix.\n\\end{document}\n";
    std::fs::write(root.join("main.tex"), text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.fast_budget = Duration::from_secs(5);
    cfg.debounce = Duration::from_millis(100);
    let s = Session::open(cfg).unwrap();
    settled(&s, 1);
    let drawing = s
        .spans("main.tex")
        .into_iter()
        .find(|sp| text[sp.range.clone()].contains("\\begin{center}"))
        .unwrap()
        .id;
    let at = text.find("Anchor words.").unwrap() + "Anchor words.".len();
    let added = format!("\n\nNew paragraph{}.", " word".repeat(200));
    let first = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at,
                text: added.clone(),
            },
        )
        .unwrap();
    let id = first.outcome.added[0];
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut edited = false;
    let mut figure = false;
    let mut revision = first.source_revision;
    let mut done = false;
    while Instant::now() < deadline && !done {
        for e in s.poll(Duration::from_millis(1)) {
            match e {
                Event::PagePreview {
                    source_revision,
                    placements,
                    page,
                    ..
                } if source_revision == first.source_revision => {
                    if placements.iter().any(|p| p.par_id == id) && !edited {
                        let pos = at + added.len();
                        revision = s
                            .apply_edit(
                                "main.tex",
                                Edit {
                                    start_byte: pos,
                                    end_byte: pos,
                                    text: " more".into(),
                                },
                            )
                            .unwrap()
                            .source_revision;
                        edited = true;
                    }
                    if placements.iter().any(|p| p.par_id == drawing) {
                        assert!(page.dl.lines.iter().any(|l| l
                            .items
                            .iter()
                            .any(|i| matches!(i, rtex_dl::Item::Rule { .. }))));
                        figure = true;
                    }
                }
                Event::LayoutUpdate {
                    versions,
                    convergence: Convergence::Converged,
                    ..
                } if edited && versions.source_revision == revision => done = true,
                _ => {}
            }
        }
    }
    assert!(
        edited && done && figure,
        "the superseded preview must include the drawing after the added paragraph"
    );
    s.close();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn typing_new_paragraphs_after_boundaries_becomes_captured_live_text() {
    if rtex_core::texlive::TexLive::discover().is_err() {
        return;
    }
    for (name, prefix) in [
        ("heading", "\\section{Title}\n"),
        ("runin", "\\paragraph{Title} Existing text.\n"),
        ("display-paragraph", "\\paragraph{Title}\n"),
        ("display-unindented", "\\paragraph{Title}\n"),
        ("pending-runin", "\\section{Section}\n\\paragraph{Title}\n"),
        (
            "list",
            "\\begin{itemize}\n\\item Existing text.\n\\end{itemize}\n",
        ),
        (
            "inline-list",
            "\\noindent Existing text.\\begin{itemize}\\item One\\end{itemize}\n",
        ),
    ] {
        let root =
            std::env::temp_dir().join(format!("rtex-new-boundary-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let setup = if name.starts_with("display-") {
            "\\usepackage{titlesec}\n\\titleformat{\\paragraph}[hang]{\\normalsize\\bfseries}{\\theparagraph}{1em}{}\n"
        } else {
            ""
        };
        let spacing = if name == "display-unindented" {
            "\\titlespacing*{\\paragraph}{0pt}{2ex}{1em}\n"
        } else {
            ""
        };
        let text = format!(
            "\\documentclass{{article}}\n{setup}{spacing}\\begin{{document}}\n{prefix}\n\\end{{document}}\n"
        );
        std::fs::write(root.join("main.tex"), &text).unwrap();
        let mut cfg = SessionConfig::new(&root, "main.tex");
        cfg.build_dir = root.join("build");
        cfg.debounce = Duration::from_millis(100);
        let s = Session::open(cfg).unwrap();
        let mut observer = rtex_core::replay::Observer::new();
        assert!(
            observer.settle(&s, 1, Duration::from_secs(120)),
            "{name}: initial layout"
        );
        let mut at = text.find("\\end{document}").unwrap();
        // Build the paragraph as an editor does, including a source newline inside it.
        // A single paste of a complete paragraph misses repeated NoContext refusals.
        let mut id = None;
        let mut revision = 1;
        let mut last_edit_id = 0;
        for addition in ["\n", "Z", "e", "bra", "\n", "new line."] {
            let edit = s
                .apply_edit(
                    "main.tex",
                    Edit {
                        start_byte: at,
                        end_byte: at,
                        text: addition.into(),
                    },
                )
                .unwrap();
            at += addition.len();
            revision = edit.source_revision;
            last_edit_id = edit.edit_id;
            if addition.trim().is_empty() && id.is_none() {
                continue;
            }
            assert_eq!(edit.routed, "fast", "{name}: {:?}", edit.reasons);
            let par_id = *id.get_or_insert_with(|| {
                *edit
                    .outcome
                    .added
                    .first()
                    .or(edit.outcome.touched.first())
                    .unwrap()
            });
            let (event, others) = s.wait_for(Duration::from_secs(30), |e| matches!(e,
                Event::ParagraphUpdate { par_id: p, edit_id, .. } if *p == par_id && *edit_id == edit.edit_id));
            for e in others {
                observer.absorb(e);
            }
            if let Some(e) = event.clone() {
                observer.absorb(e);
            }
            assert!(
                matches!(event, Some(Event::ParagraphUpdate { status, dl, .. })
                if status.starts_with("ok") && !dl.lines.is_empty()),
                "{name}: no live result"
            );
        }
        let par_id = id.unwrap();
        let (event, others) = s.wait_for(Duration::from_secs(120), |e| {
            matches!(e,
            Event::LayoutUpdate { versions, convergence: Convergence::Converged, .. }
            if versions.source_revision >= revision)
        });
        for e in others {
            observer.absorb(e);
        }
        if let Some(e) = event.clone() {
            observer.absorb(e);
        }
        let Some(Event::LayoutUpdate {
            eligible_paragraphs,
            ..
        }) = event
        else {
            panic!("{name}: no layout")
        };
        assert!(eligible_paragraphs.contains(&par_id), "{name}: new paragraph {par_id:?} never became eligible: {eligible_paragraphs:?}; spans {:?}; units {:?}", s.spans("main.tex"), s.layout_units());
        let served = observer.updates[&(last_edit_id, par_id)].clone();
        assert_eq!(
            observer.judge(&served).verdict,
            rtex_core::replay::Verdict::Match,
            "{name}: borrowed result differs from capture: {:?}",
            observer.judge(&served).details
        );
        let edit = s
            .apply_edit(
                "main.tex",
                Edit {
                    start_byte: at,
                    end_byte: at,
                    text: " More.".into(),
                },
            )
            .unwrap();
        assert_eq!(
            edit.routed, "fast",
            "{name}: captured edit {:?}",
            edit.reasons
        );
        let (event, others) = s.wait_for(Duration::from_secs(30), |e| matches!(e,
            Event::ParagraphUpdate { par_id: p, edit_id, .. } if *p == par_id && *edit_id == edit.edit_id));
        assert!(
            matches!(&event, Some(Event::ParagraphUpdate { status, dl, .. })
            if status.starts_with("ok") && !dl.lines.is_empty()),
            "{name}: captured result {event:?}"
        );
        for e in others {
            observer.absorb(e);
        }
        if let Some(e) = event {
            observer.absorb(e);
        }
        let served = observer.updates[&(edit.edit_id, par_id)].clone();
        assert!(observer.settle(&s, edit.source_revision, Duration::from_secs(120)));
        assert_eq!(
            observer.judge(&served).verdict,
            rtex_core::replay::Verdict::Match,
            "{name}: captured live edit differs from full layout: {:?}",
            observer.judge(&served).details
        );
        s.close();
        let _ = std::fs::remove_dir_all(root);
    }
}
