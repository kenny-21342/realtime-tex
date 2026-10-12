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
    rows: BTreeMap<ParaId, i64>,
    following: BTreeSet<ParaId>,
}

fn commands(s: &str) -> Vec<String> {
    static CS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\\(?:[A-Za-z@]+|.)").unwrap());
    CS.find_iter(s).map(|m| m.as_str().to_string()).collect()
}

fn plain_file_change(old: &str, new: &str) -> bool {
    let a: Vec<char> = old.chars().collect();
    let b: Vec<char> = new.chars().collect();
    let first = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let last = a[first..]
        .iter()
        .rev()
        .zip(b[first..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let plain = |c: &char| (c.is_whitespace() || !c.is_control()) && !"\\{}$%#&_~^".contains(*c);
    a[first..a.len() - last].iter().all(plain)
        && b[first..b.len() - last].iter().all(plain)
        && commands(old) == commands(new)
}

fn local_commands(text: &str) -> bool {
    !commands(text).iter().any(|c| {
        c.starts_with("\\if")
            || matches!(
                c.as_str(),
                "\\csname" | "\\def" | "\\let" | "\\includegraphics"
            )
    })
}

#[cfg(test)]
fn plain_change(old: &str, new: &str) -> bool {
    plain_file_change(old, new) && local_commands(old) && local_commands(new)
}

fn plan(s: &Shared, snapshot: &[SnapshotSpan]) -> Option<Plan> {
    if !s.cfg.warm_background || !s.preview_base_ok.load(Ordering::SeqCst) {
        return None;
    }
    let layout = s.layout.lock();
    // Compare source within each file, rather than zipping span lists: a new paragraph
    // introduces an id and moves the line ranges of every following span. Joining with
    // paragraph separators preserves the command vocabulary and admits text-only splits.
    let by_file = |spans: &[SnapshotSpan]| {
        let mut files: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for span in spans {
            files
                .entry(span.file.clone())
                .or_default()
                .push(span.text.clone());
        }
        files
            .into_iter()
            .map(|(file, text)| (file, text.join("\n\n")))
            .collect::<BTreeMap<_, _>>()
    };
    let old_files = by_file(&layout.snapshot_spans);
    let new_files = by_file(snapshot);
    if old_files.len() != new_files.len()
        || old_files.iter().any(|(file, old)| {
            new_files
                .get(file)
                .is_none_or(|new| !plain_file_change(old, new))
        })
    {
        return None;
    }
    // Removed units, preamble/setup changes and changed structural units still need an
    // ordinary layout. The retained ids are also the anchors for the new plain paragraphs.
    let ids = snapshot
        .iter()
        .map(|new| (new.id, new.file.as_str()))
        .collect::<HashMap<_, _>>();
    if layout
        .snapshot_spans
        .iter()
        .any(|old| ids.get(&old.id).is_none_or(|f| *f != old.file))
    {
        return None;
    }
    let policy = s.policy.lock();
    let validated = s.preview_validated.lock();
    let live = s.live.lock();
    let generation = s.engine_generation.load(Ordering::SeqCst);
    let mut changed = BTreeSet::new();
    let mut rows = BTreeMap::new();
    let mut additions = false;
    let mut first_page = i64::MAX;
    for new in snapshot {
        let old = layout.snapshot_span(new.id);
        additions |= old.is_none() || layout.unit(new.id).is_none_or(|u| u.rows() == 0);
        if old.is_some_and(|old| old.file == new.file && old.text == new.text) {
            continue;
        }
        let (shape, reasons) = classify_source_with(&new.text, &policy, false);
        if new.background_only
            || shape != UnitShape::Par
            || !reasons.is_empty()
            || !local_commands(&new.text)
            || old.is_some_and(|o| o.background_only || !local_commands(&o.text))
        {
            return None;
        }
        let unit = if let Some(unit) = layout.unit(new.id).filter(|u| u.rows() > 0) {
            if unit.kind() != "par" || !unit.advanced().is_empty() {
                return None;
            }
            unit
        } else {
            let (parent, _, _) = live.get(new.id)?.derived?;
            let unit = layout.unit(parent)?;
            if unit.kind() != "par" {
                return None;
            }
            unit
        };
        let (revision, gen, count) = validated.get(&new.id)?;
        if *revision < new.last_revision || *gen != generation {
            return None;
        }
        let page = unit.captured.placements.iter().map(|p| p.page).min()?;
        first_page = first_page.min(page.saturating_sub(1).max(1));
        changed.insert(new.id);
        // Captured paragraphs containing display math may have a different live-box row
        // decomposition than their page rows. Their capture completion marker decides;
        // a fresh plain paragraph additionally waits for its entire live row count.
        rows.insert(
            new.id,
            if layout.unit(new.id).is_some_and(|u| u.rows() > 0) {
                1
            } else {
                *count
            },
        );
    }
    // A new paragraph can push the following drawing onto another page. Keep the
    // run until that next unchanged unit is visible too, rather than dropping the
    // picture between two provisional pages while typing continuously.
    let mut following = BTreeSet::new();
    for (i, span) in snapshot
        .iter()
        .enumerate()
        .filter(|(_, sp)| additions && changed.contains(&sp.id))
    {
        if let Some(unit) = snapshot[i + 1..]
            .iter()
            .take_while(|sp| sp.file == span.file)
            .filter(|sp| !changed.contains(&sp.id))
            .filter_map(|sp| layout.unit(sp.id).map(|u| (sp, u)))
            .find(|(sp, u)| {
                u.rows() > 0
                    && (!sp.background_only
                        || u.kind() == "heading"
                        || (u.kind() == "env"
                            && u.name()
                                .is_some_and(|n| crate::eligibility::FLOAT_ENVS.contains(&n))))
            })
        {
            following.insert(unit.0.id);
            rows.entry(unit.0.id).or_insert(unit.1.rows());
        }
    }
    Some(Plan {
        base: layout.layout_version,
        changed,
        first_page,
        rows,
        following,
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
    completed: BTreeSet<ParaId>,
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
            completed: BTreeSet::new(),
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
                    .any(|(r, _, _)| *r >= current)
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
        for id in self.plan.rows.keys() {
            let units = cap
                .units
                .iter()
                .filter(|u| crate::layout::span_for_unit(u, self.spans) == Some(*id))
                .collect::<Vec<_>>();
            if !units.is_empty()
                && units.iter().all(|u| {
                    if self.plan.following.contains(id) {
                        cap.closed.contains(&u.uid)
                    } else {
                        cap.complete.contains(&u.uid)
                    }
                })
                && units.iter().map(|u| u.rows).sum::<i64>() >= self.plan.rows[id]
            {
                self.completed.insert(*id);
            }
        }
        let visible = self.plan.rows.keys().all(|id| self.completed.contains(id));
        if visible {
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
        if visible && current > self.revision {
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
        assert!(plain_change("word", "word\n\nnew"));
        assert!(!plain_change("\\ifnum1>0 word\\fi", "\\ifnum2>0 word\\fi"));
    }
}
