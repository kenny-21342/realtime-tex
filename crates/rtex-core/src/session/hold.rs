//! Held pictures: while an edit leaves a picture with errors, the layout keeps the picture's last
//! good drawing instead of what the broken source makes of the page (a half-typed coordinate
//! can swallow the rest of the document). The pass with the broken source still runs and its
//! errors are reported; only the pages shown come from the last good version. A held layout is
//! never `Converged` (docs/how-it-works.md, "Held pictures").
//!
//! A picture is known by its span (`ParaId`, kept across edits) and its ordinal inside the span.
//! Its last good version is the cache entry of the last pass that drew it without errors
//! (`piccache`), pinned while it may be held.

use super::*;
use crate::piccache::PictureRef;

/// A picture's identity across edits: its span and its ordinal among the span's pictures.
pub(super) type PicId = (ParaId, usize);

/// The last version of a picture a pass drew without errors (its cache entry's hash).
#[derive(Debug, Clone)]
pub(super) struct LastGood {
    pub hash: u64,
    pub env: String,
    /// Its source, `\begin` line to `\end` line.
    pub text: String,
}

/// A picture the next layout shows in its last good version.
#[derive(Debug, Clone)]
pub(super) struct HeldPic {
    pub id: PicId,
    /// Its manifest key in the current sources (`file:line` of the `\begin`).
    pub key: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub last_good: LastGood,
}

/// What the session remembers for holding pictures.
#[derive(Debug, Default)]
pub(super) struct HoldState {
    pub last_good: BTreeMap<PicId, LastGood>,
    /// The revision each held picture was first held at (for `HeldPicture::since_revision`).
    pub since: BTreeMap<PicId, Revision>,
    /// `sources_hash` of what the pages shown depict, when they were made without errors (a
    /// clean layout, or a held one: the held pictures' last good texts in place). A held layout
    /// of the same sources needs no pass.
    pub shown_clean: Option<u64>,
}

/// A held picture as `LayoutUpdate` reports it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HeldPicture {
    /// The span whose unit shows the picture (the placements give its place on the page).
    pub par_id: ParaId,
    pub file: String,
    /// 1-based `\begin` and `\end` lines in the current sources.
    pub line: usize,
    pub end_line: usize,
    /// The revision the picture was first held at.
    pub since_revision: Revision,
}

fn split_key(key: &str) -> Option<(&str, usize)> {
    let (f, l) = key.rsplit_once(':')?;
    Some((f, l.parse().ok()?))
}

/// The identity of each picture of a scan (`None`: in no span, e.g. in the preamble).
pub(super) fn identities(pics: &[PictureRef], spans: &[SnapshotSpan]) -> Vec<Option<PicId>> {
    let mut ordinals: HashMap<ParaId, usize> = HashMap::new();
    pics.iter()
        .map(|p| {
            let (file, line) = split_key(&p.key)?;
            let line = line as i64;
            let span = spans.iter().find(|s| {
                s.file.trim_start_matches("./") == file
                    && s.first_line <= line
                    && line <= s.last_line
            })?;
            let n = ordinals.entry(span.id).or_default();
            *n += 1;
            Some((span.id, *n - 1))
        })
        .collect()
}

/// Byte range of lines `first..=last` (1-based) of `text`, without the last line's newline.
fn line_range(text: &str, first: usize, last: usize) -> Option<std::ops::Range<usize>> {
    let mut starts = vec![0usize];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let a = *starts.get(first.checked_sub(1)?)?;
    let b = match starts.get(last) {
        Some(next) => next - 1,
        None => text.len(),
    };
    (a <= b).then_some(a..b)
}

/// A picture's source in `texts`, `\begin` line to `\end` line.
pub(super) fn picture_text(texts: &BTreeMap<String, String>, p: &PictureRef) -> Option<String> {
    let (file, line) = split_key(&p.key)?;
    lines_text(texts, file, line, p.end_line)
}

fn lines_text(
    texts: &BTreeMap<String, String>,
    file: &str,
    first: usize,
    last: usize,
) -> Option<String> {
    let text = texts
        .iter()
        .find(|(n, _)| n.trim_start_matches("./") == file)
        .map(|(_, t)| t)?;
    line_range(text, first, last).map(|r| text[r].to_string())
}

