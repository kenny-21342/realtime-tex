//! Picture cache for background passes. A TikZ-heavy document spends most of a layout pass
//! drawing pictures that did not change. Each pass records where every picture environment
//! landed (page, position, dimensions; `rtex-capture.lua`), and the next pass replaces every
//! picture whose source and surroundings are unchanged with that region of the earlier pass's
//! PDF (an image of exactly the picture's size), so the pass only typesets what changed.
//!
//! What identifies a picture: its environment's text, plus everything that can change its
//! rendering without changing its text: the preamble, and the definitions and settings made in
//! the document body before it (`\def`, `\newcommand`, `\tikzset`, `\definecolor` …), in the
//! order TeX reads them (`\input` chains followed). The font, color and width in force at the
//! picture are recorded by the capture and compared at use (`RecordedPic::state`). Pictures
//! that depend on more than that (labels, references, counters, external files, data files,
//! `remember picture`/`overlay`, group-escaping assignments) are never cached. Neither are two
//! pictures linked by a node name: pgf names are global, so a picture can use a node another
//! picture defines; the defining body must run, and the using picture depends on it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Picture environments: units of their own when they start a line, inline boxes after text;
/// the cache handles each (every one has its own `\end{…}` delimiter). The capture's list
/// (`rtex_capture.PICTURE_ENVS`) names the same environments.
pub const PICTURE_ENVS: &[&str] = &["tikzpicture", "circuitikz", "pgfpicture"];

/// A picture environment found in the document sources.
#[derive(Debug, Clone)]
pub struct PictureRef {
    /// `file:line` of its `\begin{…}` (what the capture reports).
    pub key: String,
    pub env: String,
    pub hash: u64,
    pub cacheable: bool,
    /// 1-based line of its `\end{…}` (the capture checks it after skipping the body).
    pub end_line: usize,
}

/// Where a pass drew a picture (`pics` in the capture JSON).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecordedPic {
    #[serde(default)]
    pub env: String,
    pub page: i64,
    /// Page coordinates (sp from the page's top-left): left edge and baseline.
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    pub d: i64,
    pub page_height: i64,
    /// Font, color and width in force where the picture began (the capture compares it before
    /// using the cached picture).
    #[serde(default)]
    pub state: String,
    /// The picture's display-list items relative to its origin (left edge, baseline), absent
    /// when it has anything native drawing cannot take back (see [`Fragment`]).
    #[serde(default)]
    pub items: Option<Vec<rtex_dl::Item>>,
    /// (An empty Lua table arrives as `[]`.)
    #[serde(default, deserialize_with = "rtex_dl::map_or_empty_array")]
    pub fonts: BTreeMap<String, rtex_dl::FontDesc>,
    /// The tiling patterns its literals fill with (as declared, on the recording page).
    #[serde(default, deserialize_with = "rtex_dl::map_or_empty_array")]
    pub patterns: BTreeMap<String, rtex_dl::Pattern>,
}

/// What a cached picture draws, kept with its PDF region: the items of its display list
/// relative to its origin and the fonts they name. A later pass that takes the picture from the
/// cache gets them back at the picture's new place ([`substitute`]), so its page can still be
/// drawn natively; the `cached_picture` item only says where the PDF region goes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fragment {
    pub items: Vec<rtex_dl::Item>,
    pub fonts: BTreeMap<String, rtex_dl::FontDesc>,
    /// The picture's height above its baseline (sp).
    pub h: i64,
    /// The tiling patterns its literals name, their matrices relative to the picture's origin
    /// (PDF space, bp): the cached PDF region keeps its tiles where the recording page had
    /// them, so they move with the picture.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub patterns: BTreeMap<String, rtex_dl::Pattern>,
}

/// The `/pgfpatN` names in a literal's operators.
fn pattern_names(data: &str) -> impl Iterator<Item = &str> {
    data.match_indices("/pgfpat").filter_map(move |(i, _)| {
        let rest = &data[i + 1..];
        let end = rest[6..]
            .find(|c: char| !c.is_ascii_digit())
            .map_or(rest.len(), |k| k + 6);
        (end > 6).then(|| &rest[..end])
    })
}

/// `data` with every `/old` name token that is a key of `names` renamed.
fn rename_patterns(data: &str, names: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(data.len());
    let mut last = 0;
    for name in pattern_names(data) {
        let start = name.as_ptr() as usize - data.as_ptr() as usize;
        if let Some(new) = names.get(name) {
            out.push_str(&data[last..start]);
            out.push_str(new);
            last = start + name.len();
        }
    }
    out.push_str(&data[last..]);
    out
}

impl Fragment {
    /// Only what a page can be drawn from natively: glyphs, rules, literals, colors, matrices,
    /// math markers and recorded shadings.
    fn from_recorded(r: &RecordedPic) -> Option<Fragment> {
        let items = r.items.as_ref()?;
        let mut patterns = BTreeMap::new();
        let (ox, oy) = (
            r.x as f64 / rtex_dl::SP_PER_BP,
            (r.page_height - r.y) as f64 / rtex_dl::SP_PER_BP,
        );
        let ok = items.iter().all(|i| match i {
            rtex_dl::Item::Unsupported { kind, .. } => kind == "shading",
            rtex_dl::Item::Image { .. } => false,
            // every pattern it fills with is known
            rtex_dl::Item::Literal { data, .. } => pattern_names(data).all(|n| {
                let Some(p) = r.patterns.get(n) else {
                    return false;
                };
                let mut p = p.clone();
                p.matrix[4] -= ox;
                p.matrix[5] -= oy;
                patterns.insert(n.to_string(), p);
                true
            }),
            _ => true,
        });
        ok.then(|| Fragment {
            items: items.clone(),
            fonts: r.fonts.clone(),
            h: r.h,
            patterns,
        })
    }
}

/// A cached picture: a region of a kept pass PDF.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub env: String,
    #[serde(default)]
    pub state: String,
    pub pdf: String,
    pub page: i64,
    /// PDF-space bounding box (llx, lly, urx, ury) in sp of a bp (`img.new{bbox}`).
    pub bbox: [i64; 4],
    pub w: i64,
    pub h: i64,
    pub d: i64,
    /// Pass serial the entry was last wanted in (for eviction).
    pub last_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Fragment>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    serial: u64,
    /// Names the PDFs of this index's lifetime (`p<epoch>-<serial>.pdf`): an index lost and
    /// rebuilt from scratch never reuses a name a reader may still hold.
    #[serde(default)]
    epoch: u64,
    entries: BTreeMap<String, CacheEntry>, // hash (decimal) -> entry
    /// `file:line` keys whose cached body did not end where the source scan said (a pass
    /// reported them mismatched), with the serial until which they are drawn, not cached.
    #[serde(default)]
    bad: BTreeMap<String, u64>,
}

pub struct PicCache {
    dir: PathBuf,
    index: Index,
}

/// Passes an entry survives without being wanted (edits that toggle a picture back and forth).
const KEEP_PASSES: u64 = 4;

