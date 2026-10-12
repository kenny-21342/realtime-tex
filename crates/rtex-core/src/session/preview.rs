//! Page previews for validated text edits, without changing the authoritative layout.
use super::*;
use crate::capture::{PageCapture, PreviewUpdate};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;

pub(super) const SUPERSEDED: &str = "superseded after a page preview";

struct Plan {
    base: u64,
    changed: BTreeSet<ParaId>,
    first_page: i64,
}

fn plain_change(old: &str, new: &str) -> bool {
    let a: Vec<char> = old.chars().collect();
    let b: Vec<char> = new.chars().collect();
    let first = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let last = a[first..]
        .iter()
        .rev()
        .zip(b[first..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let plain = |c: &char| !c.is_control() && !"\\{}$%#&_~^".contains(*c);
    if !a[first..a.len() - last].iter().all(plain) || !b[first..b.len() - last].iter().all(plain) {
        return false;
    }
    // Changing the letters of a control sequence is not a text edit, even if the diff
    // itself contains no backslash. Conditional/stateful sources are excluded too.
    static CS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\\(?:[A-Za-z@]+|.)").unwrap());
    let commands = |s: &str| {
        CS.find_iter(s)
            .map(|m| m.as_str().to_string())
            .collect::<Vec<_>>()
    };
    let ac = commands(old);
    ac == commands(new)
        && !ac.iter().any(|c| {
            c.starts_with("\\if")
                || matches!(
                    c.as_str(),
                    "\\csname" | "\\def" | "\\let" | "\\includegraphics"
                )
        })
}

fn plan(s: &Shared, snapshot: &[SnapshotSpan]) -> Option<Plan> {
    if !s.cfg.warm_background || !s.preview_base_ok.load(Ordering::SeqCst) {
        return None;
    }
    let layout = s.layout.lock();
    if layout.snapshot_spans.len() != snapshot.len() {
        return None;
    }
    let policy = s.policy.lock();
    let validated = s.preview_validated.lock();
    let generation = s.engine_generation.load(Ordering::SeqCst);
    let mut changed = BTreeSet::new();
    let mut first_page = i64::MAX;
    for (old, new) in layout.snapshot_spans.iter().zip(snapshot) {
        if old.id != new.id
            || old.file != new.file
            || old.first_line != new.first_line
            || old.last_line != new.last_line
        {
            return None;
        }
        if old.text == new.text {
            continue;
        }
        if new.background_only || !plain_change(&old.text, &new.text) {
            return None;
        }
        let unit = layout.unit(new.id)?;
        if unit.kind() != "par"
            || !unit.advanced().is_empty()
            || !classify_source_with(&new.text, &policy, false).1.is_empty()
        {
            return None;
        }
        let (revision, gen) = validated.get(&new.id)?;
        if *revision < new.last_revision || *gen != generation {
            return None;
        }
        let page = unit.captured.placements.iter().map(|p| p.page).min()?;
        first_page = first_page.min(page.saturating_sub(1).max(1));
        changed.insert(new.id);
    }
    Some(Plan {
        base: layout.layout_version,
        changed,
        first_page,
    })
}

pub(super) struct PreviewRun<'a> {
    s: &'a Shared,
    spans: &'a [SnapshotSpan],
    revision: Revision,
    plan: Plan,
    id: u64,
    pub stop: Rc<Cell<bool>>,
    pub affected_sent: Rc<Cell<bool>>,
    invalid: bool,
    checked_revision: Revision,
}

impl<'a> PreviewRun<'a> {
    pub fn new(s: &'a Shared, spans: &'a [SnapshotSpan], revision: Revision) -> Option<Self> {
        let plan = plan(s, spans)?;
        Some(Self {
            s,
            spans,
            revision,
            plan,
            id: s.preview_run.fetch_add(1, Ordering::SeqCst) + 1,
            stop: Rc::new(Cell::new(false)),
            affected_sent: Rc::new(Cell::new(false)),
            invalid: false,
            checked_revision: revision,
        })
    }

    pub fn reset(&self) {
        self.s
            .events
            .send(Event::PagePreviewReset {
                run_id: self.id,
                base_layout_version: self.plan.base,
            })
            .ok();
    }

    pub fn update(
        &mut self,
        update: PreviewUpdate,
        fragments: &BTreeMap<String, crate::piccache::Fragment>,
    ) -> bool {
        if matches!(update, PreviewUpdate::Invalid) {
            self.invalid = true;
            self.reset();
            return true;
        }
        if self.invalid || self.s.bg_paused.load(Ordering::SeqCst) {
            return true;
        }
        if matches!(update, PreviewUpdate::Tick) {
            let current = self.s.source_revision.load(Ordering::SeqCst);
            if self.plan.changed.is_empty()
                && current > self.checked_revision
                && self
                    .s
                    .preview_validated
                    .lock()
                    .values()
                    .any(|(r, _)| *r >= current)
            {
                let (_, spans, _) = snapshot(self.s);
                self.checked_revision = current;
                if plan(self.s, &spans).is_some_and(|p| !p.changed.is_empty()) {
                    self.stop.set(true);
                    return false;
                }
            }
            return true;
        }
        let PreviewUpdate::Page(mut cap) = update else {
            unreachable!()
        };
        let current = self.s.source_revision.load(Ordering::SeqCst);
        if self.plan.changed.is_empty() {
            // A no-change/manual run already in flight must not hold up a new validated
            // reflow edit. It has no new page to show, so it may yield before its first page.
            if current > self.revision {
                let (_, spans, _) = snapshot(self.s);
                if plan(self.s, &spans).is_some_and(|p| !p.changed.is_empty()) {
                    self.stop.set(true);
                    return false;
                }
            }
            return true;
        }
        if cap.dl.page.unwrap_or(0) < self.plan.first_page {
            return true;
        }
        crate::piccache::substitute(&mut cap.dl, fragments);
        let native = rtex_dl::gfx::only_literals(&cap.dl)
            .then(|| rtex_dl::gfx::native_graphics(&cap.dl).ok())
            .flatten();
        if !cap.dl.is_exact() && native.is_none() {
            return true;
        }
        let placements = page_placements(&cap, self.spans);
        let affected = placements
            .iter()
            .any(|p| self.plan.changed.contains(&p.par_id));
        if affected {
            self.affected_sent.set(true);
        }
        let page = PageUpdate {
            page: cap.dl.page.unwrap_or(0),
            exact: cap.dl.is_exact(),
            hash: crate::document::hash_str(&serde_json::to_string(&cap.dl).unwrap_or_default()),
            dl: cap.dl,
            native,
        };
        log::debug!(
            "page preview {} page {} source {}",
            self.id,
            page.page,
            self.revision
        );
        self.s
            .events
            .send(Event::PagePreview {
                run_id: self.id,
                base_layout_version: self.plan.base,
                source_revision: self.revision,
                page,
                placements,
            })
            .ok();
        if affected && current > self.revision {
            self.stop.set(true);
            return false;
        }
        true
    }
}

fn page_placements(cap: &PageCapture, snapshot: &[SnapshotSpan]) -> Vec<ParagraphPlacement> {
    let mut groups: BTreeMap<ParaId, Vec<&crate::capture::CapturedUnit>> = BTreeMap::new();
    for unit in &cap.units {
        if let Some(id) = crate::layout::span_for_unit(unit, snapshot) {
            groups.entry(id).or_default().push(unit);
        }
    }
    let mut owners = HashMap::new();
    for (id, mut units) in groups {
        if units.len() > 1 && !units.iter().all(|u| u.kind == "par" && u.nest == 1) {
            continue;
        }
        units.sort_by_key(|u| u.uid);
        let mut offset = 0;
        for u in units {
            owners.insert(u.uid, (id, offset, u.kind.clone()));
            offset += u.rows;
        }
    }
    let mut rows: BTreeMap<ParaId, Vec<(i64, &rtex_dl::Line, String)>> = BTreeMap::new();
    for line in &cap.dl.lines {
        if let Some((id, offset, kind)) = owners.get(&line.unit) {
            rows.entry(*id)
                .or_default()
                .push((offset + line.row, line, kind.clone()));
        }
    }
    rows.into_iter()
        .map(|(par_id, mut rows)| {
            rows.sort_by_key(|r| r.0);
            let xs = rows.iter().map(|r| r.1.x).collect::<Vec<_>>();
            ParagraphPlacement {
                par_id,
                lines: rows.last().unwrap().0,
                kind: rows[0].2.clone(),
                fragments: vec![Fragment {
                    page: cap.dl.page.unwrap_or(0),
                    first_line: rows[0].0,
                    last_line: rows.last().unwrap().0,
                    x: xs[0],
                    xs,
                    baselines: rows.iter().map(|r| r.1.y).collect(),
                    approximate: false,
                }],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_changes_exclude_tex_structure_commands_and_conditionals() {
        assert!(plain_change(
            "A long paragraph.",
            "A much longer paragraph."
        ));
        assert!(plain_change("\\textbf{word}", "\\textbf{words}"));
        assert!(!plain_change("\\textbf{word}", "\\textit{word}"));
        assert!(!plain_change("word", "word\\par"));
        assert!(!plain_change("word", "word\n\nnew"));
        assert!(!plain_change("\\ifnum1>0 word\\fi", "\\ifnum2>0 word\\fi"));
    }
}
