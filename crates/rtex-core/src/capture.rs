//! Running an instrumented full LuaLaTeX pass (`rtex-capture`) and reading its results.

use crate::texlive::TexLive;
use anyhow::{bail, Context, Result};
use rtex_dl::{DisplayList, Sp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Nfss {
    pub enc: Option<String>,
    pub family: Option<String>,
    pub series: Option<String>,
    pub shape: Option<String>,
    pub size: Option<String>,
    pub baselineskip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParaBegin {
    pub line: i64,
    pub nest: i64,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub font: Option<i64>,
    #[serde(default)]
    pub nfss: Nfss,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub mathversion: Option<String>,
    /// `\if@nobreak` at paragraph start (true right after a heading).
    #[serde(default)]
    pub nobreak: bool,
    /// Unit the paragraph belongs to.
    #[serde(default)]
    pub unit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Placement {
    pub page: i64,
    /// 1-based row index within the unit.
    #[serde(default)]
    pub row: i64,
    /// Capture paragraph sequence number of the row's line (0 for display rows), and its line index.
    #[serde(default)]
    pub par: i64,
    #[serde(default)]
    pub line: i64,
    pub x: Sp,
    pub y: Sp,
    pub w: Sp,
    pub h: Sp,
    pub d: Sp,
}

/// A fast-path unit as seen by the capture run: a top-level paragraph, a block environment
/// or a heading, with the state in force when it began and where its rows were shipped.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapturedUnit {
    pub uid: i64,
    /// "par" | "env" | "heading"
    pub kind: String,
    /// Environment or sectioning command name for env/heading units.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    pub begin_line: i64,
    #[serde(default)]
    pub end_line: Option<i64>,
    #[serde(default)]
    pub nest: i64,
    #[serde(default)]
    pub seqs: Vec<i64>,
    #[serde(default)]
    pub placements: Vec<Placement>,
    #[serde(default)]
    pub rows: i64,
    /// Counter values that changed since the previous unit (delta encoding; see `abs_counters`).
    /// An empty Lua table arrives as `[]`, hence the loose type.
    #[serde(default)]
    pub counters: serde_json::Value,
    /// Counters the unit advanced: name → value at the unit's end (absent when none).
    #[serde(default)]
    pub advanced: Option<BTreeMap<String, i64>>,
    /// `\the<counter>` bodies that changed since the previous unit (delta; see `abs_thefmt`).
    #[serde(default)]
    pub thefmt: serde_json::Value,
    /// Absolute `\the<counter>` bodies at unit begin (filled by `CaptureJson::finalize`).
    #[serde(skip)]
    pub abs_thefmt: BTreeMap<String, String>,
    /// Meanings of the macros the body (re)defines that changed since the previous unit (delta).
    #[serde(default)]
    pub macros: serde_json::Value,
    /// Absolute meanings at unit begin (filled by `CaptureJson::finalize`).
    #[serde(skip)]
    pub abs_macros: BTreeMap<String, String>,
    #[serde(default)]
    pub everypar: String,
    #[serde(default)]
    pub nobreak: bool,
    #[serde(default)]
    pub afterindent: bool,
    #[serde(default)]
    pub noskipsec: bool,
    #[serde(default)]
    pub nfss: Nfss,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub ints: BTreeMap<String, i64>,
    #[serde(default)]
    pub dims: BTreeMap<String, i64>,
    #[serde(default)]
    pub glues: BTreeMap<String, Vec<f64>>,
    #[serde(default)]
    pub parshape: Option<serde_json::Value>,
    /// Absolute counter values at unit begin (filled by `CaptureJson::finalize`).
    #[serde(skip)]
    pub abs_counters: BTreeMap<String, i64>,
}

/// A paragraph as seen by `pre_linebreak_filter` in the instrumented run, with its context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapturedParagraph {
    pub seq: i64,
    pub groupcode: String,
    pub nest: i64,
    pub end_line: i64,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub lines: Option<i64>,
    #[serde(default)]
    pub ints: BTreeMap<String, i64>,
    #[serde(default)]
    pub dims: BTreeMap<String, i64>,
    #[serde(default)]
    pub glues: BTreeMap<String, Vec<f64>>,
    #[serde(default)]
    pub parshape: Option<serde_json::Value>,
    #[serde(default)]
    pub everypar: String,
    #[serde(default)]
    pub flags: BTreeMap<String, i64>,
    #[serde(default)]
    pub begin: Option<ParaBegin>,
    #[serde(default)]
    pub placements: Option<Vec<Placement>>,
}

impl CapturedParagraph {
    pub fn is_top_level(&self) -> bool {
        self.groupcode.is_empty() && self.nest == 1
    }
    /// Source line range (1-based, inclusive start, inclusive end as reported by the filter).
    pub fn start_line(&self) -> Option<i64> {
        self.begin.as_ref().map(|b| b.line)
    }
    /// The context the fast server needs (serialized as the `ctx` of a `context` request).
    pub fn context_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ints": self.ints, "dims": self.dims, "glues": self.glues,
            "parshape": self.parshape, "everypar": self.everypar,
            "begin": self.begin,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CaptureJson {
    pub version: i64,
    pub jobname: String,
    pub pages: i64,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub paragraphs: Vec<CapturedParagraph>,
    #[serde(default)]
    pub units: Vec<CapturedUnit>,
    /// LaTeX counter names known to the document (from `\cl@@ckpt`).
    #[serde(default)]
    pub counters: Vec<String>,
    /// Where this pass drew each picture environment (`file:line` → position), for the
    /// picture cache. An empty Lua table arrives as `[]`, hence the loose type.
    #[serde(default)]
    pub pics: serde_json::Value,
    /// Keys of cached pictures whose skipped body did not end on the line the source scan
    /// predicted (the cache forgets them; see `piccache::PicCache::absorb`).
    #[serde(default)]
    pub pic_mismatch: Vec<String>,
}

impl CaptureJson {
    /// `pics` as a map (empty when the pass recorded none).
    /// Entry by entry: one picture the capture describes in a way this version cannot read
    /// costs that picture its cache entry, not every picture of the pass.
    pub fn recorded_pics(&self) -> BTreeMap<String, crate::piccache::RecordedPic> {
        let serde_json::Value::Object(m) = &self.pics else {
            return BTreeMap::new();
        };
        m.iter()
            .filter_map(|(k, v)| match serde_json::from_value(v.clone()) {
                Ok(r) => Some((k.clone(), r)),
                Err(e) => {
                    log::warn!("picture {k}: unreadable record ({e})");
                    None
                }
            })
            .collect()
    }

    /// Reconstruct absolute counter values from the per-unit deltas (units are in document order).
    pub fn finalize(&mut self) {
        let mut acc: BTreeMap<String, i64> = BTreeMap::new();
        let mut fmt: BTreeMap<String, String> = BTreeMap::new();
        let mut mac: BTreeMap<String, String> = BTreeMap::new();
        for u in &mut self.units {
            if let serde_json::Value::Object(m) = &u.counters {
                for (k, v) in m {
                    if let Some(n) = v.as_i64() {
                        acc.insert(k.clone(), n);
                    }
                }
            }
            if let serde_json::Value::Object(m) = &u.thefmt {
                for (k, v) in m {
                    if let Some(b) = v.as_str() {
                        fmt.insert(k.clone(), b.to_string());
                    }
                }
            }
            if let serde_json::Value::Object(m) = &u.macros {
                for (k, v) in m {
                    if let Some(b) = v.as_str() {
                        mac.insert(k.clone(), b.to_string());
                    }
                }
            }
            u.abs_counters = acc.clone();
            u.abs_thefmt = fmt.clone();
            u.abs_macros = mac.clone();
        }
    }
    pub fn unit(&self, uid: i64) -> Option<&CapturedUnit> {
        self.units.iter().find(|u| u.uid == uid)
    }
}

#[derive(Debug)]
pub struct CaptureResult {
    pub out_dir: PathBuf,
    pub jobname: String,
    pub json: CaptureJson,
    pub pdf: PathBuf,
    pub log: PathBuf,
    pub wall: std::time::Duration,
    pub exit_ok: bool,
    /// Drawings of the pictures the pass could take from the picture cache (by `file:line`
    /// key): [`CaptureResult::page`] splices them back in place of the cached regions.
    pub pic_fragments: BTreeMap<String, crate::piccache::Fragment>,
}

/// LuaTeX's last word after a fatal error (an emergency stop, 100 errors, a runaway argument at
/// the end of the file).
const NO_PDF: &str = "no output PDF file produced";

/// True when the end of `log` says LuaTeX stopped on a fatal error.
pub fn log_says_fatal(log: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log) else { return false };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(4096)));
    let mut tail = Vec::new();
    let _ = f.read_to_end(&mut tail);
    String::from_utf8_lossy(&tail).contains(NO_PDF)
}