impl PicCache {
    pub fn open(dir: &Path) -> PicCache {
        // the manifest names the cached PDFs by absolute path: lualatex runs in the snapshot
        // directory, not where the host started
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut index: Index = std::fs::read_to_string(dir.join("index.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if index.epoch == 0 {
            index.epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(1)
                .max(1);
        }
        PicCache { dir, index }
    }

    pub fn entries(&self) -> usize {
        self.index.entries.len()
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(
            &self.dir.join("index.json"),
            &serde_json::to_vec(&self.index)?,
        )
    }

    /// Write the manifest the next capture pass reads: every current picture the cache holds,
    /// keyed by its `file:line`. Returns the number of hits. Removes the file when there are
    /// none (the capture then draws everything).
    pub fn write_manifest(&mut self, pics: &[PictureRef], manifest: &Path) -> Result<usize> {
        self.index.serial += 1;
        let serial = self.index.serial;
        let mut m: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for p in pics.iter().filter(|p| p.cacheable) {
            if let Some(e) = self.index.entries.get_mut(&p.hash.to_string()) {
                e.last_used = serial;
            }
            if let Some(v) = self.entry_json(p) {
                m.insert(p.key.clone(), v);
            }
        }
        if m.is_empty() {
            let _ = std::fs::remove_file(manifest);
        } else {
            write_atomic(manifest, &serde_json::to_vec(&m)?)?;
        }
        let _ = self.save();
        Ok(m.len())
    }

    /// The fragments of the pictures a manifest offers (by `file:line` key): what a pass that
    /// takes them from the cache splices back into its page lists.
    pub fn fragments(&self, pics: &[PictureRef]) -> BTreeMap<String, Fragment> {
        pics.iter()
            .filter(|p| p.cacheable && !self.index.bad.contains_key(&p.key))
            .filter_map(|p| {
                let e = self.index.entries.get(&p.hash.to_string())?;
                (e.env == p.env)
                    .then(|| e.native.clone())
                    .flatten()
                    .map(|f| (p.key.clone(), f))
            })
            .collect()
    }

    /// The cache entry for a picture, as the capture and the live engine read it (`pdf` by
    /// absolute path, `bbox` in PDF user space, `state` to compare, `end_line` of the source).
    pub fn entry_json(&self, p: &PictureRef) -> Option<serde_json::Value> {
        if !p.cacheable || self.index.bad.contains_key(&p.key) {
            return None;
        }
        let e = self.index.entries.get(&p.hash.to_string())?;
        if e.env != p.env || !self.dir.join(&e.pdf).exists() {
            return None;
        }
        Some(serde_json::json!({
            "key": p.key,
            "env": e.env,
            "state": e.state,
            "end_line": p.end_line,
            "pdf": self.dir.join(&e.pdf).to_string_lossy(),
            "page": e.page, "bbox": e.bbox, "w": e.w, "h": e.h, "d": e.d,
        }))
    }

    /// After a pass: remember every current picture the pass drew (its page of `pass_pdf`,
    /// extracted into the cache), forget the pictures the pass reported as mismatched
    /// (`stale_keys`: a cached body that did not end where the source scan said), evict what
    /// no picture wanted for a while, drop PDFs nothing references. A picture with an error
    /// between its `\begin` and `\end` lines (`errors`: the pass's `(file, line)` error
    /// locations) is not cached: taken from the cache it would no longer be compiled, and the
    /// error would vanish from the next pass. Returns the number of new entries.
    pub fn absorb(
        &mut self,
        pics: &[PictureRef],
        recorded: &BTreeMap<String, RecordedPic>,
        stale_keys: &[String],
        errors: &[(String, i64)],
        pass_pdf: &Path,
    ) -> Result<usize> {
        let serial = self.index.serial;
        // a mismatched key is drawn for a while instead of cached again right away (the
        // mismatch would repeat on every other pass)
        self.index.bad.retain(|_, until| *until >= serial);
        for k in stale_keys {
            if let Some(p) = pics.iter().find(|p| &p.key == k) {
                self.index.entries.remove(&p.hash.to_string());
            }
            self.index.bad.insert(k.clone(), serial + KEEP_PASSES);
        }
        let mut new: Vec<(&PictureRef, &RecordedPic)> = Vec::new();
        let has_error = |p: &PictureRef| {
            let Some((file, line)) = p.key.rsplit_once(':') else {
                return false;
            };
            let Ok(begin) = line.parse::<i64>() else {
                return false;
            };
            errors.iter().any(|(f, l)| {
                f.trim_start_matches("./") == file.trim_start_matches("./")
                    && (begin..=p.end_line as i64).contains(l)
            })
        };
        for p in pics
            .iter()
            .filter(|p| p.cacheable && !self.index.bad.contains_key(&p.key) && !has_error(p))
        {
            let Some(r) = recorded.get(&p.key) else {
                continue;
            };
            if r.env != p.env || r.w <= 0 || r.h + r.d <= 0 {
                continue;
            }
            // an entry drawn under another state (font, color, width), or whose PDF is gone,
            // is replaced
            if self
                .index
                .entries
                .get(&p.hash.to_string())
                .map(|e| e.state == r.state && self.dir.join(&e.pdf).exists())
                .unwrap_or(false)
            {
                continue;
            }
            new.push((p, r));
        }
        let added = new.len();
        if !new.is_empty() {
            std::fs::create_dir_all(&self.dir)?;
            let name = format!("p{}-{serial}.pdf", self.index.epoch);
            // only the pages that carry new pictures (a 100-page PDF per edited picture
            // would fill the disk); page numbers are remapped onto the extract
            let pages: Vec<i64> = new
                .iter()
                .map(|(_, r)| r.page)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let remap: BTreeMap<i64, i64> =
                match extract_pages(pass_pdf, &pages, &self.dir.join(&name)) {
                    Ok(()) => pages
                        .iter()
                        .enumerate()
                        .map(|(i, p)| (*p, i as i64 + 1))
                        .collect(),
                    Err(e) => {
                        log::warn!(
                            "picture cache: page extraction failed ({e:#}), copying the pass PDF"
                        );
                        std::fs::copy(pass_pdf, self.dir.join(&name)).with_context(|| {
                            format!("copying {} into the picture cache", pass_pdf.display())
                        })?;
                        pages.iter().map(|p| (*p, *p)).collect()
                    }
                };
            for (p, r) in new {
                // PDF user space: x unchanged (sp of a bp == sp), y measured from the page bottom
                let bbox = [
                    r.x,
                    r.page_height - (r.y + r.d),
                    r.x + r.w,
                    r.page_height - (r.y - r.h),
                ];
                self.index.entries.insert(
                    p.hash.to_string(),
                    CacheEntry {
                        env: p.env.clone(),
                        state: r.state.clone(),
                        pdf: name.clone(),
                        page: remap[&r.page],
                        bbox,
                        w: r.w,
                        h: r.h,
                        d: r.d,
                        last_used: serial,
                        native: Fragment::from_recorded(r),
                    },
                );
            }
        }
        // eviction: entries no current picture wants for KEEP_PASSES passes
        self.index
            .entries
            .retain(|_, e| e.last_used + KEEP_PASSES >= serial);
        // PDFs nothing references any more
        let referenced: BTreeSet<&str> = self
            .index
            .entries
            .values()
            .map(|e| e.pdf.as_str())
            .collect();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.starts_with('p')
                    && name.ends_with(".pdf")
                    && !referenced.contains(name.as_str())
                {
                    let _ = std::fs::remove_file(ent.path());
                }
            }
        }
        self.save()?;
        Ok(added)
    }
}

/// Write `data` to `path` through a temporary file and a rename: a reader (the live engine's
/// index lookup, the capture's manifest read) never sees a torn file.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))
}

/// Write the given pages (1-based, ascending) of `src` as `dst`, in that order.
fn extract_pages(src: &Path, pages: &[i64], dst: &Path) -> Result<()> {
    let mut doc = lopdf::Document::load(src).context("loading the pass PDF")?;
    let all: Vec<u32> = doc.get_pages().keys().copied().collect();
    let keep: BTreeSet<u32> = pages.iter().map(|p| *p as u32).collect();
    if !keep.iter().all(|p| all.contains(p)) {
        anyhow::bail!("page out of range");
    }
    let delete: Vec<u32> = all.into_iter().filter(|p| !keep.contains(p)).collect();
    doc.delete_pages(&delete);
    doc.prune_objects();
    doc.renumber_objects();
    doc.save(dst).context("writing the page extract")?;
    Ok(())
}

