//! `cargo run -p rtex-dl --example gfx_dump -- PAGE.json...`: the native drawing of each page
//! display list (capture JSON), as JSON lines `{"file", "ok", "error", "page"}`, where `page`
//! is the `NativePage` plus the page's rules (drawn by the host, filled rectangles) for
//! comparison with a PDF's drawings (scripts/gfx_compare.py).
use rtex_dl::gfx::{apply, native_graphics, ItemRef};
use rtex_dl::{DisplayList, Item};

fn main() {
    for path in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&path).expect("read");
        let dl: DisplayList = serde_json::from_str(&text).expect("display list JSON");
        let candidate = rtex_dl::gfx::only_literals(&dl);
        let out = match native_graphics(&dl) {
            Ok(n) => {
                // rules in display-list coordinates, through their transform when a scope moved them
                let mut rules = Vec::new();
                let rows = std::iter::once((None, &dl.other)).chain(dl.lines.iter().enumerate().map(|(k, l)| (Some(k), &l.items)));
                for (line, items) in rows {
                    for (k, it) in items.iter().enumerate() {
                        if let Item::Rule { x, y_top, width, height } = it {
                            let r = ItemRef { line, item: k };
                            let m = n.transforms.iter().find(|(a, _)| *a == r).map(|(_, m)| *m).unwrap_or(rtex_dl::gfx::IDENTITY);
                            let pts: Vec<(f64, f64)> = [(0, 0), (*width, 0), (*width, *height), (0, *height)]
                                .iter()
                                .map(|(dx, dy)| apply(&m, (*x + dx) as f64, (*y_top + dy) as f64))
                                .collect();
                            rules.push(pts);
                        }
                    }
                }
                // glyphs a transformed scope moves: char and where their origin lands
                let mut moved = Vec::new();
                for (r, m) in &n.transforms {
                    let items = match r.line {
                        None => &dl.other,
                        Some(k) => &dl.lines[k].items,
                    };
                    if let Item::Glyph { char, x, y, .. } = &items[r.item] {
                        let (gx, gy) = apply(m, *x as f64, *y as f64);
                        moved.push(serde_json::json!({"char": char, "x": gx, "y": gy, "dl_x": x, "dl_y": y}));
                    }
                }
                serde_json::json!({"file": path, "ok": true, "candidate": candidate, "flags": dl.flags, "page": n, "rules": rules, "moved_glyphs": moved})
            }
            Err(e) => serde_json::json!({"file": path, "ok": false, "candidate": candidate, "flags": dl.flags, "error": e.to_string()}),
        };
        println!("{out}");
    }
}