/// The spans of a held layout as it depicts them: each held picture's last good text in place
/// of its current one. The live path's probe compiles a span's text and compares the rows with
/// the layout's; the broken text would fail, and its errors are not the layout's.
pub(super) fn depicted_spans(
    mut spans: Vec<SnapshotSpan>,
    texts: &BTreeMap<String, String>,
    held: &[HeldPic],
) -> Vec<SnapshotSpan> {
    for h in held {
        let Some(current) = lines_text(texts, &h.file, h.line, h.end_line) else {
            continue;
        };
        if let Some(sp) = spans.iter_mut().find(|sp| sp.id == h.id.0) {
            sp.text = sp.text.replacen(&current, &h.last_good.text, 1);
        }
    }
    spans
}

/// The line (1-based) where `file` reads `target` with `\\input`/`\\include` (the first one).
fn input_line(texts: &BTreeMap<String, String>, file: &str, target: &str) -> Option<i64> {
    let text = texts
        .iter()
        .find(|(n, _)| n.trim_start_matches("./") == file)
        .map(|(_, t)| t)?;
    crate::document::find_inputs(text)
        .into_iter()
        .find(|(_, t, _)| t.trim_start_matches("./") == target)
        .map(|(r, _, _)| text[..r.start].matches('\n').count() as i64 + 1)
}

/// The pictures to hold after a pass with these errors: pictures changed since their last good
/// version (same environment), when every located error comes at or after one of them: in its
/// file from its `\begin` on, or in the file that `\input`s its file from that line on (a file
/// that ends inside the picture reports it on the `\input` line). A
/// half-typed coordinate reports its errors from the picture to the end of the document (a
/// picture in a chapter file: in the main file too). An error before every changed picture
/// means the document has another problem: nothing is held. Of the changed pictures, those
/// after the first error of their file are not the cause and are not held.
pub(super) fn decide(
    texts: &BTreeMap<String, String>,
    pics: &[PictureRef],
    ids: &[Option<PicId>],
    errors: &[(String, i64)],
    last_good: &BTreeMap<PicId, LastGood>,
) -> Vec<HeldPic> {
    if errors.is_empty() {
        return Vec::new();
    }
    let mut first_error: BTreeMap<&str, i64> = BTreeMap::new();
    for (f, l) in errors {
        let e = first_error.entry(f.trim_start_matches("./")).or_insert(*l);
        *e = (*e).min(*l);
    }
    let mut held = Vec::new();
    for (p, id) in pics.iter().zip(ids) {
        let (Some(id), Some((file, line))) = (id, split_key(&p.key)) else {
            continue;
        };
        let Some(lg) = last_good.get(id) else {
            continue;
        };
        if lg.env != p.env || lg.hash == p.hash || p.end_line <= line {
            continue;
        }
        if first_error
            .get(file)
            .is_some_and(|&first| (line as i64) > first)
        {
            continue;
        }
        // the capture takes a cached picture only where its \begin starts the line
        let begins = picture_text(texts, p).is_some_and(|t| {
            t.lines()
                .next()
                .is_some_and(|l| l.trim_start().starts_with(&format!("\\begin{{{}}}", p.env)))
        });
        if !begins {
            continue;
        }
        held.push(HeldPic {
            id: *id,
            key: p.key.clone(),
            file: file.to_string(),
            line,
            end_line: p.end_line,
            last_good: lg.clone(),
        });
    }
    let explained = |(f, l): &(String, i64)| {
        let f = f.trim_start_matches("./");
        held.iter().any(|h| {
            (h.file == f && *l >= h.line as i64)
                || input_line(texts, f, &h.file).is_some_and(|at| *l >= at)
        })
    };
    if !held.is_empty() && errors.iter().all(explained) {
        held
    } else {
        Vec::new()
    }
}

fn file_text_mut<'a>(
    texts: &'a mut BTreeMap<String, String>,
    file: &str,
) -> Option<&'a mut String> {
    texts
        .iter_mut()
        .find(|(n, _)| n.trim_start_matches("./") == file)
        .map(|(_, t)| t)
}

