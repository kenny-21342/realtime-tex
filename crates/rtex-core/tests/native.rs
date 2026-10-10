//! Native drawing of TikZ pages in the live session: a layout whose pictures come from the
//! picture cache still carries their drawing (the cache keeps each picture's display-list
//! fragment), and what a page draws natively is the same whether its pictures were typeset in
//! the pass or taken from the cache. Skipped without lualatex.

use rtex_core::background::{run_pass, BibTool};
use rtex_core::texlive::TexLive;
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use rtex_dl::gfx::{native_graphics, GfxOp, NativePage};
use rtex_dl::{DisplayList, Item};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn corpus() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/corpus/pictures/main.tex")
}

/// Pages of the next converged layout, with what each `LayoutUpdate` said about native drawing.
fn converged(s: &Session, pages: &mut BTreeMap<i64, (DisplayList, Option<NativePage>)>, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        assert!(
            Instant::now() < deadline,
            "no converged layout within {secs} s"
        );
        for ev in s.poll(Duration::from_millis(200)) {
            if let Event::LayoutUpdate {
                pages_changed,
                convergence,
                ..
            } = ev
            {
                for p in pages_changed {
                    pages.insert(p.page, (p.dl, p.native));
                }
                if convergence == Convergence::Converged {
                    return;
                }
            }
        }
    }
}

fn cached_pictures(dl: &DisplayList) -> usize {
    dl.lines
        .iter()
        .flat_map(|l| l.items.iter())
        .filter(|i| matches!(i, Item::Unsupported { kind, .. } if kind == "cached_picture"))
        .count()
}

/// The paint and shading operations of a page (clip bookkeeping and item references aside),
/// rounded: two drawings of the same page agree on these.
fn paints(n: &NativePage) -> Vec<String> {
    n.ops
        .iter()
        .filter_map(|o| match o {
            GfxOp::Paint {
                ctm,
                path,
                fill,
                stroke,
                even_odd,
                ..
            } => Some(format!(
                "paint {:?} {:?} {:?} {:?} {}",
                ctm.iter().map(|v| v.round() as i64).collect::<Vec<_>>(),
                path,
                fill,
                stroke,
                even_odd
            )),
            GfxOp::Clip { ctm, path, .. } => Some(format!(
                "clip {:?} {:?}",
                ctm.iter().map(|v| v.round() as i64).collect::<Vec<_>>(),
                path
            )),
            GfxOp::Shade {
                ctm,
                bbox,
                coords,
                stops,
                ..
            } => Some(format!(
                "shade {:?} {:?} {:?} {:?}",
                ctm.iter().map(|v| v.round() as i64).collect::<Vec<_>>(),
                bbox,
                coords,
                stops
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn cached_pictures_are_drawn_natively_like_typeset_ones() {
    let tl = match TexLive::discover() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("SKIP: {e}");
            return;
        }
    };
    let root = std::env::temp_dir().join(format!("rtex-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::copy(corpus(), project.join("main.tex")).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    let s = Session::open(cfg).unwrap();
    let mut pages = BTreeMap::new();
    converged(&s, &mut pages, 180);
    // an edit that moves everything but changes no picture: every page is new, and the next run
    // takes the pictures from the cache
    let doc = s.document_text("main.tex").unwrap();
    let at = doc.find("\\begin{document}").unwrap() + "\\begin{document}\n".len();
    s.apply_edit(
        "main.tex",
        Edit {
            start_byte: at,
            end_byte: at,
            text: "A new first paragraph that moves the rest down.\n\n".into(),
        },
    )
    .unwrap();
    s.request_layout();
    pages.clear();
    converged(&s, &mut pages, 180);
    let src = s.document_text("main.tex").unwrap();
    s.close();
    // the pass really took pictures from the cache: its own page lists (before the drawings are
    // put back) hold cached regions
    let mut raw_cached = 0;
    for dir in ["pass-0", "pass-1"] {
        for ent in std::fs::read_dir(root.join("build/bg").join(dir))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.contains(".rtex-page") {
                let text = std::fs::read_to_string(ent.path()).unwrap();
                raw_cached += text.matches("\"cached_picture\"").count();
            }
        }
    }
    assert!(
        raw_cached > 0,
        "the last pass took no picture from the cache"
    );

    // the same source captured without the cache: every picture typeset
    let clean_dir = root.join("clean");
    std::fs::create_dir_all(clean_dir.join("src")).unwrap();
    std::fs::write(clean_dir.join("src/main.tex"), &src).unwrap();
    let clean = run_pass(
        &tl,
        &clean_dir.join("src"),
        "main.tex",
        &clean_dir.join("out"),
        5,
        BibTool::None,
        true,
    )
    .unwrap();

    let mut drawn_from_cache = 0;
    for (n, (dl, native)) in &pages {
        let flags = dl.flags_map();
        assert!(
            !flags.contains_key("pic_cache"),
            "page {n}: a cached picture without its drawing ({flags:?})"
        );
        assert_eq!(cached_pictures(dl), 0, "page {n}");
        let typeset = clean.capture.page(*n).unwrap();
        let want = native_graphics(&typeset);
        match (native, want) {
            (Some(got), Ok(want)) => {
                assert_eq!(
                    paints(got),
                    paints(&want),
                    "page {n}: the cached drawing differs from the typeset one"
                );
                assert_eq!(got.transforms.len(), want.transforms.len(), "page {n}");
                drawn_from_cache += 1;
            }
            (None, Err(_)) => {}
            (got, want) => panic!(
                "page {n}: native {:?} vs typeset {:?}",
                got.is_some(),
                want.map(|_| ())
            ),
        }
    }
    assert!(drawn_from_cache > 0, "no page was drawn natively");
    // the pictures put back are listed by key and place (hosts copy them into live units)
    let spots: Vec<_> = pages
        .values()
        .flat_map(|(dl, _)| dl.pictures.iter())
        .collect();
    assert!(!spots.is_empty(), "no picture spot on the pages");
    assert!(
        spots
            .iter()
            .all(|p| p.key.starts_with("main.tex:") && p.width > 0 && p.height > 0),
        "{spots:?}"
    );
    eprintln!("cached regions in the pass: {raw_cached}; pages drawn natively: {drawn_from_cache}");
    let _ = std::fs::remove_dir_all(&root);
}
