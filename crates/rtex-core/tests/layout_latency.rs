//! Whole-page updates must make progress while typing changes line breaking or page layout.
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use std::time::{Duration, Instant};

fn settled(s: &Session) -> (u64, i64) {
    let end = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(Instant::now() < end, "no settled layout");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate {
                convergence: Convergence::Converged,
                versions,
                pages_total,
                ..
            } = e
            {
                return (versions.source_revision, pages_total);
            }
        }
    }
}

#[test]
fn reflowing_paragraphs_publish_pages_before_typing_stops() {
    continuous_reflow("paragraph", "Anchor words here.", true);
}

#[test]
fn body_edits_on_the_background_path_publish_pages_before_typing_stops() {
    continuous_reflow(
        "block-with-trailing-text",
        "Anchor \\begin{center}boxed\\end{center} words here.",
        false,
    );
}

fn continuous_reflow(name: &str, paragraph: &str, live: bool) {
    if rtex_core::texlive::TexLive::discover().is_err() {
        return;
    }
    let root =
        std::env::temp_dir().join(format!("rtex-reflow-latency-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text = format!("\\documentclass{{article}}\n\\usepackage[paperwidth=180pt,paperheight=160pt,margin=20pt]{{geometry}}\n\\pdfvariable suppressoptionalinfo 767\n\\begin{{document}}\n{paragraph}\n\nFollowing paragraph.\n\\end{{document}}\n");
    std::fs::write(root.join("main.tex"), text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(200);
    cfg.fast_budget = Duration::from_secs(5);
    let s = Session::open(cfg).unwrap();
    settled(&s);
    let initial = s.versions().layout_version;
    let mut page_progress = false;
    let mut reflow = false;
    let mut last_settled = None;
    for _ in 0..60 {
        let doc = s.document_text("main.tex").unwrap();
        let at = doc.find("Anchor").unwrap() + "Anchor".len();
        let result = s
            .apply_edit(
                "main.tex",
                Edit {
                    start_byte: at,
                    end_byte: at,
                    text: " more".into(),
                },
            )
            .unwrap();
        if !live {
            assert_eq!(result.routed, "background", "exercise direct body fallback");
        }
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            for e in s.poll(until.saturating_duration_since(Instant::now())) {
                match e {
                    Event::ParagraphUpdate {
                        pagination_stale: true,
                        ..
                    } => reflow = true,
                    Event::LayoutUpdate {
                        versions,
                        convergence,
                        pages_total,
                        ..
                    } if versions.layout_version > initial => {
                        page_progress = true;
                        if convergence == Convergence::Converged {
                            last_settled = Some((versions.source_revision, pages_total));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if live {
        assert!(reflow, "the edits must change the paragraph's line count");
    }
    assert!(
        page_progress,
        "a full-page update must arrive during continuous typing"
    );
    // A fast final pass may already have arrived during the last polling interval.
    let (revision, pages) = last_settled
        .filter(|(revision, _)| *revision == s.versions().source_revision)
        .unwrap_or_else(|| settled(&s));
    assert_eq!(
        revision,
        s.versions().source_revision,
        "the latest source must converge"
    );
    let final_text = s.document_text("main.tex").unwrap();
    assert_eq!(final_text.matches(" more").count(), 60);
    assert!(pages >= 2, "typing must move material onto another page");
    let clean = root.join("clean-source");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::write(clean.join("main.tex"), final_text).unwrap();
    let tl = rtex_core::texlive::TexLive::discover().unwrap();
    let fresh = rtex_core::background::run_pass(
        &tl,
        &clean,
        "main.tex",
        &root.join("clean-output"),
        5,
        rtex_core::background::BibTool::None,
        true,
    )
    .unwrap();
    assert!(fresh.aux_stable);
    assert_eq!(fresh.capture.json.pages, pages);
    assert_eq!(
        std::fs::read(root.join("build/bg/main.pdf")).unwrap(),
        std::fs::read(fresh.capture.pdf).unwrap(),
        "settled PDF must equal an independent clean build after reflow"
    );
    s.close();
    let _ = std::fs::remove_dir_all(root);
}