/// The sources with each held picture's body blanked, line count kept: `\begin{env}%`, `%` lines,
/// and its `\end` line from `\end{env}` on. The pass's manifest offers the last good drawing for
/// the picture, so the capture skips this body (`rtex-pic.tex`), which must be balanced: the
/// broken one need not be.
pub(super) fn blank(
    texts: &BTreeMap<String, String>,
    held: &[HeldPic],
) -> BTreeMap<String, String> {
    let mut out = texts.clone();
    for h in held {
        let Some(text) = file_text_mut(&mut out, &h.file) else {
            continue;
        };
        let Some(r) = line_range(text, h.line, h.end_line) else {
            continue;
        };
        let env = &h.last_good.env;
        let end_marker = format!("\\end{{{env}}}");
        let lines: Vec<&str> = text[r.clone()].split('\n').collect();
        let n = lines.len();
        let mut blanked: Vec<String> = Vec::with_capacity(n);
        for (i, l) in lines.iter().enumerate() {
            let cr = if l.ends_with('\r') { "\r" } else { "" };
            let indent: String = l
                .chars()
                .take_while(|c| c.is_whitespace() && *c != '\r')
                .collect();
            let new = if i == 0 {
                format!("{indent}\\begin{{{env}}}%{cr}")
            } else if i + 1 == n {
                match l.find(&end_marker) {
                    Some(at) => format!("{indent}{}", &l[at..]),
                    None => l.to_string(),
                }
            } else {
                format!("%{cr}")
            };
            blanked.push(new);
        }
        text.replace_range(r, &blanked.join("\n"));
    }
    out
}

