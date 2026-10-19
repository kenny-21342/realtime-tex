//! Debug bundles (`SessionConfig::debug_dir`, docs/development.md "When the live engine fails")
//! as regression inputs.
//!
//! `replay` starts a live engine on the bundle's preamble copy, installs its context and sends
//! its source (and picture entries) once, as the session did when it failed. Exit status 0 when
//! the engine answers `ok`/`ok_degraded`, 1 when it fails again (an error result, a fatal
//! answer, no answer within the timeout). A bundle under `fixtures/bundles/` must replay with 0:
//! `crates/rtex-cli/tests/bundles.rs` runs them all.
//!
//! `anonymize` copies what `replay` needs (report, context, pictures, preamble, source) with the
//! document's words replaced by `x`es of the same length, outside commands, environment names,
//! keys and math, and leaves out the TeX log and trace (they quote the document). Read the
//! result before committing it: braced command arguments other than environment names keep
//! their text, since they can be what the engine failed on.

use anyhow::{bail, Context, Result};
use rtex_core::engine::FastServer;
use rtex_core::texlive::TexLive;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Outcome {
    pub status: String,
    pub detail: String,
    pub ms: u128,
}

pub fn replay(dir: &Path, project: Option<&Path>, timeout: Option<Duration>) -> Result<Outcome> {
    let tl = TexLive::discover()?;
    let read = |name: &str| std::fs::read_to_string(dir.join(name));
    let preamble = read("rtex-preamble.tex")
        .with_context(|| format!("{}: no rtex-preamble.tex", dir.display()))?;
    let source = read("source.tex").with_context(|| format!("{}: no source.tex", dir.display()))?;
    let pics = read("pics.json").unwrap_or_default();
    let report: serde_json::Value = read("report.json")
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let timeout = timeout.unwrap_or_else(|| {
        Duration::from_millis(
            report["compile_timeout_ms"]
                .as_u64()
                .unwrap_or(5000)
                .max(1000),
        )
    });
    let work = std::env::temp_dir().join(format!("rtex-bundle-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let cwd = project.unwrap_or(dir);
    let mut srv = FastServer::spawn(&tl, cwd, &work, &preamble, 1, None)
        .context("starting the live engine on the bundle's preamble")?;
    srv.timeout = timeout;
    let ctx = match read("context.json") {
        Ok(t) => {
            let c: serde_json::Value = serde_json::from_str(&t).context("context.json")?;
            srv.set_context(1, &c)?;
            1
        }
        Err(_) => 0,
    };
    let t0 = Instant::now();
    let r = srv.compile_with_pics(ctx, &source, &pics);
    let ms = t0.elapsed().as_millis();
    let out = match r {
        Ok((cr, _)) => {
            let mut detail: Vec<String> =
                cr.errors.iter().filter_map(|e| e.message.clone()).collect();
            if let Some(i) = &cr.internal {
                detail.push(format!("internal: {}", i.lines().next().unwrap_or("")));
            }
            Outcome {
                status: cr.status,
                detail: detail.join("; "),
                ms,
            }
        }
        Err(e) => Outcome {
            status: "failed".into(),
            detail: format!("{e:#}"),
            ms,
        },
    };
    srv.kill();
    let _ = std::fs::remove_dir_all(&work);
    Ok(out)
}

pub fn passed(o: &Outcome) -> bool {
    o.status == "ok" || o.status == "ok_degraded"
}

/// Words of `text` that are document prose: runs of letters at brace depth 0 or inside the
/// braces of text commands, not in a control word, an environment name, an option list or math.
fn prose_words(text: &str) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut math, mut brackets) = (0, false, 0i32);
    // brace frames: true when the group's contents are prose
    let mut frames: Vec<bool> = Vec::new();
    let mut last_cs = String::new();
    while i < b.len() {
        match b[i] {
            b'\\' => {
                let s = i + 1;
                i = s;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if i == s {
                    i += 1; // control symbol
                }
                last_cs = text[s..i.min(b.len())].to_string();
                continue;
            }
            b'%' => {
                // a comment: prose too
                let e = text[i..].find('\n').map_or(b.len(), |k| i + k);
                let mut k = i + 1;
                while k < e {
                    if b[k].is_ascii_alphabetic() {
                        let s = k;
                        while k < e && b[k].is_ascii_alphabetic() {
                            k += 1;
                        }
                        out.push((s, k));
                    } else {
                        k += 1;
                    }
                }
                i = e;
                continue;
            }
            b'$' => math = !math,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'{' => {
                let structural = matches!(
                    last_cs.as_str(),
                    "begin"
                        | "end"
                        | "label"
                        | "ref"
                        | "cref"
                        | "Cref"
                        | "eqref"
                        | "cite"
                        | "includegraphics"
                        | "input"
                        | "include"
                        | "usepackage"
                        | "documentclass"
                        | "newcommand"
                        | "renewcommand"
                        | "definecolor"
                        | "color"
                        | "textcolor"
                        | "pageref"
                        | "url"
                        | "href"
                        | "setlength"
                        | "hspace"
                        | "vspace"
                );
                frames.push(!structural && frames.last().copied().unwrap_or(true));
            }
            b'}' => {
                frames.pop();
            }
            c if c.is_ascii_alphabetic() || c >= 0x80 => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] >= 0x80) {
                    i += 1;
                }
                if !math && brackets <= 0 && frames.last().copied().unwrap_or(true) {
                    out.push((s, i));
                }
                last_cs.clear();
                continue;
            }
            b' ' | b'\n' | b'\t' => {}
            _ => last_cs.clear(),
        }
        i += 1;
    }
    out
}