/// Control words that make a picture depend on more than its own source and the definitions
/// before it (matched as whole words: `\ref`, not `\reflectbox`).
const UNCACHEABLE_WORDS: &[&str] = &[
    // the aux file
    "label",
    "ref",
    "pageref",
    "eqref",
    "footnotemark",
    "footnotetext",
    // links and anchors (not in the page content an image carries)
    "href",
    "hyperref",
    "hyperlink",
    "hypertarget",
    "url",
    "autoref",
    "cref",
    "Cref",
    "vref",
    "nameref",
    // counters, dates
    "the",
    "value",
    "arabic",
    "roman",
    "Roman",
    "alph",
    "Alph",
    "fnsymbol",
    "today",
    "newcounter",
    "stepcounter",
    "refstepcounter",
    "addtocounter",
    "setcounter",
    // other files, verbatim, notes
    "verb",
    "input",
    "pgfimage",
    "includesvg",
    "includepdf",
    "lstinputlisting",
    "include",
    "footnote",
    "index",
    "marginpar",
    "write",
    "immediate",
    // randomness, positions
    "pgfmathrandom",
    "random",
    "pgfmathsetseed",
    "pdfsavepos",
    "savepos",
    // assignments and boxes that escape the picture's group: a skipped body must have no
    // effect on what follows it
    "global",
    "xdef",
    "gdef",
    "savebox",
    "sbox",
    "pgfdeclarelayer",
    "newbox",
    "newsavebox",
    "usebox",
    "setbox",
    // catcode changes and Lua: a skipped body is scanned with the catcodes in force at the
    // picture, so a body that reads part of itself under other catcodes (`%` inside
    // `luacode*`, `\verb|}|`) is not skipped the way the drawing reads it; Lua may also
    // print anything
    "catcode",
    "makeatletter",
    "makeatother",
    "ExplSyntaxOn",
    "obeylines",
    "obeyspaces",
    "scantokens",
    "lstinline",
    "mintinline",
    "Verb",
    "directlua",
    "luaexec",
    "luadirect",
];

/// Control-word prefixes (`\citep`, `\includegraphics*`, `\pgfplotstableread` …).
const UNCACHEABLE_PREFIXES: &[&str] = &["cite", "includegraphics", "pgfplotstable", "footcite"];

/// Plain text. `trim left`/`trim right` (pgfplots `trim axis left/right`) are kerns pgf puts
/// outside the picture box, which a cached box does not carry; `legend to name` writes a
/// label.
const UNCACHEABLE_TEXT: &[&str] = &[
    "remember picture",
    "overlay",
    "trim left",
    "trim right",
    "trim axis",
    "legend to name",
];

/// Body-level statements whose effect a later picture may depend on (hashed into every
/// picture after them, the whole statement when it spans lines).
const PRELUDE_HEADS: &[&str] = &[
    "def",
    "edef",
    "gdef",
    "xdef",
    "let",
    "newcommand",
    "renewcommand",
    "providecommand",
    "newenvironment",
    "renewenvironment",
    "tikzset",
    "tikzstyle",
    "pgfplotsset",
    "pgfkeys",
    "definecolor",
    "colorlet",
    "setlength",
    "newlength",
    "pgfmathsetmacro",
    "pgfmathsetlengthmacro",
    "pgfdeclare",
    "ctikzset",
    "usetikzlibrary",
    "usepgfplotslibrary",
    "newcolumntype",
    "linespread",
    "selectfont",
    "pgfdeclarelayer",
    "pgfsetlayers",
];

fn is_letter(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// Does `text` contain the control word `\name` (not as a prefix of a longer word)?
fn has_cs(text: &str, name: &str) -> bool {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(k) = text[from..].find('\\') {
        let start = from + k + 1;
        let end = start + name.len();
        if text[start..].starts_with(name) && (end >= b.len() || !is_letter(b[end])) {
            return true;
        }
        from = start;
    }
    false
}

/// Does `text` print a counter with `\the<counter>` (`\thesection`, `\thepage`, `\theequation`
/// and every user `\the…`)? `\theta` and `\therefore` are symbols, `\the` itself is a word of
/// its own.
fn has_the_counter(text: &str) -> bool {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(k) = text[from..].find("\\the") {
        let start = from + k + 4;
        let mut end = start;
        while end < b.len() && is_letter(b[end]) {
            end += 1;
        }
        let rest = &text[start..end];
        if !rest.is_empty() && !matches!(rest, "ta" | "refore") {
            return true;
        }
        from = start;
    }
    false
}

/// Does `text` contain a control word starting with `\name`?
fn has_cs_prefix(text: &str, name: &str) -> bool {
    text.contains(&format!("\\{name}"))
}

/// Does the word `word` (letters around it excluded) appear followed by `[` or `{`, like the
/// pgfplots data sources `\addplot table[x=a] {f.dat}` and `\addplot file {f.dat}`?
fn word_before_arg(text: &str, word: &str) -> bool {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(k) = text[from..].find(word) {
        let start = from + k;
        let end = start + word.len();
        let left_ok = start == 0 || !is_letter(b[start - 1]);
        let mut j = end;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t' || b[j] == b'\n') {
            j += 1;
        }
        if left_ok
            && j < b.len()
            && (b[j] == b'[' || b[j] == b'{')
            && (end == b.len() || !is_letter(b[end]))
        {
            return true;
        }
        from = end;
    }
    false
}

/// Does `text` open an environment that reads its body under other catcodes (the list is the
/// segmenter's: `document::is_verbatim_env`)?
fn has_verbatim_env(text: &str) -> bool {
    let mut from = 0;
    while let Some(k) = text[from..].find("\\begin{") {
        let start = from + k + "\\begin{".len();
        let name = text[start..].split('}').next().unwrap_or("");
        if crate::document::is_verbatim_env(name) {
            return true;
        }
        from = start;
    }
    false
}