impl CaptureResult {
    /// The pass ended on a fatal error: LuaTeX produced no PDF, although it may have shipped
    /// pages (the capture has them) and left a partial file behind (opened at the first
    /// shipout, never finished).
    pub fn fatal(&self) -> bool {
        !self.pdf.exists() || log_says_fatal(&self.log)
    }

    pub fn page(&self, n: i64) -> Result<DisplayList> {
        let p = self
            .out_dir
            .join(format!("{}.rtex-page{}.json", self.jobname, n));
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let mut dl = DisplayList::from_json(&s)?;
        crate::piccache::substitute(&mut dl, &self.pic_fragments);
        Ok(dl)
    }
    pub fn paragraph(&self, seq: i64) -> Option<&CapturedParagraph> {
        self.json.paragraphs.iter().find(|p| p.seq == seq)
    }
}

/// Run `lualatex '\RequirePackage{rtex-capture}\input{main}'` in `src_dir`, writing to `out_dir`.
/// `instrumented=false` runs the same build without the capture package (clean reference).
pub fn run_capture(
    tl: &TexLive,
    src_dir: &Path,
    main: &str,
    out_dir: &Path,
    instrumented: bool,
) -> Result<CaptureResult> {
    run_capture_with(tl, src_dir, main, out_dir, instrumented, "")
}

