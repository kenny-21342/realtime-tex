#!/usr/bin/env python3
"""Check rtex's native pgf shadings against MuPDF's rendering of the PDF.

Usage: gfx_shading_check.py PDF DUMP.jsonl [--dpi 600] [--sabotage]
  DUMP.jsonl: output of `cargo run -p rtex-dl --example gfx_dump` for the capture's page lists.

For every `Shade` op, points on a grid over its box are kept when they lie inside every clip in
force (by more than 1.5 pt), away (2 pt) from every path painted later on the page and from every
glyph; at each, the color the gradient gives (composited on white with the op's alpha) is compared
with MuPDF's pixel. --sabotage reverses every shading's stops: the check must then fail.
Exit status 1 when any shading differs by more than 0.04 on a channel at any sample.

Each sample is a pixel: the exact color is evaluated at the pixel's center (through the inverse
of the op's transform), and pixels where the gradient changes by more than the tolerance within
half a pixel are skipped.
"""
import json
import math
import re
import sys

import pymupdf

SP_PER_BP = 65536.0 * 72.27 / 72.0
TOL = 0.04


def apply(m, x, y):
    return (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])


def to_pt(m, x, y):
    X, Y = apply(m, x, y)
    return (X / SP_PER_BP, Y / SP_PER_BP)


def flatten(path, m):
    """Polygons (lists of page points, pt) of a path, curves in 16 steps."""
    polys, cur, start = [], None, None
    for s in path:
        if s["s"] == "M":
            cur = start = (s["x"], s["y"])
            polys.append([to_pt(m, *cur)])
        elif s["s"] == "L":
            cur = (s["x"], s["y"])
            polys[-1].append(to_pt(m, *cur))
        elif s["s"] == "C":
            p0, p1, p2, p3 = cur, (s["x1"], s["y1"]), (s["x2"], s["y2"]), (s["x"], s["y"])
            for k in range(1, 17):
                t = k / 16
                u = 1 - t
                x = u ** 3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t ** 3 * p3[0]
                y = u ** 3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t ** 3 * p3[1]
                polys[-1].append(to_pt(m, x, y))
            cur = p3
        elif s["s"] == "Z":
            if polys and start:
                polys[-1].append(to_pt(m, *start))
            cur = start
    return [p for p in polys if len(p) > 1]


def winding(polys, x, y):
    w = 0
    for poly in polys:
        for (x0, y0), (x1, y1) in zip(poly, poly[1:] + poly[:1]):
            if y0 <= y < y1 and (x1 - x0) * (y - y0) - (x - x0) * (y1 - y0) > 0:
                w += 1
            elif y1 <= y < y0 and (x1 - x0) * (y - y0) - (x - x0) * (y1 - y0) < 0:
                w -= 1
    return w


def inside(polys, even_odd, x, y):
    w = winding(polys, x, y)
    return (w % 2 == 1) if even_odd else (w != 0)


def dist_to_edges(polys, x, y):
    best = math.inf
    for poly in polys:
        for (x0, y0), (x1, y1) in zip(poly, poly[1:] + poly[:1]):
            dx, dy = x1 - x0, y1 - y0
            L = dx * dx + dy * dy
            t = 0 if L == 0 else max(0, min(1, ((x - x0) * dx + (y - y0) * dy) / L))
            best = min(best, math.hypot(x - (x0 + t * dx), y - (y0 + t * dy)))
    return best


def rgb(comps):
    if len(comps) == 1:
        return [comps[0]] * 3
    if len(comps) == 3:
        return list(comps)
    pix = pymupdf.Pixmap(pymupdf.csCMYK, pymupdf.IRect(0, 0, 1, 1), False)
    pix.set_pixel(0, 0, tuple(round(v * 255) for v in comps))
    return [v / 255 for v in pymupdf.Pixmap(pymupdf.csRGB, pix).pixel(0, 0)[:3]]


def param(op, fx, fy):
    """The shading parameter s (0..1 between the ends) at a form-space point, or None."""
    c = op["coords"]
    if not op["radial"]:
        x0, y0, x1, y1 = c
        dx, dy = x1 - x0, y1 - y0
        return ((fx - x0) * dx + (fy - y0) * dy) / (dx * dx + dy * dy)
    x0, y0, r0, x1, y1, r1 = c
    # largest s with |p - c(s)| = r(s), r(s) >= 0
    cdx, cdy, dr = x1 - x0, y1 - y0, r1 - r0
    px, py = fx - x0, fy - y0
    a = cdx * cdx + cdy * cdy - dr * dr
    b = px * cdx + py * cdy + r0 * dr
    cc = px * px + py * py - r0 * r0
    if abs(a) < 1e-12:
        s = cc / (2 * b) if b else None
        return s
    disc = b * b - a * cc
    if disc < 0:
        return None
    for s in sorted([(b + math.sqrt(disc)) / a, (b - math.sqrt(disc)) / a], reverse=True):
        if r0 + s * dr >= 0:
            return s
    return None


def inverse(m):
    a, b, c, d, e, f = m
    det = a * d - b * c
    return [d / det, -b / det, -c / det, a / det, (c * f - d * e) / det, (b * e - a * f) / det]


def form_point(op, x_pt, y_pt):
    return apply(op["_inv"], x_pt * SP_PER_BP, y_pt * SP_PER_BP)


