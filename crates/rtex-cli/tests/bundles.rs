//! Debug bundles as regression tests (`rtex bundle`, src/bundle.rs).
//!
//! - Every bundle under `fixtures/bundles/` replays to `ok`/`ok_degraded`: a failure it
//!   recorded stays fixed.
//! - The machinery itself: a session with the engine's internal-error seam
//!   (`RTEX_TEST_FINISH_ERROR`) writes a bundle; replayed with the seam it fails again (exit 1),
//!   without it it passes, and its anonymized copy passes too and has none of the document's
//!   words.
//!
//! Its own test binary (the seam is a process environment variable). Skipped without lualatex.

use rtex_core::{Edit, Event, Session, SessionConfig};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const MARKER: &str = "ZZBUNDLEZZ";

fn rtex() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rtex"));
    c.env_remove("RTEX_TEST_FINISH_ERROR");
    c
}

fn replay(dir: &Path, seam: bool) -> (bool, String) {
    let mut c = rtex();
    c.args(["bundle", "replay"]).arg(dir);
    if seam {
        c.env("RTEX_TEST_FINISH_ERROR", MARKER);
    }
    let o = c.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.success(), text)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn bundles_replay_and_stay_fixed() {
    if let Err(e) = rtex_core::texlive::TexLive::discover() {
        eprintln!("SKIP: {e}");
        return;
    }
    // the recorded bundles
    let dir = repo_root().join("fixtures/bundles");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.path().join("source.tex").is_file() {
                let (ok, out) = replay(&e.path(), false);
                assert!(ok, "{} fails again: {out}", e.path().display());
            }
        }
    }

    // a bundle written by a session
    let root = std::env::temp_dir().join(format!("rtex-bundles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("main.tex"),
        "\\documentclass{article}\n\\title{Confidential Report}\n\\begin{document}\nA warm-up paragraph about apricots.\n\nPrivate musings about the harbour, \\emph{quietly} kept.\n\\end{document}\n",
    )
    .unwrap();
    // SAFETY: set before any server is spawned; this binary holds only this test
    unsafe { std::env::set_var("RTEX_TEST_FINISH_ERROR", MARKER) };
    let mut cfg = SessionConfig::new(&project, "main.tex");
    cfg.build_dir = root.join("build");
    cfg.debug_dir = Some(root.join("debug"));
    let s = Session::open(cfg).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    'layout: loop {
        assert!(Instant::now() < deadline, "no layout");
        for ev in s.poll(Duration::from_millis(100)) {
            if matches!(ev, Event::LayoutUpdate { .. }) {
                break 'layout;
            }
        }
    }
    let src = s.document_text("main.tex").unwrap();
    let at = src.find("the harbour").unwrap() + "the harbour".len();
    let r = s
        .apply_edit(
            "main.tex",
            Edit {
                start_byte: at,
                end_byte: at,
                text: format!(" {MARKER}"),
            },
        )
        .unwrap();
    assert_eq!(r.routed, "fast", "{:?}", r.reasons);
    let debug = root.join("debug");
    let deadline = Instant::now() + Duration::from_secs(30);
    let bundle = loop {
        s.poll(Duration::from_millis(100));
        let found = std::fs::read_dir(&debug).ok().and_then(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .find(|p| p.join("report.json").is_file() && p.join("rtex-preamble.tex").is_file())
        });
        if let Some(b) = found {
            break b;
        }
        assert!(
            Instant::now() < deadline,
            "no bundle in {}",
            debug.display()
        );
    };
    s.close();
    unsafe { std::env::remove_var("RTEX_TEST_FINISH_ERROR") };

    let (ok, out) = replay(&bundle, true);
    assert!(!ok, "with the seam the bundle must fail again: {out}");
    let (ok, out) = replay(&bundle, false);
    assert!(ok, "without the seam the bundle must pass: {out}");

    let anon = root.join("anon");
    let o = rtex()
        .args(["bundle", "anonymize"])
        .arg(&bundle)
        .arg(&anon)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let mut all = String::new();
    for e in std::fs::read_dir(&anon).unwrap().flatten() {
        all.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
    }
    for w in [
        "Confidential",
        "Report",
        "Private",
        "musings",
        "harbour",
        "quietly",
    ] {
        assert!(!all.contains(w), "{w} survived anonymization");
    }
    assert!(!anon.join(format!("{}.log", "rtex-serve-g1")).exists());
    let (ok, out) = replay(&anon, false);
    assert!(ok, "the anonymized bundle must replay: {out}");
    let _ = std::fs::remove_dir_all(&root);
}
