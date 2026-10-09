//! `cargo run --release -p rtex-core --example native_session -- PROJECT MAIN OUT [FIND]`
//!
//! Opens a session (picture cache on), waits for a converged layout, makes one text edit (a
//! word inserted before the first occurrence of FIND, default: after \begin{document}) so the
//! next run takes unchanged pictures from the cache, waits again, and writes every page of that
//! layout as OUT/page<N>.json plus the layout's PDF as OUT/layout.pdf: the input of
//! scripts/gfx_compare.py and scripts/gfx_shading_check.py (via the gfx_dump example). Prints how
//! many pages are drawn natively and how many cached pictures the pass used.
use rtex_core::{Convergence, Edit, Event, Session, SessionConfig};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (project, main, out) = (&a[1], &a[2], std::path::PathBuf::from(&a[3]));
    let find = a.get(4).cloned();
    std::fs::create_dir_all(&out)?;
    let mut cfg = SessionConfig::new(project, main.clone());
    cfg.build_dir = out.join("build");
    let s = Session::open(cfg)?;
    let wait = |pages: &mut BTreeMap<i64, rtex_core::session::PageUpdate>| -> anyhow::Result<Option<std::path::PathBuf>> {
        let deadline = Instant::now() + Duration::from_secs(1800);
        loop {
            anyhow::ensure!(Instant::now() < deadline, "no converged layout");
            for ev in s.poll(Duration::from_millis(500)) {
                if let Event::LayoutUpdate { pages_changed, convergence, pdf_fallback, .. } = ev {
                    for p in pages_changed {
                        pages.insert(p.page, p);
                    }
                    match convergence {
                        Convergence::Converged => return Ok(pdf_fallback),
                        Convergence::PassLimitReached { reasons, .. } => anyhow::bail!("run ended without converging: {reasons:?}"),
                        _ => {}
                    }
                }
            }
        }
    };
    let t0 = Instant::now();
    let mut pages = BTreeMap::new();
    wait(&mut pages)?;
    eprintln!("first layout converged in {:.1} s", t0.elapsed().as_secs_f64());
    let doc = s.document_text(main).unwrap();
    let at = match &find {
        Some(f) => doc.find(f.as_str()).expect("FIND not in the document"),
        None => doc.find("\\begin{document}").unwrap() + "\\begin{document}".len(),
    };
    // a paragraph of its own before everything: every page moves, no picture changes
    s.apply_edit(main, Edit { start_byte: at, end_byte: at, text: "\n\nInserted paragraph.\n\n".into() })?;
    s.request_layout();
    let t1 = Instant::now();
    let pdf = wait(&mut pages)?;
    eprintln!("edited layout converged in {:.1} s", t1.elapsed().as_secs_f64());
    s.close();
    let (mut native, mut degraded) = (0, 0);
    for (n, p) in &pages {
        std::fs::write(out.join(format!("page{n}.json")), serde_json::to_string(&p.dl)?)?;
        if !p.exact {
            degraded += 1;
        }
        if p.native.is_some() {
            native += 1;
        }
    }
    if let Some(pdf) = pdf {
        std::fs::copy(pdf, out.join("layout.pdf"))?;
    }
    // cached regions the last pass used (its own page lists, before the drawings went back)
    let mut cached = 0;
    for dir in ["pass-0", "pass-1"] {
        for e in std::fs::read_dir(out.join("build/bg").join(dir)).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().contains(".rtex-page") {
                cached += std::fs::read_to_string(e.path())?.matches("\"cached_picture\"").count();
            }
        }
    }
    println!("pages {} degraded {} native {} cached pictures in the last pass {}", pages.len(), degraded, native, cached);
    Ok(())
}