/// `run_capture` with extra unit environments (comma separated, `$RTEX_UNIT_ENVS`): theorem-like
/// environments the capture should treat as units.
pub fn run_capture_with(
    tl: &TexLive,
    src_dir: &Path,
    main: &str,
    out_dir: &Path,
    instrumented: bool,
    unit_envs: &str,
) -> Result<CaptureResult> {
    run_capture_until(tl, src_dir, main, out_dir, instrumented, unit_envs, &never_stop)
}

/// `run_capture_with` that `stop` may end early (see [`StopCheck`]).
pub fn run_capture_until(
    tl: &TexLive,
    src_dir: &Path,
    main: &str,
    out_dir: &Path,
    instrumented: bool,
    unit_envs: &str,
    stop: StopCheck,
) -> Result<CaptureResult> {
    let (mut cmd, jobname, out_dir) =
        capture_command(tl, src_dir, main, out_dir, instrumented, unit_envs)?;
    cmd.arg(if instrumented {
        format!("\\RequirePackage{{rtex-capture}}\\input{{{main}}}")
    } else {
        format!("\\input{{{main}}}")
    });
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let t0 = Instant::now();
    let mut child = cmd.spawn().context("spawning lualatex")?;
    let (status, stdout) = wait_child(&mut child, t0, stop)?;
    collect_capture(
        &out_dir,
        &jobname,
        instrumented,
        status.success(),
        status.code(),
        &stdout,
        t0.elapsed(),
    )
}

/// Whether a running pass is stopped: called every 100 ms with the time since the pass
/// started; `Some(reason)` kills the engine and the pass fails with `reason`. A document that
/// loops forever (`\def\x{\x}\x`) never ends its pass otherwise.
pub type StopCheck<'a> = &'a dyn Fn(std::time::Duration) -> Option<String>;