/// Would the skip of a cached picture's body (rtex-pic.tex: `\rtex@gobbleto`, which takes
/// everything up to each `\end` as a macro argument) stop at the picture's own `\end{env}`?
/// It cannot when a `}` closes a group the body never opened (an argument error) or when the
/// `\end{env}` sits inside braces (the skip runs past it to the end of the file). `text` is
/// the comment-stripped source from the `\begin{env}` line to the `\end{env}` line.
fn skip_stops_at_end(text: &str, env: &str) -> bool {
    let begin = format!("\\begin{{{env}}}");
    let end = format!("\\end{{{env}}}");
    let Some(b) = text.find(&begin) else {
        return false;
    };
    let body = &text[b + begin.len()..];
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if depth == 0 && body[i..].starts_with(&end) {
                    return true;
                }
                i += 1;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Does a picture with this (comment-stripped) text depend on more than its source and the
/// definitions before it?
pub fn uncacheable(text: &str) -> bool {
    UNCACHEABLE_WORDS.iter().any(|w| has_cs(text, w))
        || has_verbatim_env(text)
        || has_the_counter(text)
        || UNCACHEABLE_PREFIXES.iter().any(|w| has_cs_prefix(text, w))
        || UNCACHEABLE_TEXT.iter().any(|t| text.contains(t))
        || word_before_arg(text, "table")
        || word_before_arg(text, "file")
}

fn hash_bytes(h: &mut std::collections::hash_map::DefaultHasher, s: &str) {
    use std::hash::Hasher;
    h.write(s.as_bytes());
    h.write_u8(0);
}

/// Hash of the preamble (with its `\input`s expanded) for the picture hashes: every picture
/// depends on it.
pub fn preamble_hash(texts: &BTreeMap<String, String>, main: &str) -> u64 {
    texts
        .get(main)
        .and_then(|t| crate::split_preamble(t))
        .map(|(p, _)| crate::document::hash_str(&crate::document::expand_inputs(p, texts)))
        .unwrap_or(0)
}

/// Number of lines, from `lines[0]`, a statement spans: until its braces balance (a `\tikzset{`
/// block over several lines). At least 1, at most 64.
fn statement_lines(lines: &[&str]) -> usize {
    let mut depth = 0i32;
    for (n, line) in lines.iter().enumerate().take(64) {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 1,
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if depth <= 0 {
            return n + 1;
        }
    }
    lines.len().clamp(1, 64)
}

/// Every picture environment in the project's body text, with its hash (preamble hash, the
/// definitions made before it, its own text) and whether it may be cached.
pub fn scan_pictures(
    texts: &BTreeMap<String, String>,
    main: &str,
    preamble_hash: u64,
) -> Vec<PictureRef> {
    use crate::document::strip_comment;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let body_start = |name: &str| -> usize {
        if name != main {
            return 0;
        }
        // the line after the \begin{document} TeX reads (not one in a comment or verbatim)
        let text = &texts[name];
        crate::document::find_command(text, "\\begin{document}")
            .map(|off| text[..off].matches('\n').count() + 1)
            .unwrap_or(0)
    };
    // `remember picture` anywhere (comments aside; the preamble too: `\tikzset{every
    // picture/.style={remember picture}}`) disables the cache: such pictures place material
    // relative to others
    let has_remember = texts.values().any(|text| {
        text.lines()
            .any(|l| strip_comment(l).contains("remember picture"))
    });
    struct Walker<'a> {
        texts: &'a BTreeMap<String, String>,
        prelude: DefaultHasher,
        out: Vec<PictureRef>,
        has_remember: bool,
        stack: Vec<String>,
        /// How often each file was read: a file `\input` twice yields two pictures per
        /// `file:line` key, which the capture cannot tell apart.
        visits: BTreeMap<String, u32>,
        /// Comment-stripped text of each picture in `out`, for the node-name links.
        bodies: Vec<String>,
    }
    impl Walker<'_> {
        fn walk(&mut self, name: &str, start: usize) {
            if self.stack.len() > 8 || self.stack.iter().any(|s| s == name) {
                return;
            }
            *self.visits.entry(name.to_string()).or_default() += 1;
            self.stack.push(name.to_string());
            let text = &self.texts[name];
            let lines: Vec<&str> = text.lines().collect();
            let mut i = start;
            while i < lines.len() {
                let line = strip_comment(lines[i]);
                let trimmed = line.trim_start();
                let opened = PICTURE_ENVS
                    .iter()
                    .find(|e| trimmed.contains(&format!("\\begin{{{e}}}")));
                if let Some(env) = opened {
                    let begin_marker = format!("\\begin{{{env}}}");
                    let end_marker = format!("\\end{{{env}}}");
                    let start = i;
                    let mut depth = 0i32;
                    let mut end = None;
                    let mut j = i;
                    while j < lines.len() {
                        let l = strip_comment(lines[j]);
                        depth += l.matches(&begin_marker).count() as i32;
                        depth -= l.matches(&end_marker).count() as i32;
                        if depth <= 0 {
                            end = Some(j);
                            break;
                        }
                        j += 1;
                    }
                    let Some(end) = end else { break };
                    let body: Vec<&str> = lines[start..=end]
                        .iter()
                        .map(|l| strip_comment(l))
                        .collect();
                    let text_all = body.join("\n");
                    // the capture keys a picture by the line its \begin executes on, which is
                    // this line only when nothing precedes the \begin on it (a macro argument
                    // closing here would execute its pictures with this line number)
                    let cacheable = !self.has_remember
                        && trimmed.starts_with(&begin_marker)
                        && text_all.matches(&begin_marker).count() == 1
                        && !line.contains("\\end{")
                        && !uncacheable(&text_all)
                        && skip_stops_at_end(&text_all, env);
                    let mut h = self.prelude.clone();
                    hash_bytes(&mut h, &text_all);
                    self.bodies.push(text_all.clone());
                    self.out.push(PictureRef {
                        key: format!("{}:{}", name.trim_start_matches("./"), start + 1),
                        env: env.to_string(),
                        hash: h.finish(),
                        cacheable,
                        end_line: end + 1,
                    });
                    i = end + 1;
                    continue;
                }
                // files read here come before everything after this line
                for (_, target, _) in crate::document::find_inputs(line) {
                    if self.texts.contains_key(&target) {
                        self.walk(&target, 0);
                    }
                }
                if PRELUDE_HEADS.iter().any(|h| has_cs(line, h)) {
                    let n = statement_lines(&lines[i..]);
                    for l in &lines[i..i + n] {
                        hash_bytes(&mut self.prelude, strip_comment(l).trim());
                    }
                    i += n;
                    continue;
                }
                i += 1;
            }
            self.stack.pop();
        }
    }
    let mut prelude = DefaultHasher::new();
    prelude.write_u64(preamble_hash);
    let mut w = Walker {
        texts,
        prelude,
        out: Vec::new(),
        has_remember,
        stack: Vec::new(),
        visits: BTreeMap::new(),
        bodies: Vec::new(),
    };
    if texts.contains_key(main) {
        w.walk(main, body_start(main));
    }
    let twice: Vec<&str> = w
        .visits
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(f, _)| f.trim_start_matches("./"))
        .collect();
    if !twice.is_empty() {
        for p in &mut w.out {
            if p.key
                .rsplit_once(':')
                .is_some_and(|(f, _)| twice.contains(&f))
            {
                p.cacheable = false;
            }
        }
    }
    for k in linked_by_names(&w.bodies) {
        w.out[k].cacheable = false;
    }
    w.out
}

/// Node and coordinate names a picture defines: `name=`/`alias=` options (pgfplots axes too),
/// `node`/`coordinate`/`matrix`/`pic` followed by `(name)`, `\pgfcoordinate{name}`. Generous on
/// purpose: a name taken for defined only costs caching.
fn defined_names(text: &str) -> BTreeSet<String> {
    use std::sync::OnceLock;
    static RES: OnceLock<[regex::Regex; 3]> = OnceLock::new();
    let res = RES.get_or_init(|| {
        [
            regex::Regex::new(r"(?:^|[^A-Za-z ])\s*(?:name|alias)\s*=\s*\{?\s*([A-Za-z0-9_:\-]+)").unwrap(),
            regex::Regex::new(
                r"\b(?:node|coordinate|matrix|pic)\s*(?:\[[^\]]*\]\s*)?(?:at\s*\([^)]*\)\s*)?\(\s*([A-Za-z0-9_:\-]+)\s*\)",
            )
            .unwrap(),
            regex::Regex::new(r"\\pgf(?:coordinate|nodealias)\s*\{([^}]*)\}").unwrap(),
        ]
    });
    res.iter()
        .flat_map(|re| re.captures_iter(text).map(|c| c[1].trim().to_string()))
        .filter(|n| !n.is_empty())
        .collect()
}

