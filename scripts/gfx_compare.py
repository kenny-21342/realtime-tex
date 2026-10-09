#!/usr/bin/env python3
"""Compare rtex's native drawing of PDF literals with MuPDF's reading of the PDF itself.

Usage: gfx_compare.py PDF DUMP.jsonl [--show N]
  DUMP.jsonl: output of `cargo run -p rtex-dl --example gfx_dump -- <capture>/<job>.rtex-page*.json`
              (one line per page file; the page number is taken from the file name).

Every fill/stroke rtex would draw is matched against PyMuPDF's `page.get_drawings()` by type,
bounding box (0.05 pt), colors (0.01) and stroke width (0.02 pt); display-list rules are matched
against the stroked lines LuaTeX draws them as. A page is NATIVE-EXACT when both sides match
completely. Exit status 1 when any interpreted page differs.
"""
import json
import math
import re
import sys

import pymupdf

SP_PER_BP = 65536.0 * 72.27 / 72.0
TOL = 0.05


def apply(m, x, y):
    return (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])


def cubic_extrema(p0, p1, p2, p3):
    """Points of a cubic Bezier at t=0, 1 and where dx/dt or dy/dt vanish (tight bounds)."""
    pts = [p0, p3]
    for k in (0, 1):
        a = -p0[k] + 3 * p1[k] - 3 * p2[k] + p3[k]
        b = 2 * (p0[k] - 2 * p1[k] + p2[k])
        c = p1[k] - p0[k]
        ts = []
        if abs(a) < 1e-12:
            if abs(b) > 1e-12:
                ts.append(-c / b)
        else:
            disc = b * b - 4 * a * c
            if disc >= 0:
                r = math.sqrt(disc)
                ts += [(-b + r) / (2 * a), (-b - r) / (2 * a)]
        for t in ts:
            if 0 < t < 1:
                u = 1 - t
                pts.append(tuple(u ** 3 * p0[j] + 3 * u * u * t * p1[j] + 3 * u * t * t * p2[j] + t ** 3 * p3[j] for j in (0, 1)))
    return pts


def path_points(path, m):
    """Points bounding the path's ink (curves by their extrema; a moveto counts only when a
    segment follows it), in display-list sp."""
    out = []
    cur = start = None
    pending = None
    for s in path:
        if s["s"] == "M":
            cur = start = pending = apply(m, s["x"], s["y"])
        elif s["s"] == "L":
            if pending:
                out.append(pending)
                pending = None
            cur = apply(m, s["x"], s["y"])
            out.append(cur)
        elif s["s"] == "C":
            if pending:
                out.append(pending)
                pending = None
            p1, p2, p3 = apply(m, s["x1"], s["y1"]), apply(m, s["x2"], s["y2"]), apply(m, s["x"], s["y"])
            out += cubic_extrema(cur, p1, p2, p3)
            cur = p3
        elif s["s"] == "Z":
            cur = start
    return out


def to_rgb(color):
    """A rtex Color as MuPDF renders it (its CMYK conversion), plus alpha."""
    c = color["comps"]
    if len(c) == 4:
        pix = pymupdf.Pixmap(pymupdf.csCMYK, pymupdf.IRect(0, 0, 1, 1), False)
        pix.set_pixel(0, 0, tuple(round(v * 255) for v in c))
        rgb = pymupdf.Pixmap(pymupdf.csRGB, pix).pixel(0, 0)
        return [v / 255 for v in rgb[:3]] + [color["alpha"]]
    if len(c) == 1:
        return [c[0]] * 3 + [color["alpha"]]
    return list(c) + [color["alpha"]]


def bbox(pts):
    xs = [p[0] for p in pts]
    ys = [p[1] for p in pts]
    return (min(xs), min(ys), max(xs), max(ys))