/// The error of a pass a [`StopCheck`] ended, with its reason.
#[derive(Debug)]
pub struct Stopped(pub String);

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Stopped {}

/// A [`StopCheck`] that lets every pass run to its end (offline tools, tests).
pub fn never_stop(_: std::time::Duration) -> Option<String> {
    None
}

/// Wait for a pass's engine, reading its stdout, and kill it when `stop` says so. The output is
/// read on a thread: its end (the engine exiting) ends the wait at once.
pub fn wait_child(
    child: &mut std::process::Child,
    t0: Instant,
    stop: StopCheck,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut so) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = std::io::Read::read_to_end(&mut so, &mut v);
            let _ = tx.send(v);
        });
    } else {
        let _ = tx.send(Vec::new());
    }
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(out) => return Ok((child.wait().context("waiting for lualatex")?, out)),
            // the reader is gone without its output: the engine's stdout closed
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Ok((child.wait().context("waiting for lualatex")?, Vec::new()))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(reason) = stop(t0.elapsed()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Stopped(reason).into());
        }
    }
}

/// The lualatex command for a pass over `src_dir`, without its final `\input` argument.
/// Returns (command, jobname, canonical out_dir).
pub fn capture_command(
    tl: &TexLive,
    src_dir: &Path,
    main: &str,
    out_dir: &Path,
    instrumented: bool,
    unit_envs: &str,
) -> Result<(Command, String, std::path::PathBuf)> {
    std::fs::create_dir_all(out_dir)?;
    let out_dir = out_dir.canonicalize()?;
    crate::background::mirror_dirs(src_dir, &out_dir, 0)?;
    let jobname = Path::new(main)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("main")
        .to_string();
    let mut cmd = tl.lualatex_cmd(src_dir);
    cmd.arg("-interaction=nonstopmode")
        .arg("-file-line-error")
        .arg(format!("--jobname={jobname}"))
        .arg(format!("--output-directory={}", out_dir.display()))
        .env("RTEX_CAPTURE_DIR", &out_dir)
        .env("RTEX_UNIT_ENVS", unit_envs);
    let _ = instrumented;
    Ok((cmd, jobname, out_dir))
}

/// Read the outputs of a finished pass (log, PDF, capture JSON when instrumented).
pub fn collect_capture(
    out_dir: &Path,
    jobname: &str,
    instrumented: bool,
    exit_ok: bool,
    exit_code: Option<i32>,
    stdout: &[u8],
    wall: std::time::Duration,
) -> Result<CaptureResult> {
    let log = out_dir.join(format!("{jobname}.log"));
    let pdf = out_dir.join(format!("{jobname}.pdf"));
    let json = if instrumented {
        let jp = out_dir.join(format!("{jobname}.rtex.json"));
        // written at \end{document}: a pass that stopped earlier (a fatal error) leaves the file
        // of an earlier pass in the same directory, which is not this pass's capture
        let started = std::time::SystemTime::now() - wall - std::time::Duration::from_secs(1);
        let fresh = std::fs::metadata(&jp)
            .and_then(|m| m.modified())
            .map(|t| t >= started)
            .unwrap_or(false);
        if !fresh {
            bail!(
                "capture run produced no {}; lualatex exit {:?}\n{}",
                jp.display(),
                exit_code,
                String::from_utf8_lossy(stdout)
                    .chars()
                    .rev()
                    .take(2000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            );
        }
        {
            let mut j: CaptureJson = serde_json::from_str(&std::fs::read_to_string(&jp)?)
                .with_context(|| format!("parsing capture json {}", jp.display()))?;
            j.finalize();
            j
        }
    } else {
        CaptureJson {
            jobname: jobname.to_string(),
            ..Default::default()
        }
    };
    Ok(CaptureResult {
        out_dir: out_dir.to_path_buf(),
        jobname: jobname.to_string(),
        json,
        pdf,
        log,
        wall,
        exit_ok,
        pic_fragments: BTreeMap::new(),
    })
}