/// Does `text` refer to the node `name`: `(name)`, `(name.anchor)`, `-| name`, `|- name`, `of name`?
fn uses_name(text: &str, name: &str) -> bool {
    let b = text.as_bytes();
    let is_name = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'\\';
    let mut from = 0;
    while let Some(k) = text[from..].find(name) {
        let at = from + k;
        let end = at + name.len();
        from = at + 1;
        if (at > 0 && is_name(b[at - 1])) || (end < b.len() && is_name(b[end])) {
            continue;
        }
        let before = text[..at].trim_end();
        if before.ends_with('(')
            || before.ends_with("-|")
            || before.ends_with("|-")
            || before.ends_with("of")
            || before.ends_with("of=")
        {
            return true;
        }
    }
    false
}

/// Indices of the pictures linked by a node name: one uses a name another defines (and it does
/// not define itself). Both must be drawn: the definition has to run, and the user's drawing
/// depends on the definer.
fn linked_by_names(bodies: &[String]) -> BTreeSet<usize> {
    let defs: Vec<BTreeSet<String>> = bodies.iter().map(|t| defined_names(t)).collect();
    let mut owners: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (k, d) in defs.iter().enumerate() {
        for n in d {
            owners.entry(n.as_str()).or_default().push(k);
        }
    }
    let mut out = BTreeSet::new();
    for (p, text) in bodies.iter().enumerate() {
        for (name, qs) in &owners {
            if defs[p].contains(*name) || !uses_name(text, name) {
                continue;
            }
            out.insert(p);
            out.extend(qs.iter().copied());
        }
    }
    out
}