/// `text` with its prose words replaced: ASCII letters by `x`/`X`, other characters by `x`.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (s, e) in prose_words(text) {
        out.push_str(&text[at..s]);
        for c in text[s..e].chars() {
            out.push(if c.is_ascii_uppercase() { 'X' } else { 'x' });
        }
        at = e;
    }
    out.push_str(&text[at..]);
    out
}

pub fn anonymize(dir: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        bail!("{} exists", out.display());
    }
    std::fs::create_dir_all(out)?;
    for name in ["report.json", "context.json", "pics.json"] {
        if dir.join(name).is_file() {
            std::fs::copy(dir.join(name), out.join(name))?;
        }
    }
    for name in ["source.tex", "rtex-preamble.tex"] {
        let t = std::fs::read_to_string(dir.join(name))
            .with_context(|| format!("{}: no {name}", dir.display()))?;
        let t = if name == "source.tex" {
            scrub(&t)
        } else {
            // the preamble's own prose: \title, \author, comments
            let (head, tail) = t.split_at(t.find("\\begin{document}").unwrap_or(t.len()));
            format!("{}{tail}", scrub_preamble(head))
        };
        std::fs::write(out.join(name), t)?;
    }
    Ok(())
}

/// The preamble keeps its commands and package lists; the arguments of title-block commands and
/// comments lose their words.
fn scrub_preamble(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let code_end = line
            .char_indices()
            .find(|&(k, c)| c == '%' && (k == 0 || line.as_bytes()[k - 1] != b'\\'))
            .map_or(line.len(), |(k, _)| k);
        let (code, comment) = line.split_at(code_end);
        let code = if ["\\title", "\\author", "\\date", "\\subtitle", "\\institute"]
            .iter()
            .any(|c| code.trim_start().starts_with(c))
        {
            match code.find('{') {
                Some(k) => format!("{}{}", &code[..k], scrub(&code[k..])),
                None => code.to_string(),
            }
        } else {
            code.to_string()
        };
        out.push_str(&code);
        out.push_str(&scrub(comment));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_goes_structure_stays() {
        let src = "Secret words here, \\emph{hidden} too; see \\cref{fig:plan}.\n\\begin{itemize}[label=--]\n\\item Item stuff $x^2 = y$ \\% \\textbf{bold}\n\\end{itemize} % a private note\n";
        let s = scrub(src);
        for w in [
            "Secret", "words", "hidden", "Item", "stuff", "bold", "private", "note",
        ] {
            assert!(!s.contains(w), "{w} survived: {s}");
        }
        for k in [
            "\\emph{",
            "\\cref{fig:plan}",
            "\\begin{itemize}[label=--]",
            "$x^2 = y$",
            "\\textbf{",
            "\\end{itemize}",
        ] {
            assert!(s.contains(k), "{k} lost: {s}");
        }
        assert_eq!(s.len(), src.len());
        let pre =
            "\\documentclass{article}\n\\usepackage{amsmath} % for proofs\n\\title{My Thesis}\n";
        let p = scrub_preamble(pre);
        assert!(
            p.contains("\\usepackage{amsmath}")
                && p.contains("\\title{")
                && !p.contains("Thesis")
                && !p.contains("proofs"),
            "{p}"
        );
    }
}
