//! Which reasons keep a project's units off the live path, without compiling it.
//!
//! `cargo run -p rtex-core --example eligibility_census -- <project> <main.tex> [<project> <main.tex> ...]`
//!
//! Every file the main file inputs is segmented with the project's preamble policy and each
//! span is classified with the strict allow-list (no probing), as `rtex verify` and the session
//! do before a unit has been captured. Prints the reasons with the number of units each blocks,
//! most frequent first, per project and summed. Only reason names are printed, never source text.

use std::collections::BTreeMap;

use rtex_core::document::{load_project_files, FileBuf, IdAllocator};
use rtex_core::eligibility::{classify_source_with, Policy};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        !args.is_empty() && args.len().is_multiple_of(2),
        "usage: eligibility_census <project> <main.tex> [<project> <main.tex> ...]"
    );
    let mut total: BTreeMap<String, usize> = BTreeMap::new();
    for pair in args.chunks(2) {
        let (project, main) = (std::path::Path::new(&pair[0]), pair[1].as_str());
        let preamble = rtex_core::project_preamble(project, main)?;
        let policy = Policy::from_preamble(&preamble, &[], &[]);
        let files = load_project_files(project, main)?;
        let mut ids = IdAllocator(1);
        let (mut units, mut live) = (0usize, 0usize);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (rel, text) in &files {
            // the main file's body only: its preamble is not made of units
            let body = if rel == main {
                match text.find("\\begin{document}") {
                    Some(at) => &text[at + "\\begin{document}".len()..],
                    None => continue,
                }
            } else {
                text.as_str()
            };
            let fb = FileBuf::with_policy(body, &mut ids, 0, &policy);
            for sp in &fb.spans {
                let src = &fb.text[sp.range.clone()];
                if src.trim().is_empty() {
                    continue;
                }
                units += 1;
                let (_, reasons) = classify_source_with(src, &policy, false);
                if reasons.is_empty() {
                    live += 1;
                }
                // one count per unit and reason kind
                let mut seen: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
                seen.sort();
                seen.dedup();
                for r in seen {
                    *counts.entry(r).or_default() += 1;
                }
            }
        }
        println!(
            "== {} ({main}): {units} units, {live} pass the strict allow-list",
            project.display()
        );
        print_sorted(&counts);
        for (k, v) in counts {
            *total.entry(k).or_default() += v;
        }
    }
    if args.len() > 2 {
        println!("== all projects");
        print_sorted(&total);
    }
    Ok(())
}

fn print_sorted(counts: &BTreeMap<String, usize>) {
    let mut v: Vec<_> = counts.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (reason, n) in v {
        println!("{n:6}  {reason}");
    }
}