def ours(entry):
    out = []
    for op in entry["page"]["ops"]:
        if op["op"] != "Paint":
            continue
        m = op["ctm"]
        pts = [(x / SP_PER_BP, y / SP_PER_BP) for x, y in path_points(op["path"], m)]
        if not pts:
            continue
        kind = ("f" if op["fill"] else "") + ("s" if op["stroke"] else "")
        scale = math.sqrt(abs(m[0] * m[3] - m[1] * m[2])) / SP_PER_BP
        out.append({
            "type": kind,
            "rect": bbox(pts),
            "fill": to_rgb(op["fill"]) if op["fill"] else None,
            "color": to_rgb(op["stroke"]["color"]) if op["stroke"] else None,
            "width": op["stroke"]["width"] * scale if op["stroke"] else None,
            "n": len(op["path"]),
            "pts": [(round(x, 4), round(y, 4)) for x, y in pts],
        })
    # compared as separate fills and strokes: MuPDF reports a fill-and-stroke as one or two
    # drawings depending on the mode and on whether pgf wrote one operator or two
    merged = []
    for o in out:
        if o["type"] == "fs":
            merged.append(dict(o, type="f", color=None, width=None))
            merged.append(dict(o, type="s", fill=None))
        else:
            merged.append(o)
    rules = [bbox([(x / SP_PER_BP, y / SP_PER_BP) for x, y in r]) for r in entry.get("rules", [])]
    return merged, rules


def close(a, b, tol=TOL):
    return all(abs(p - q) <= tol for p, q in zip(a, b))


def rgb_close(a, b):
    if a is None or b is None:
        return a is None and b is None
    # MuPDF's CMYK conversion goes through 8 bits here: 0.01 covers it
    return all(abs(p - q) <= 0.01 for p, q in zip(a[:3], b[:3]))


def visible_drawings(page):
    """The page's fills and strokes that their clips leave visible. A cached picture is a region
    of an earlier pass's page placed as a form: the rest of that page is in it too, clipped away
    by the form's box, and MuPDF lists those paths (extended mode gives the clips: a `clip` item's
    `scissor` holds for the deeper `level`s after it)."""
    out, clips = [], []  # clips: (level, rect)
    for d in page.get_drawings(extended=True):
        lvl = d.get("level", 0)
        while clips and clips[-1][0] >= lvl:
            clips.pop()
        if d["type"] == "clip":
            clips.append((lvl, d["scissor"]))
            continue
        if d["type"] not in ("f", "s", "fs"):
            continue
        r = pymupdf.Rect(d["rect"])
        w = (d.get("width") or 0) / 2
        ink = pymupdf.Rect(r.x0 - w, r.y0 - w, r.x1 + w, r.y1 + w)
        if any(not ink.intersects(c) and not ink.is_empty for _, c in clips) or \
           any(ink.is_empty and not c.contains(ink.tl) for _, c in clips):
            continue
        out.append(d)
    return out


def items_bbox(d):
    """Bounds of a MuPDF drawing from its segments (its `rect` misses subpaths of some
    multi-segment strokes)."""
    pts = []
    for it in d["items"]:
        if it[0] == "l":
            pts += [(p.x, p.y) for p in it[1:3]]
        elif it[0] == "c":
            pts += cubic_extrema(*[(p.x, p.y) for p in it[1:5]])
        elif it[0] == "re":
            r = it[1]
            pts += [(r.x0, r.y0), (r.x1, r.y1)]
        elif it[0] == "qu":
            q = it[1]
            pts += [(p.x, p.y) for p in (q.ul, q.ur, q.ll, q.lr)]
    return bbox(pts) if pts else tuple(d["rect"])


def opacity(d, key):
    v = d.get(key)
    return 1.0 if v is None else v


def matches(o, d):
    if o["type"] != d["type"]:
        return False
    if not close(o["rect"], d["_bbox"]):
        return False
    if "f" in o["type"]:
        if not rgb_close(o["fill"], d.get("fill")):
            return False
        if abs(o["fill"][3] - opacity(d, "fill_opacity")) > 0.01:
            return False
    if "s" in o["type"]:
        if not rgb_close(o["color"], d.get("color")):
            return False
        if abs(o["color"][3] - opacity(d, "stroke_opacity")) > 0.01:
            return False
        if abs(o["width"] - (d.get("width") or 0)) > 0.02:
            return False
    return True


