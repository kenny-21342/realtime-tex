//! A live label edit keeps the picture's paths and the label's rotation. Its drawing is
//! independently compared with the next full page, after applying the live row's placement.
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use rtex_dl::gfx::{apply, GfxOp, NativePage, Seg};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn layout(
    s: &Session,
) -> (
    BTreeMap<i64, rtex_core::session::PageUpdate>,
    Vec<rtex_core::session::ParagraphPlacement>,
) {
    let end = Instant::now() + Duration::from_secs(120);
    let mut pages = BTreeMap::new();
    loop {
        assert!(Instant::now() < end, "no converged layout");
        for e in s.poll(Duration::from_millis(100)) {
            if let Event::LayoutUpdate {
                pages_changed,
                placements,
                convergence,
                ..
            } = e
            {
                for p in pages_changed {
                    pages.insert(p.page, p);
                }
                if convergence == Convergence::Converged {
                    return (pages, placements);
                }
            }
        }
    }
}

fn open(name: &str, preamble: &str, picture: &str) -> Option<(Session, String)> {
    if rtex_core::texlive::TexLive::discover().is_err() {
        return None;
    }
    let root =
        std::env::temp_dir().join(format!("rtex-live-picture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let text = format!("\\documentclass{{article}}\n{preamble}\n\\begin{{document}}\nBefore.\n\n{picture}\n\nAfter.\n\\end{{document}}\n");
    std::fs::write(root.join("main.tex"), &text).unwrap();
    let mut cfg = SessionConfig::new(&root, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.fast_budget = Duration::from_secs(5);
    cfg.debounce = Duration::from_millis(50);
    Some((Session::open(cfg).unwrap(), text))
}

fn edit_label(s: &Session, text: &str) -> Event {
    s.pause_background(true);
    let at = text.find("static").unwrap();
    let r = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at + 6,
                text: "dynamic".into(),
            },
        )
        .unwrap();
    assert_eq!(r.routed, "fast", "{:?}", r.reasons);
    s.wait_for(
        Duration::from_secs(60),
        |e| matches!(e, Event::ParagraphUpdate { edit_id, .. } if *edit_id == r.edit_id),
    )
    .0
    .expect("no live update")
}

/// Paint paths in page space (sp), rounded only to absorb PDF serialization's sub-sp noise.
fn paints(n: &NativePage, shift: impl Fn(Option<usize>) -> (f64, f64)) -> Vec<Vec<(i64, i64)>> {
    n.ops
        .iter()
        .filter_map(|op| {
            let GfxOp::Paint { at, ctm, path, .. } = op else {
                return None;
            };
            let (dx, dy) = shift(at.line);
            let mut points = vec![];
            for seg in path {
                let xy = match seg {
                    Seg::M { x, y } | Seg::L { x, y } => vec![(*x, *y)],
                    Seg::C {
                        x1,
                        y1,
                        x2,
                        y2,
                        x,
                        y,
                    } => vec![(*x1, *y1), (*x2, *y2), (*x, *y)],
                    Seg::Z => vec![],
                };
                for (x, y) in xy {
                    let (x, y) = apply(ctm, x, y);
                    points.push((
                        ((x + dx) / 64.0).round() as i64,
                        ((y + dy) / 64.0).round() as i64,
                    ));
                }
            }
            Some(points)
        })
        .collect()
}

fn check_live_geometry(name: &str, picture: &str, cached: usize) {
    let Some((s, text)) = open(name, "\\usepackage{tikz}", picture) else {
        return;
    };
    layout(&s);
    let Event::ParagraphUpdate {
        par_id,
        status,
        dl,
        native,
        ..
    } = edit_label(&s, &text)
    else {
        unreachable!()
    };
    assert!(status == "ok" || status == "ok_degraded", "{status}");
    assert_eq!(dl.pictures.len(), cached);
    let native = native.unwrap_or_else(|| panic!("live picture has no native drawing: flags {:?}, height {:?}, interpreter {:?}, dl {:?}", dl.flags, dl.page_height, rtex_dl::gfx::native_graphics(&dl), dl));
    assert!(dl
        .lines
        .iter()
        .flat_map(|l| &l.items)
        .any(|i| matches!(i, rtex_dl::Item::Literal { data, .. } if data != "data")));
    assert!(
        native
            .transforms
            .iter()
            .any(|(_, m)| m[1].abs() > 0.1 && m[2].abs() > 0.1),
        "rotated label has no transform"
    );
    assert!(paints(&native, |_| (0.0, 0.0)).len() >= 2);
    s.pause_background(false);
    let (pages, placements) = layout(&s);
    // A longer rotated label changes the box height. Compare at the new page's placement,
    // rather than the old layout's baseline used by the provisional live overlay.
    let fragments = &placements
        .iter()
        .find(|p| p.par_id == par_id)
        .unwrap()
        .fragments;
    let placed = |line: Option<usize>| {
        let line = line.unwrap();
        let f = fragments
            .iter()
            .find(|f| f.first_line as usize <= line + 1 && f.last_line as usize > line)
            .unwrap();
        let k = line + 1 - f.first_line as usize;
        (
            (f.xs[k] - dl.lines[line].x) as f64,
            (f.baselines[k] - dl.lines[line].y) as f64,
        )
    };
    let live = paints(&native, placed);
    let page = &pages[&fragments[0].page];
    let full = page
        .native
        .as_ref()
        .expect("background has no native picture");
    assert_eq!(
        live,
        paints(full, |_| (0.0, 0.0)),
        "live paths differ from the next page"
    );
    s.close();
}

#[test]
fn live_label_keeps_paths_and_rotation_at_the_background_placement() {
    check_live_geometry("tikz", "\\begin{tikzpicture}\n\\draw[->] (0,0) -- (3,0);\n\\draw[blue,thick] (0,0) -- (2,2) node[rotate=45,above] {static};\n\\end{tikzpicture}", 0);
}

#[test]
fn live_label_alongside_cached_pictures_restores_their_native_drawing() {
    check_live_geometry("mixed", "\\begin{center}\n\\begin{tikzpicture}\n\\draw (0,0) -- (1,1) node[rotate=45] {static};\n\\end{tikzpicture}\n\\hfill\n\\begin{tikzpicture}\n\\draw[red] (0,0) -- (1,0);\n\\end{tikzpicture}\n\\hfill\n\\begin{tikzpicture}\n\\draw[blue] (0,0) -- (0,1);\n\\end{tikzpicture}\n\\end{center}", 2);
}

#[test]
fn a_cached_earlier_label_version_has_native_drawing() {
    let Some((s, text)) = open(
        "backspace",
        "\\usepackage{tikz}",
        "\\begin{tikzpicture}\n\\draw (0,0) -- (1,1) node[rotate=45] {static};\n\\end{tikzpicture}",
    ) else {
        return;
    };
    layout(&s);
    let Event::ParagraphUpdate { native, .. } = edit_label(&s, &text) else {
        unreachable!()
    };
    assert!(native.is_some());
    s.pause_background(false);
    layout(&s);
    s.pause_background(true);
    let current = s.document_text("main.tex").unwrap();
    let at = current.find("dynamic").unwrap();
    let r = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at + 7,
                text: "static".into(),
            },
        )
        .unwrap();
    assert_eq!(r.routed, "fast");
    let (e, _) = s.wait_for(
        Duration::from_secs(60),
        |e| matches!(e, Event::ParagraphUpdate { edit_id, .. } if *edit_id == r.edit_id),
    );
    let Some(Event::ParagraphUpdate { native, dl, .. }) = e else {
        panic!("no live update")
    };
    assert!(
        native.is_some(),
        "a cached label version must not copy the current page's different label"
    );
    assert_eq!(
        dl.pictures.len(),
        1,
        "the previous label version should come from its recording"
    );
    s.close();
}