/// Put the drawing of cached pictures back into a page list: every `cached_picture` item whose
/// detail names a key in `fragments` becomes that picture's items at the picture's place (fonts
/// matched by identity or added), and the page's flags follow (`pic_cache` drops when no cached
/// picture is left; `literal`/`shading` count what came back). Returns how many were replaced.
pub fn substitute(dl: &mut rtex_dl::DisplayList, fragments: &BTreeMap<String, Fragment>) -> usize {
    use rtex_dl::Item;
    if fragments.is_empty() {
        return 0;
    }
    let mut replaced = 0;
    let mut added_flags: BTreeMap<String, i64> = BTreeMap::new();
    let mut glyphs = 0;
    let mut fonts = dl.fonts.clone();
    let mut spots: Vec<rtex_dl::PictureSpot> = Vec::new();
    let mut patterns: BTreeMap<String, rtex_dl::Pattern> = BTreeMap::new();
    let mut next_id = fonts
        .keys()
        .filter_map(|k| k.parse::<i64>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let mut splice = |items: &mut Vec<Item>| {
        let mut out = Vec::with_capacity(items.len());
        for it in items.drain(..) {
            let frag = match &it {
                Item::Unsupported { kind, detail } if kind == "cached_picture" => {
                    detail.as_str().and_then(|d| {
                        let mut f = d.splitn(6, ' ');
                        let _idx = f.next()?;
                        let x: i64 = f.next()?.parse().ok()?;
                        let top: i64 = f.next()?.parse().ok()?;
                        let (_w, _h) = (f.next()?, f.next()?);
                        let frag = fragments.get(f.next()?)?;
                        Some((frag, x, top + frag.h))
                    })
                }
                _ => None,
            };
            let Some((frag, ox, oy)) = frag else {
                out.push(it);
                continue;
            };
            // where the picture is (a host copying a picture into a live unit needs it)
            if let Item::Unsupported { detail, .. } = &it {
                let f: Vec<&str> = detail.as_str().unwrap_or("").splitn(6, ' ').collect();
                if let (Some(w), Some(h), Some(key)) = (
                    f.get(3).and_then(|v| v.parse().ok()),
                    f.get(4).and_then(|v| v.parse().ok()),
                    f.get(5),
                ) {
                    spots.push(rtex_dl::PictureSpot {
                        key: key.to_string(),
                        x: ox,
                        top: oy - frag.h,
                        width: w,
                        height: h,
                    });
                }
            }
            // its patterns, under names of their own on this page, at the picture's place
            let mut names: BTreeMap<String, String> = BTreeMap::new();
            if !frag.patterns.is_empty() {
                let page_h = dl.page_height.unwrap_or(0) as f64;
                let (px, py) = (
                    ox as f64 / rtex_dl::SP_PER_BP,
                    (page_h - oy as f64) / rtex_dl::SP_PER_BP,
                );
                for (n, p) in &frag.patterns {
                    let new = format!("{n}c{replaced}");
                    let mut p = p.clone();
                    p.matrix[4] += px;
                    p.matrix[5] += py;
                    patterns.insert(new.clone(), p);
                    names.insert(n.clone(), new);
                }
            }
            // the fragment's font ids are the recording pass's: map them onto this page's
            let mut ids: BTreeMap<i64, i64> = BTreeMap::new();
            for (k, fd) in &frag.fonts {
                let Ok(old) = k.parse::<i64>() else { continue };
                let id = match fonts.iter().find(|(_, f)| f.key() == fd.key()) {
                    Some((pk, _)) => pk.parse().unwrap_or(old),
                    None => {
                        let id = next_id;
                        next_id += 1;
                        let mut nf = fd.clone();
                        nf.id = id;
                        fonts.insert(id.to_string(), nf);
                        id
                    }
                };
                ids.insert(old, id);
            }
            for i in &frag.items {
                let moved = match i.clone() {
                    Item::Glyph {
                        font,
                        char,
                        index,
                        x,
                        y,
                        width,
                        expansion,
                    } => {
                        glyphs += 1;
                        Item::Glyph {
                            font: *ids.get(&font).unwrap_or(&font),
                            char,
                            index,
                            x: x + ox,
                            y: y + oy,
                            width,
                            expansion,
                        }
                    }
                    Item::Rule {
                        x,
                        y_top,
                        width,
                        height,
                    } => Item::Rule {
                        x: x + ox,
                        y_top: y_top + oy,
                        width,
                        height,
                    },
                    Item::Literal { mode, data, at } => {
                        *added_flags.entry("literal".into()).or_default() += 1;
                        Item::Literal {
                            mode,
                            data: if names.is_empty() {
                                data
                            } else {
                                rename_patterns(&data, &names)
                            },
                            at: at.map(|(x, y)| (x + ox, y + oy)),
                        }
                    }
                    Item::Matrix { op, x, y, data } => Item::Matrix {
                        op,
                        x: x + ox,
                        y: y + oy,
                        data,
                    },
                    Item::Math { on, x } => Item::Math { on, x: x + ox },
                    Item::Unsupported { kind, detail } if kind == "shading" => {
                        *added_flags.entry("shading".into()).or_default() += 1;
                        let d = detail.as_str().unwrap_or("");
                        let mut f = d.splitn(4, ' ');
                        let moved = match (
                            f.next(),
                            f.next().and_then(|v| v.parse::<i64>().ok()),
                            f.next().and_then(|v| v.parse::<i64>().ok()),
                            f.next(),
                        ) {
                            (Some(idx), Some(x), Some(top), Some(rest)) => {
                                format!("{idx} {} {} {rest}", x + ox, top + oy)
                            }
                            _ => d.to_string(),
                        };
                        Item::Unsupported {
                            kind,
                            detail: serde_json::Value::String(moved),
                        }
                    }
                    other => other,
                };
                out.push(moved);
            }
            replaced += 1;
        }
        *items = out;
    };
    splice(&mut dl.other);
    for l in &mut dl.lines {
        splice(&mut l.items);
    }
    if replaced == 0 {
        return 0;
    }
    dl.fonts = fonts;
    dl.patterns.extend(patterns);
    dl.glyphs += glyphs;
    dl.pictures.extend(spots);
    let mut flags = dl.flags_map();
    let still_cached = dl
        .lines
        .iter()
        .flat_map(|l| l.items.iter())
        .chain(dl.other.iter())
        .filter(|i| matches!(i, Item::Unsupported { kind, .. } if kind == "cached_picture"))
        .count();
    if still_cached == 0 {
        flags.remove("pic_cache");
        flags.remove("pic_cache_detail");
    } else {
        flags.insert("pic_cache".into(), serde_json::json!(still_cached));
    }
    for (k, n) in added_flags {
        let v = flags.get(&k).and_then(|v| v.as_i64()).unwrap_or(0) + n;
        flags.insert(k, serde_json::json!(v));
    }
    dl.flags = serde_json::to_value(flags).unwrap_or_default();
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts_of(main: &str) -> BTreeMap<String, String> {
        let mut t = BTreeMap::new();
        t.insert("main.tex".to_string(), main.to_string());
        t
    }

    #[test]
    fn scan_and_hash() {
        let mut texts = texts_of(
            "\\documentclass{article}\n\\begin{document}\n\\def\\H{4}\nText.\n\\begin{tikzpicture}\n  \\draw (0,0) -- (\\H,1);\n\\end{tikzpicture}\n\n\\begin{tikzpicture}[remember picture]\n\\end{tikzpicture}\n\\begin{tikzpicture}\n\\node {\\ref{x}};\n\\end{tikzpicture}\n\\end{document}\n",
        );
        let pics = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics.len(), 3);
        assert!(
            !pics.iter().any(|p| p.cacheable),
            "remember picture disables the cache"
        );
        let t = texts.get_mut("main.tex").unwrap();
        *t = t.replace("[remember picture]", "");
        let pics = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics[0].key, "main.tex:5");
        assert_eq!(pics[0].end_line, 7);
        assert!(pics[0].cacheable && pics[1].cacheable && !pics[2].cacheable);
        let h0 = pics[0].hash;
        // a definition before the picture changes its hash; text after it does not
        let t = texts.get_mut("main.tex").unwrap();
        *t = t.replace("\\def\\H{4}", "\\def\\H{5}");
        let pics2 = scan_pictures(&texts, "main.tex", 1);
        assert_ne!(pics2[0].hash, h0);
        let t = texts.get_mut("main.tex").unwrap();
        *t = t.replace("Text.", "Other text.");
        let pics3 = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics3[0].hash, pics2[0].hash);
        // the preamble is part of every hash
        let pics4 = scan_pictures(&texts, "main.tex", 2);
        assert_ne!(pics4[0].hash, pics3[0].hash);
        // a comment mentioning remember picture does not disable the cache
        let t = texts.get_mut("main.tex").unwrap();
        *t = t.replace("Other text.", "Other text. % use remember picture one day");
        assert!(scan_pictures(&texts, "main.tex", 1)[0].cacheable);
        // remember picture set in the preamble (tikzmark, overlays) disables it as well
        let t = texts.get_mut("main.tex").unwrap();
        *t = t.replace(
            "\\documentclass{article}\n",
            "\\documentclass{article}\n\\tikzset{every picture/.style={remember picture}}\n",
        );
        assert!(!scan_pictures(&texts, "main.tex", 1)[0].cacheable);
    }

    #[test]
    fn the_body_starts_at_the_begin_document_tex_reads() {
        // a commented \begin{document} in the preamble does not make the preamble's picture
        // (inside a macro definition) a body picture
        let main = "\\documentclass{article}\n% \\begin{document} goes below\n\\newcommand\\pic{%\n\\begin{tikzpicture}\\draw (0,0)--(1,1);\\end{tikzpicture}}\n\\begin{document}\n\\begin{tikzpicture}\n\\draw (0,0) -- (1,1);\n\\end{tikzpicture}\n\\end{document}\n";
        let pics = scan_pictures(&texts_of(main), "main.tex", 0);
        let keys: Vec<&str> = pics.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["main.tex:6"]);
    }

    #[test]
    fn counters_links_and_double_inputs_are_not_cached() {
        // \the<counter> prints a number the picture's text does not show
        assert!(uncacheable("\\node {Section \\thesection};"));
        assert!(uncacheable("\\node {p.~\\thepage};"));
        assert!(!uncacheable("\\draw (0,0) -- (\\theta:1);"));
        assert!(!uncacheable("\\node {$\\therefore x$};"));
        assert!(uncacheable("\\node {\\href{https://x.y}{link}};"));
        assert!(uncacheable("\\node {text\\footnotemark};"));
        assert!(uncacheable("\\pgfimage{fig}"));
        // a file read twice: its pictures share their file:line keys
        let mut texts = texts_of(
            "\\begin{document}\n\\input{fig}\n\\def\\H{2}\n\\input{fig}\n\\end{document}\n",
        );
        texts.insert(
            "fig.tex".into(),
            "\\begin{tikzpicture}\n\\draw (0,0) -- (\\H,1);\n\\end{tikzpicture}\n".into(),
        );
        let pics = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].key, pics[1].key);
        assert!(!pics[0].cacheable && !pics[1].cacheable);
        let t = texts.get_mut("main.tex").unwrap();
        *t = t.replacen("\\input{fig}\n", "", 1);
        let pics = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics.len(), 1);
        assert!(pics[0].cacheable);
    }

    #[test]
    fn prelude_order_and_multiline_statements() {
        // a multi-line \tikzset: a change on its second line reaches the pictures after it
        let mut texts = texts_of(
            "\\begin{document}\n\\tikzset{%\n  every node/.style={draw,red},\n}\n\\begin{tikzpicture}\n\\node {a};\n\\end{tikzpicture}\n\\input{styles}\n\\begin{tikzpicture}[mystyle]\n\\node {b};\n\\end{tikzpicture}\n\\end{document}\n",
        );
        texts.insert(
            "styles.tex".to_string(),
            "\\centering\\tikzset{mystyle/.style={thick}}\n".to_string(),
        );
        let p1 = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(p1.len(), 2);
        let changed = texts["main.tex"].replace("draw,red", "draw,blue");
        texts.insert("main.tex".to_string(), changed);
        let p2 = scan_pictures(&texts, "main.tex", 1);
        assert_ne!(p1[0].hash, p2[0].hash, "second line of the \\tikzset block");
        // a definition in an \input file (not at the start of its line) reaches the pictures
        // after the \input, not the ones before it
        texts.insert(
            "styles.tex".to_string(),
            "\\centering\\tikzset{mystyle/.style={dashed}}\n".to_string(),
        );
        let p3 = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(p2[0].hash, p3[0].hash);
        assert_ne!(p2[1].hash, p3[1].hash);
    }

    #[test]
    fn pictures_linked_by_a_node_name_are_not_cached() {
        let cacheable = |main: &str| -> Vec<bool> {
            scan_pictures(&texts_of(main), "main.tex", 1)
                .iter()
                .map(|p| p.cacheable)
                .collect()
        };
        // a pgfplots axis named in one picture, the next one placed below it
        let doc = "\\documentclass{article}\n\\begin{document}\n\\begin{tikzpicture}\n\\begin{axis}[\n    name=upper,\n]\n\\end{axis}\n\\end{tikzpicture}\n\\begin{tikzpicture}\n\\begin{axis}[\n    at={(upper.below south west)},\n]\n\\end{axis}\n\\end{tikzpicture}\n\\begin{tikzpicture}\n\\draw (0,0) -- (1,1);\n\\end{tikzpicture}\n\\end{document}\n";
        assert_eq!(cacheable(doc), vec![false, false, true]);
        // a node, used with an anchor, `-|` and positioning's `of`
        for using in [
            "\\draw (p.east) -- ++(1,0);",
            "\\draw (0,0) -| p;",
            "\\node[right=of p] {x};",
            "\\draw (p) -- (1,0);",
        ] {
            let doc = format!("\\documentclass{{article}}\n\\begin{{document}}\n\\begin{{tikzpicture}}\n\\node[draw] (p) {{P}};\n\\end{{tikzpicture}}\n\\begin{{tikzpicture}}\n{using}\n\\end{{tikzpicture}}\n\\end{{document}}\n");
            assert_eq!(cacheable(&doc), vec![false, false], "{using}");
        }
        // every picture defining and using its own (A), or printing the letter: still cached
        let doc = "\\documentclass{article}\n\\begin{document}\n\\begin{tikzpicture}\n\\coordinate (A) at (0,0);\n\\draw (A) -- (1,0);\n\\end{tikzpicture}\n\\begin{tikzpicture}\n\\node (A) at (0,0) {A};\n\\draw (A) -- (1,1) node {A};\n\\end{tikzpicture}\n\\end{document}\n";
        assert_eq!(cacheable(doc), vec![true, true]);
        // `legend to name` / `name path` are not node names
        assert!(defined_names("\\begin{axis}[legend to name=leg, name path=curve]").is_empty());
    }

    #[test]
    fn pattern_names_are_found_and_renamed_as_whole_tokens() {
        let d = "/pgfprgb cs 1 0 0 /pgfpat1 scn /pgfpat12 scn /pgfpatx";
        assert_eq!(
            pattern_names(d).collect::<Vec<_>>(),
            ["pgfpat1", "pgfpat12"]
        );
        let names = BTreeMap::from([("pgfpat1".to_string(), "pgfpat1c0".to_string())]);
        assert_eq!(
            rename_patterns(d, &names),
            "/pgfprgb cs 1 0 0 /pgfpat1c0 scn /pgfpat12 scn /pgfpatx"
        );
    }

    #[test]
    fn cached_pictures_keep_their_patterns_where_their_tiles_were() {
        use rtex_dl::{DisplayList, Item, Line, Pattern, SP_PER_BP};
        let k = SP_PER_BP;
        let pat = Pattern {
            paint_type: 2,
            bbox: [0.0, 0.0, 3.0, 3.0],
            xstep: 3.0,
            ystep: 3.0,
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            content: "0 0 m 3 3 l S".into(),
        };
        let lit = |name: &str| Item::Literal {
            mode: 0,
            data: format!("/pgfprgb cs 0 0 1 /{name} scn 0 0 5 5 re f"),
            at: Some((0, 0)),
        };
        // recorded at (100 bp, baseline 300 bp from the top) of a 800 bp page
        let rec = |patterns: BTreeMap<String, Pattern>| RecordedPic {
            page: 1,
            x: (100.0 * k) as i64,
            y: (300.0 * k) as i64,
            w: 1000,
            h: 2000,
            d: 0,
            page_height: (800.0 * k) as i64,
            items: Some(vec![lit("pgfpat3")]),
            patterns,
            ..Default::default()
        };
        // a picture whose pattern was not recorded has no drawing to give back
        assert!(Fragment::from_recorded(&rec(BTreeMap::new())).is_none());
        let frag =
            Fragment::from_recorded(&rec(BTreeMap::from([("pgfpat3".to_string(), pat)]))).unwrap();
        let rel = &frag.patterns["pgfpat3"];
        assert!((rel.matrix[4] + 100.0).abs() < 1e-3 && (rel.matrix[5] + 500.0).abs() < 1e-3);
        // put back 10 bp lower on a page that has its own pgfpat3
        let mut dl = DisplayList {
            kind: "page".into(),
            page_height: Some((800.0 * k) as i64),
            lines: vec![Line {
                items: vec![
                    lit("pgfpat3"),
                    Item::Unsupported {
                        kind: "cached_picture".into(),
                        detail: serde_json::Value::String(format!(
                            "5 {} {} 1000 2000 main.tex:3",
                            (100.0 * k) as i64,
                            (310.0 * k) as i64 - 2000
                        )),
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        dl.patterns.insert("pgfpat3".into(), Pattern::default());
        let fragments = BTreeMap::from([("main.tex:3".to_string(), frag)]);
        assert_eq!(substitute(&mut dl, &fragments), 1);
        assert_eq!(dl.lines[0].items[0], lit("pgfpat3"));
        assert_eq!(dl.lines[0].items[1], {
            let Item::Literal { data, .. } = lit("pgfpat3c0") else {
                unreachable!()
            };
            Item::Literal {
                mode: 0,
                data,
                at: Some(((100.0 * k) as i64, (310.0 * k) as i64)),
            }
        });
        let moved = &dl.patterns["pgfpat3c0"];
        assert!((moved.matrix[4]).abs() < 1e-3 && (moved.matrix[5] + 10.0).abs() < 1e-3);
        assert_eq!(dl.patterns["pgfpat3"], Pattern::default());
    }

    #[test]
    fn substitute_puts_a_cached_picture_back_and_says_where() {
        use rtex_dl::{DisplayList, FontDesc, Item, Line};
        let font = FontDesc {
            id: 7,
            filename: Some("lmroman10-regular.otf".into()),
            size: Some(655360.0),
            ..Default::default()
        };
        let mut frag_fonts = BTreeMap::new();
        frag_fonts.insert("7".to_string(), font.clone());
        let frag = Fragment {
            items: vec![
                Item::Literal {
                    mode: 0,
                    data: "0 0 m 10 0 l S".into(),
                    at: Some((0, 0)),
                },
                Item::Glyph {
                    font: 7,
                    char: 65,
                    index: Some(36),
                    x: 100,
                    y: -50,
                    width: 400,
                    expansion: 0,
                },
            ],
            fonts: frag_fonts,
            h: 3000,
            patterns: BTreeMap::new(),
        };
        let mut fragments = BTreeMap::new();
        fragments.insert("main.tex:12".to_string(), frag);
        // the same font under another id on the page
        let mut page_font = font;
        page_font.id = 3;
        let mut dl = DisplayList {
            kind: "page".into(),
            lines: vec![Line {
                items: vec![Item::Unsupported {
                    kind: "cached_picture".into(),
                    detail: serde_json::Value::String("5 1000 2000 4000 3500 main.tex:12".into()),
                }],
                ..Default::default()
            }],
            flags: serde_json::json!({"pic_cache": 1}),
            ..Default::default()
        };
        dl.fonts.insert("3".into(), page_font);
        assert_eq!(substitute(&mut dl, &fragments), 1);
        let items = &dl.lines[0].items;
        // baseline = top + h: 2000 + 3000
        assert_eq!(
            items[0],
            Item::Literal {
                mode: 0,
                data: "0 0 m 10 0 l S".into(),
                at: Some((1000, 5000))
            }
        );
        assert!(matches!(
            items[1],
            Item::Glyph {
                font: 3,
                x: 1100,
                y: 4950,
                ..
            }
        ));
        let flags = dl.flags_map();
        assert!(!flags.contains_key("pic_cache"));
        assert_eq!(flags.get("literal").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            dl.pictures,
            vec![rtex_dl::PictureSpot {
                key: "main.tex:12".into(),
                x: 1000,
                top: 2000,
                width: 4000,
                height: 3500
            }]
        );
        // an unknown key stays a cached region
        let mut other = dl.clone();
        other.lines[0].items = vec![Item::Unsupported {
            kind: "cached_picture".into(),
            detail: serde_json::Value::String("5 1 2 3 4 other.tex:1".into()),
        }];
        assert_eq!(substitute(&mut other, &fragments), 0);
    }

    #[test]
    fn cacheability_rules() {
        assert!(!uncacheable(
            "\\node {$\\alpha$}; \\draw (0,0) -- (\\theta:1);"
        ));
        assert!(!uncacheable(
            "\\reflectbox{x} \\romannumeral 4 \\indexspace"
        ));
        assert!(uncacheable("\\node {\\ref{x}};"));
        assert!(uncacheable("\\node {\\alph{page}};"));
        assert!(uncacheable("\\citep{k}"));
        assert!(uncacheable("\\addplot table[x=t,y=v] {results.dat};"));
        assert!(uncacheable("\\addplot table [col sep=comma] {f.csv};"));
        assert!(uncacheable("\\addplot file {f.dat};"));
        assert!(!uncacheable("\\node {a table of files};"));
        assert!(uncacheable("\\pgfmathsetmacro\\w{3} \\xdef\\figwidth{\\w}"));
        assert!(uncacheable("\\global\\advance\\c by 1"));
        assert!(uncacheable("\\includegraphics*[width=1cm]{f}"));
        // bodies read under other catcodes, Lua
        assert!(uncacheable(
            "\\begin{luacode*}\n tex.print(\"{hsb}{\")\n\\end{luacode*}"
        ));
        assert!(uncacheable("\\begin{Verbatim}x\\end{Verbatim}"));
        assert!(uncacheable("\\node {\\directlua{tex.print(1)}};"));
        assert!(uncacheable("{\\catcode`\\|=13 x}"));
        assert!(!uncacheable("\\node[draw] {a comment-free node};"));
    }

    #[test]
    fn the_body_skip_must_stop_at_the_pictures_end() {
        let ok = "\\begin{tikzpicture}\n\\draw (0,0) -- (1,1);\n\\end{tikzpicture}";
        assert!(skip_stops_at_end(ok, "tikzpicture"));
        // a `{` left open by a stripped `%` (luacode* read under normal catcodes)
        let open = "\\begin{tikzpicture}\n\\fill[c] {hsb}{\n\\end{tikzpicture}";
        assert!(!skip_stops_at_end(open, "tikzpicture"));
        let extra = "\\begin{tikzpicture}\n}\\draw;\n\\end{tikzpicture}";
        assert!(!skip_stops_at_end(extra, "tikzpicture"));
        let nested = "\\begin{tikzpicture}\n\\node {\\end{tikzpicture}};";
        assert!(!skip_stops_at_end(nested, "tikzpicture"));
        // a picture after text on its line, or with text before its \begin, is not keyed by
        // the line its \begin is on
        let texts = texts_of(
            "\\begin{document}\n\\foo{\n\\begin{tikzpicture} a\n\\end{tikzpicture}}\\begin{tikzpicture}\nb\n\\end{tikzpicture}\n\\end{document}\n",
        );
        // the scan sees one picture (two \begins before the last \end): not cacheable
        let pics = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics.len(), 1);
        assert!(!pics[0].cacheable);
        let texts = texts_of(
            "\\begin{document}\n\\foo{x}\\begin{tikzpicture}\nb\n\\end{tikzpicture}\n\\end{document}\n",
        );
        let pics = scan_pictures(&texts, "main.tex", 1);
        assert_eq!(pics.len(), 1);
        assert!(!pics[0].cacheable, "text before the \\begin on its line");
    }

    #[test]
    fn cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rtex-piccache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pdf = dir.join("pass.pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&pdf, b"%PDF-1.5 fake").unwrap();
        let pics = vec![PictureRef {
            key: "main.tex:5".into(),
            env: "tikzpicture".into(),
            hash: 42,
            cacheable: true,
            end_line: 9,
        }];
        let mut cache = PicCache::open(&dir.join("cache"));
        let manifest = dir.join("pic-manifest.json");
        assert_eq!(cache.write_manifest(&pics, &manifest).unwrap(), 0);
        assert!(!manifest.exists());
        let mut rec = BTreeMap::new();
        rec.insert(
            "main.tex:5".to_string(),
            RecordedPic {
                env: "tikzpicture".into(),
                page: 2,
                x: 100,
                y: 1000,
                w: 300,
                h: 200,
                d: 50,
                page_height: 5000,
                state: "font/0 g 0 G".into(),
                ..Default::default()
            },
        );
        // a picture with an error in its lines (the \end line here) is not cached; one elsewhere
        // does not matter
        let err = |f: &str, l: i64| vec![(f.to_string(), l)];
        let mut other = PicCache::open(&dir.join("cache-errors"));
        assert_eq!(
            other
                .absorb(&pics, &rec, &[], &err("./main.tex", 9), &pdf)
                .unwrap(),
            0
        );
        assert_eq!(other.entries(), 0);
        assert_eq!(
            other
                .absorb(&pics, &rec, &[], &err("./other.tex", 6), &pdf)
                .unwrap(),
            1
        );
        // not a real PDF: the whole file is copied and page numbers stay
        assert_eq!(cache.absorb(&pics, &rec, &[], &[], &pdf).unwrap(), 1);
        assert_eq!(cache.entries(), 1);
        assert_eq!(cache.write_manifest(&pics, &manifest).unwrap(), 1);
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
        let e = &m["main.tex:5"];
        assert_eq!(e["page"], 2);
        assert_eq!(e["end_line"], 9);
        assert_eq!(
            e["bbox"],
            serde_json::json!([100, 5000 - 1050, 400, 5000 - 800])
        );
        assert_eq!(e["d"], 50);
        assert_eq!(e["state"], "font/0 g 0 G");
        // the same picture drawn under another state (font, color, width) replaces the entry
        let mut rec2 = rec.clone();
        rec2.get_mut("main.tex:5").unwrap().state = "other font/0 g 0 G".into();
        rec2.get_mut("main.tex:5").unwrap().h = 400;
        assert_eq!(cache.absorb(&pics, &rec2, &[], &[], &pdf).unwrap(), 1);
        assert_eq!(cache.entries(), 1);
        cache.write_manifest(&pics, &manifest).unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
        assert_eq!(m["main.tex:5"]["state"], "other font/0 g 0 G");
        assert_eq!(m["main.tex:5"]["h"], 400);
        // a pass that reports the picture as mismatched forgets it
        assert_eq!(
            cache
                .absorb(&pics, &BTreeMap::new(), &["main.tex:5".into()], &[], &pdf)
                .unwrap(),
            0
        );
        assert_eq!(cache.entries(), 0);
        // and draws it (no entry) for KEEP_PASSES passes before caching it again
        assert_eq!(cache.absorb(&pics, &rec, &[], &[], &pdf).unwrap(), 0);
        for _ in 0..KEEP_PASSES {
            assert_eq!(cache.write_manifest(&pics, &manifest).unwrap(), 0);
            assert_eq!(cache.absorb(&pics, &rec, &[], &[], &pdf).unwrap(), 0);
        }
        cache.write_manifest(&pics, &manifest).unwrap();
        assert_eq!(cache.absorb(&pics, &rec, &[], &[], &pdf).unwrap(), 1);
        assert_eq!(cache.write_manifest(&pics, &manifest).unwrap(), 1);
        // a changed picture: no hit; after KEEP_PASSES unwanted passes the entry and its PDF go
        let changed = vec![PictureRef {
            hash: 43,
            ..pics[0].clone()
        }];
        for _ in 0..(KEEP_PASSES + 1) {
            assert_eq!(cache.write_manifest(&changed, &manifest).unwrap(), 0);
            cache
                .absorb(&changed, &BTreeMap::new(), &[], &[], &pdf)
                .unwrap();
        }
        assert_eq!(cache.entries(), 0);
        assert!(std::fs::read_dir(dir.join("cache"))
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".pdf")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