def rule_ink(d):
    """The ink rectangle of a drawing LuaTeX may draw a rule as: one straight stroked segment
    (most rules) or a filled rectangle (both render the same)."""
    items = d["items"]
    if d["type"] == "f" and len(items) == 1 and items[0][0] == "re":
        r = items[0][1]
        return (r.x0, r.y0, r.x1, r.y1)
    if d["type"] != "s" or len(items) != 1 or items[0][0] != "l":
        return None
    (p, q) = items[0][1], items[0][2]
    w = (d.get("width") or 0) / 2
    if abs(p.y - q.y) < 1e-6:
        return (min(p.x, q.x), p.y - w, max(p.x, q.x), p.y + w)
    if abs(p.x - q.x) < 1e-6:
        return (p.x - w, min(p.y, q.y), p.x + w, max(p.y, q.y))
    return None


def main():
    pdf, dump = sys.argv[1], sys.argv[2]
    show = int(sys.argv[sys.argv.index("--show") + 1]) if "--show" in sys.argv else 3
    doc = pymupdf.open(pdf)
    bad = 0
    totals = {"pages": 0, "candidates": 0, "interpreted": 0, "exact": 0}
    reasons = {}
    for line in open(dump):
        e = json.loads(line)
        m = re.search(r"page(\d+)\.json$", e["file"])
        pno = int(m.group(1))
        totals["pages"] += 1
        if not e["candidate"]:
            continue
        totals["candidates"] += 1
        if not e["ok"]:
            key = re.sub(r" \(line .*", "", e["error"])
            reasons[key] = reasons.get(key, 0) + 1
            print(f"page {pno}: not native: {e['error']}")
            continue
        totals["interpreted"] += 1
        mine, rules = ours(e)
        drawings = []
        for d in visible_drawings(doc[pno - 1]):
            d["_bbox"] = items_bbox(d)
            if d["type"] == "fs":
                drawings.append(dict(d, type="f", color=None, width=None))
                drawings.append(dict(d, type="s", fill=None))
            else:
                drawings.append(d)
        left = list(range(len(drawings)))
        unmatched = []
        for o in mine:
            hit = next((k for k in left if matches(o, drawings[k])), None)
            if hit is None:
                unmatched.append(o)
            else:
                left.remove(hit)
        rules_left = []
        for r in rules:
            hit = next((k for k in left if (ri := rule_ink(drawings[k])) and close(r, ri)), None)
            if hit is None:
                rules_left.append(r)
            else:
                left.remove(hit)
        # glyphs moved by a transformed scope: their mapped origin must be where the PDF has them
        # clipped characters too: a glyph clipped away in the PDF is clipped by the native ops as well
        flags = pymupdf.TEXTFLAGS_RAWDICT & ~pymupdf.TEXT_CLIP & ~pymupdf.TEXT_MEDIABOX_CLIP
        chars = [c for b in doc[pno - 1].get_text("rawdict", flags=flags)["blocks"] for l in b.get("lines", [])
                 for sp in l["spans"] for c in sp["chars"]]
        glyphs_bad = []
        for g in e.get("moved_glyphs", []):
            gx, gy = g["x"] / SP_PER_BP, g["y"] / SP_PER_BP
            if not any(abs(c["origin"][0] - gx) <= TOL and abs(c["origin"][1] - gy) <= TOL for c in chars):
                glyphs_bad.append(g)
        ok = not unmatched and not left and not rules_left and not glyphs_bad
        totals["exact"] += ok
        bad += not ok
        print(f"page {pno}: {'NATIVE-EXACT' if ok else 'DIFFERS'} ops {len(mine)} rules {len(rules)} "
              f"pdf drawings {len(drawings)}; unmatched ours {len(unmatched)} rules {len(rules_left)} pdf {len(left)}; "
              f"moved glyphs {len(e.get('moved_glyphs', []))}, misplaced {len(glyphs_bad)}")
        for g in glyphs_bad[:show]:
            print(f"    glyph: {g}")
        for o in unmatched[:show]:
            print(f"    ours: { {k: v for k, v in o.items() if k != 'pts'} }")
        for r in rules_left[:show]:
            print(f"    rule: {r}")
        for k in left[:show]:
            d = drawings[k]
            print(f"    pdf:  type {d['type']} bbox {tuple(round(v, 3) for v in d['_bbox'])} fill {d.get('fill')} "
                  f"color {d.get('color')} width {d.get('width')} items {len(d['items'])}")
    print("totals:", totals, "not native:", reasons)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