def gradient_step_px(op, x_pt, y_pt, half):
    """Largest color change within half a pixel (page space) of a pixel center."""
    c = color_at(op, param(op, *form_point(op, x_pt, y_pt)))
    worst = 0.0
    for dx, dy in ((half, 0), (-half, 0), (0, half), (0, -half)):
        n = color_at(op, param(op, *form_point(op, x_pt + dx, y_pt + dy)))
        if c is None or n is None:
            return math.inf
        worst = max(worst, max(abs(a - b) for a, b in zip(c, n)))
    return worst


def color_at(op, s):
    if s is None:
        return None
    if s < 0:
        if not op["extend"][0]:
            return None
        s = 0.0
    if s > 1:
        if not op["extend"][1]:
            return None
        s = 1.0
    stops = op["stops"]
    for (t0, c0), (t1, c1) in zip(stops, stops[1:]):
        if t0 <= s <= t1:
            u = 0 if t1 == t0 else (s - t0) / (t1 - t0)
            return [a + u * (b - a) for a, b in zip(rgb(c0["comps"]), rgb(c1["comps"]))]
    return rgb(stops[-1][1]["comps"])


def main():
    pdf, dump = sys.argv[1], sys.argv[2]
    dpi = int(sys.argv[sys.argv.index("--dpi") + 1]) if "--dpi" in sys.argv else 600
    sabotage = "--sabotage" in sys.argv
    doc = pymupdf.open(pdf)
    bad = shades = samples_total = 0
    for line in open(dump):
        e = json.loads(line)
        if not e["ok"]:
            continue
        ops = e["page"]["ops"]
        if not any(o["op"] == "Shade" for o in ops):
            continue
        pno = int(re.search(r"page(\d+)\.json$", e["file"]).group(1))
        page = doc[pno - 1]
        pix = page.get_pixmap(dpi=dpi, alpha=False)
        scale = dpi / 72.0
        flags = pymupdf.TEXTFLAGS_RAWDICT & ~pymupdf.TEXT_CLIP
        glyph_boxes = [pymupdf.Rect(c["bbox"]) + (-1, -1, 1, 1)
                       for b in page.get_text("rawdict", flags=flags)["blocks"] for l in b.get("lines", [])
                       for sp in l["spans"] for c in sp["chars"]]
        clips = [[]]
        for k, op in enumerate(ops):
            if op["op"] == "Save":
                clips.append(list(clips[-1]))
            elif op["op"] == "Restore":
                clips.pop()
            elif op["op"] == "Clip":
                clips[-1].append((flatten(op["path"], op["ctm"]), op["even_odd"]))
            elif op["op"] == "Shade":
                shades += 1
                if sabotage:
                    op["stops"] = [(1 - t, c) for t, c in reversed(op["stops"])]
                # paths painted after the shading: stay clear of their edges, and of the inside of fills
                later = [(flatten(o["path"], o["ctm"]), o["fill"] is not None) for o in ops[k + 1:] if o["op"] == "Paint"]
                # and of shadings painted after it (their box)
                for o in ops[k + 1:]:
                    if o["op"] == "Shade":
                        a0, b0, a1, b1 = o["bbox"]
                        later.append(([[to_pt(o["ctm"], a0, b0), to_pt(o["ctm"], a1, b0), to_pt(o["ctm"], a1, b1), to_pt(o["ctm"], a0, b1)]], True))
                op["_inv"] = inverse(op["ctm"])
                x0, y0, x1, y1 = op["bbox"]
                worst, n, worst_at = 0.0, 0, None
                why = {"clip": 0, "later paths": 0, "glyphs": 0, "outside the shading": 0, "steep": 0}
                G = 48
                for i in range(1, G):
                    for j in range(1, G):
                        fx, fy = x0 + (x1 - x0) * i / G, y0 + (y1 - y0) * j / G
                        px, py = to_pt(op["ctm"], fx, fy)
                        if not all(inside(p, eo, px, py) and dist_to_edges(p, px, py) > 0.75 for p, eo in clips[-1]):
                            why["clip"] += 1
                            continue
                        if any(dist_to_edges(p, px, py) < 1 or (filled and inside(p, False, px, py)) for p, filled in later):
                            why["later paths"] += 1
                            continue
                        if any(r.contains((px, py)) for r in glyph_boxes):
                            why["glyphs"] += 1
                            continue
                        # the pixel containing the point, judged at its center
                        ix, iy = int(px * scale), int(py * scale)
                        cx, cy = (ix + 0.5) / scale, (iy + 0.5) / scale
                        s0 = param(op, *form_point(op, cx, cy))
                        want = color_at(op, s0)
                        if want is None:
                            continue
                        if gradient_step_px(op, cx, cy, 0.5 / scale) > TOL:
                            why["steep"] += 1
                            continue
                        got = [v / 255 for v in pix.pixel(ix, iy)[:3]]
                        a = op["alpha"]
                        want = [a * c + (1 - a) for c in want]
                        errs = [max(abs(p - q) for p, q in zip(want, got))]
                        if min(errs) > worst:
                            worst = min(errs)
                            worst_at = (round(px, 2), round(py, 2), round(s0, 4), [round(v, 3) for v in got],
                                        [round(v, 3) for v in color_at(op, s0)])
                        n += 1
                samples_total += n
                ok = worst <= TOL and n > 0
                bad += not ok
                print(f"page {pno} shade #{k}: {'OK' if ok else 'DIFFERS'} {'radial' if op['radial'] else 'axial'} "
                      f"samples {n} worst channel error {worst:.3f}" + ("" if n else f"; excluded {why}")
                      + (f"; worst at pt {worst_at[:2]} s {worst_at[2]} pdf {worst_at[3]} exact {worst_at[4]}" if not ok and n else ""))
    print(f"shadings {shades}, samples {samples_total}, differing {bad}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