/// The sources with each held picture's last good text in place of its current one: what a
/// held layout depicts.
pub(super) fn restore(
    texts: &BTreeMap<String, String>,
    held: &[HeldPic],
) -> BTreeMap<String, String> {
    let mut out = texts.clone();
    // bottom-up per file: earlier ranges stay valid when a later one changes length
    let mut order: Vec<&HeldPic> = held.iter().collect();
    order.sort_by(|a, b| (&b.file, b.line).cmp(&(&a.file, a.line)));
    for h in order {
        let Some(text) = file_text_mut(&mut out, &h.file) else {
            continue;
        };
        if let Some(r) = line_range(text, h.line, h.end_line) {
            text.replace_range(r, &h.last_good.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pic(key: &str, end_line: usize, hash: u64) -> PictureRef {
        PictureRef {
            key: key.into(),
            env: "tikzpicture".into(),
            hash,
            cacheable: true,
            end_line,
        }
    }

    fn span(id: u64, first: i64, last: i64) -> SnapshotSpan {
        SnapshotSpan {
            id: ParaId(id),
            file: "main.tex".into(),
            first_line: first,
            last_line: last,
            last_revision: 0,
            background_only: true,
            text: String::new(),
        }
    }

    const DOC: &str = "\\begin{document}\nText.\n\n\\begin{tikzpicture}[scale=2]\n\\draw (0,0) -- (1.5,;\n  \\node at (1,1) {$x$};\n\\end{tikzpicture}\n\nAfter.\n\\end{document}\n";

    fn texts() -> BTreeMap<String, String> {
        [("main.tex".to_string(), DOC.to_string())].into()
    }

    fn good() -> LastGood {
        LastGood {
            hash: 1,
            env: "tikzpicture".into(),
            text: "\\begin{tikzpicture}[scale=2]\n\\draw (0,0) -- (1.5,2);\n\\end{tikzpicture}"
                .into(),
        }
    }

    #[test]
    fn pictures_are_known_by_span_and_ordinal() {
        let pics = [
            pic("main.tex:4", 7, 2),
            pic("main.tex:12", 14, 3),
            pic("main.tex:20", 22, 4),
        ];
        let spans = [span(7, 4, 7), span(9, 10, 15), span(11, 19, 23)];
        let ids = identities(&pics, &spans);
        assert_eq!(
            ids,
            vec![
                Some((ParaId(7), 0)),
                Some((ParaId(9), 0)),
                Some((ParaId(11), 0))
            ]
        );
        // two pictures in one span (a figure with two subfigures)
        let ids = identities(
            &[pic("main.tex:11", 12, 2), pic("main.tex:13", 14, 3)],
            &spans,
        );
        assert_eq!(ids, vec![Some((ParaId(9), 0)), Some((ParaId(9), 1))]);
    }

    #[test]
    fn a_changed_picture_before_the_first_error_is_held() {
        let pics = [pic("main.tex:4", 7, 2)];
        let ids = [Some((ParaId(7), 0))];
        let lg: BTreeMap<PicId, LastGood> = [((ParaId(7), 0), good())].into();
        // a runaway: errors after the \end, up to the end of the document
        let errors = vec![("./main.tex".to_string(), 8), ("./main.tex".into(), 10)];
        let held = decide(&texts(), &pics, &ids, &errors, &lg);
        assert_eq!(held.len(), 1);
        assert_eq!((held[0].line, held[0].end_line), (4, 7));
        // an error before the picture: the document has another problem
        let errors = vec![("./main.tex".to_string(), 2), ("./main.tex".into(), 8)];
        assert!(decide(&texts(), &pics, &ids, &errors, &lg).is_empty());
        // an error in another file too
        let errors = vec![("./main.tex".to_string(), 8), ("./ch1.tex".into(), 3)];
        assert!(decide(&texts(), &pics, &ids, &errors, &lg).is_empty());
        // unchanged since its last good version: not the cause
        let same = [pic("main.tex:4", 7, 1)];
        let errors = vec![("./main.tex".to_string(), 8)];
        assert!(decide(&texts(), &same, &ids, &errors, &lg).is_empty());
        // no last good version (a new picture)
        assert!(decide(&texts(), &pics, &ids, &errors, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_picture_in_an_input_file_is_held_for_errors_after_its_input() {
        let mut t = texts();
        t.insert(
            "ch1.tex".into(),
            DOC.replace("\\begin{document}\n", "")
                .replace("\\end{document}\n", ""),
        );
        t.insert(
            "main.tex".into(),
            "\\begin{document}\nIntro.\n\\input{ch1}\nEnd.\n\\end{document}\n".into(),
        );
        let pics = [PictureRef {
            key: "ch1.tex:3".into(),
            ..pic("x", 6, 2)
        }];
        let ids = [Some((ParaId(7), 0))];
        let lg: BTreeMap<PicId, LastGood> = [((ParaId(7), 0), good())].into();
        // the runaway in ch1.tex, and \end{document} in main.tex ending the open picture
        let errors = vec![("./ch1.tex".to_string(), 7), ("./main.tex".into(), 5)];
        assert_eq!(decide(&t, &pics, &ids, &errors, &lg).len(), 1);
        // ch1.tex ends while TeX still scans the coordinate: reported on the \input line
        let errors = vec![("./main.tex".to_string(), 3), ("./main.tex".into(), 5)];
        assert_eq!(decide(&t, &pics, &ids, &errors, &lg).len(), 1);
        // an error in main.tex before the \input is not the picture's
        let errors = vec![("./ch1.tex".to_string(), 7), ("./main.tex".into(), 2)];
        assert!(decide(&t, &pics, &ids, &errors, &lg).is_empty());
    }

    #[test]
    fn blanking_keeps_the_lines_and_restoring_puts_the_last_good_text_back() {
        let pics = [pic("main.tex:4", 7, 2)];
        let ids = [Some((ParaId(7), 0))];
        let lg: BTreeMap<PicId, LastGood> = [((ParaId(7), 0), good())].into();
        let held = decide(&texts(), &pics, &ids, &[("main.tex".into(), 8)], &lg);
        let b = blank(&texts(), &held);
        let t = &b["main.tex"];
        assert_eq!(t.lines().count(), DOC.lines().count());
        assert!(
            t.contains("\nText.\n\n\\begin{tikzpicture}%\n%\n%\n\\end{tikzpicture}\n\nAfter."),
            "{t}"
        );
        let r = restore(&texts(), &held);
        assert_eq!(
            r["main.tex"],
            DOC.replace(
                "\\draw (0,0) -- (1.5,;\n  \\node at (1,1) {$x$};\n",
                "\\draw (0,0) -- (1.5,2);\n"
            )
        );
        // CRLF sources keep their line ends
        let crlf: BTreeMap<String, String> =
            [("main.tex".to_string(), DOC.replace('\n', "\r\n"))].into();
        let b = blank(&crlf, &held);
        assert!(b["main.tex"].contains("\\begin{tikzpicture}%\r\n%\r\n%\r\n\\end{tikzpicture}\r\n"));
    }
}
