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

/// pict2e (picture mode, and diagbox's diagonal) writes its literals as token lists the display
/// list cannot read; the capture routes them through Lua like pgf's. pgf's tiling patterns are
/// drawn from their cells. Both pages are drawn natively.
#[test]
fn pict2e_and_tiling_patterns_are_drawn_natively() {
    if let Err(e) = TexLive::discover() {
        eprintln!("SKIP: {e}");
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-native-p2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let main = "\\documentclass{article}\n\\usepackage{pict2e,diagbox,tikz}\n\
                \\usetikzlibrary{patterns}\n\\begin{document}\n\
                \\begin{tabular}{|c|c|}\\hline\\diagbox{A}{B} & C\\\\\\hline\\end{tabular}\n\n\
                \\setlength{\\unitlength}{1mm}\\begin{picture}(40,20)\n\
                \\put(0,0){\\line(2,1){40}}\\put(20,10){\\circle{10}}\\qbezier(0,0)(20,20)(40,0)\n\
                \\end{picture}\n\\newpage\n\
                \\begin{tikzpicture}\\fill[pattern=north east lines, pattern color=red] (0,0) \
                rectangle (3,2);\\end{tikzpicture}\n\\end{document}\n";
    std::fs::write(project.join("main.tex"), main).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    let s = Session::open(cfg).unwrap();
    let mut pages = BTreeMap::new();
    converged(&s, &mut pages, 180);
    s.close();
    let native = |page: i64| {
        let (dl, n) = &pages[&page];
        let n = n.clone().unwrap_or_else(|| {
            panic!(
                "page {page} is not drawn natively: {:?}",
                native_graphics(dl).err()
            )
        });
        assert!(
            n.ops.iter().any(|o| matches!(o, GfxOp::Paint { .. })),
            "page {page} draws nothing"
        );
        n
    };
    native(1);
    let p2 = native(2);
    assert_eq!(p2.patterns.len(), 1, "{:?}", p2.patterns.keys());
}

/// graphicx clips a bitmap (`trim`/`viewport` with `clip`) by saving it in a box resource: the
/// capture writes the box's contents in its place, clipped to the form's box, so the page is
/// drawn natively with the image (not a `box_resource` the host cannot draw).
#[test]
fn clipped_images_are_drawn_from_their_box_resource() {
    if let Err(e) = TexLive::discover() {
        eprintln!("SKIP: {e}");
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-native-clip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    // a 1x1 PNG
    let png: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(project.join("dot.png"), png).unwrap();
    let main = "\\documentclass{article}\n\\usepackage{graphicx}\n\\begin{document}\n\
                Clipped: \\includegraphics[viewport=0 0 0.5 0.5,clip,width=2cm]{dot.png}\n\
                \\end{document}\n";
    std::fs::write(project.join("main.tex"), main).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    let s = Session::open(cfg).unwrap();
    let mut pages = BTreeMap::new();
    converged(&s, &mut pages, 180);
    s.close();
    let (dl, n) = &pages[&1];
    assert!(
        !dl.flags_map().contains_key("box_resource"),
        "{:?}",
        dl.flags
    );
    let items: Vec<&Item> = dl.lines.iter().flat_map(|l| l.items.iter()).collect();
    assert!(
        items.iter().any(|i| matches!(i, Item::Image { .. })),
        "the clipped image is not in the list"
    );
    let n = n
        .as_ref()
        .unwrap_or_else(|| panic!("not drawn natively: {:?}", native_graphics(dl).err()));
    assert!(
        n.ops.iter().any(|o| matches!(o, GfxOp::Clip { .. })),
        "no clip"
    );
}

/// pgf's fadings are soft masks drawing the fading's box (saved in the preamble, where the
/// fadings library declares them): the page is drawn natively, its paints naming the mask, and
/// each mask draws its fading's shading.
#[test]
fn fadings_are_drawn_as_soft_masks() {
    if let Err(e) = TexLive::discover() {
        eprintln!("SKIP: {e}");
        return;
    }
    let root = std::env::temp_dir().join(format!("rtex-native-fade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let main = "\\documentclass{article}\n\\usepackage{tikz}\n\\usetikzlibrary{fadings}\n\
                \\begin{document}\nFadings.\n\n\\begin{tikzpicture}\n\
                \\fill[red, path fading=east] (0,0) rectangle (3,1);\n\
                \\fill[blue, path fading=circle with fuzzy edge 20 percent] (4,0) rectangle (5,1);\n\
                \\end{tikzpicture}\n\\end{document}\n";
    std::fs::write(project.join("main.tex"), main).unwrap();
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debounce = Duration::from_millis(50);
    let s = Session::open(cfg).unwrap();
    let mut pages = BTreeMap::new();
    converged(&s, &mut pages, 180);
    s.close();
    let (dl, n) = &pages[&1];
    let n = n
        .as_ref()
        .unwrap_or_else(|| panic!("not drawn natively: {:?}", native_graphics(dl).err()));
    assert_eq!(n.masks.len(), 2, "{:?}", n.masks.keys());
    for (name, m) in &n.masks {
        assert!(
            m.ops.iter().any(|o| matches!(o, GfxOp::Shade { .. })),
            "mask {name} draws no shading"
        );
    }
    let masked = n
        .ops
        .iter()
        .filter(|o| matches!(o, GfxOp::Paint { mask: Some(_), .. }))
        .count();
    assert_eq!(masked, 2);
}