#[test]
fn live_pict2e_code_is_readable() {
    let Some((s,text)) = open("pict2e", "\\usepackage{pict2e}", "\\begin{picture}(100,60)\n\\put(0,0){\\line(1,1){40}}\n\\put(20,10){static}\n\\end{picture}") else { return; };
    layout(&s);
    let Event::ParagraphUpdate {
        native, status, dl, ..
    } = edit_label(&s, &text)
    else {
        unreachable!()
    };
    assert!(status == "ok" || status == "ok_degraded", "{status}");
    assert!(!paints(
        &native.unwrap_or_else(|| panic!(
            "pict2e has no native drawing: {:?}; {:?}",
            rtex_dl::gfx::native_graphics(&dl),
            dl
        )),
        |_| (0.0, 0.0)
    )
    .is_empty());
    s.close();
}

#[test]
fn unavailable_pattern_has_no_partial_native_drawing() {
    let Some((s,text)) = open("pattern", "\\usepackage{tikz}\n\\usetikzlibrary{patterns}", "\\begin{tikzpicture}\n\\fill[pattern=north east lines] (0,0) rectangle (2,1);\n\\node at (1,1) {static};\n\\end{tikzpicture}") else { return; };
    layout(&s);
    let Event::ParagraphUpdate { native, status, .. } = edit_label(&s, &text) else {
        unreachable!()
    };
    assert!(status == "ok" || status == "ok_degraded", "{status}");
    assert!(
        native.is_none(),
        "undeclared pattern must not produce partial drawing"
    );
    s.close();
}
