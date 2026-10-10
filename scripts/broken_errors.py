"""Error reporting on the TeX stress test's `broken` suite: for every case, the first error rtex
reports (status, file:line, message: what an editor's problem list shows) against a plain lualatex
run of the same file (-file-line-error), and the suite's own expectation (mostly pdfLaTeX).

Usage: python3 scripts/broken_errors.py STRESS_ROOT OUT [CASE_ID ...]
  needs target/release/examples/replay (cargo build --release -p rtex-core --example replay)
Writes OUT/report.json and prints a Markdown table."""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

root, out = sys.argv[1], sys.argv[2]
only = set(sys.argv[3:])
suite = os.path.join(root, "suites", "broken")
cases = json.load(open(os.path.join(suite, "cases.json")))["cases"]
replay = os.path.abspath("target/release/examples/replay")
os.makedirs(out, exist_ok=True)
LOC = re.compile(r"^(.*?):(\d+): (.*)$")


def lualatex(case, work):
    """Plain lualatex: (status, first error (file, line, message) or None)."""
    main = case["main"]
    try:
        subprocess.run(["lualatex", "-interaction=nonstopmode", "-file-line-error", main], cwd=work,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=min(case.get("timeout", 60), 60))
    except subprocess.TimeoutExpired:
        return "timeout", None
    log = os.path.join(work, os.path.splitext(main)[0] + ".log")
    text = open(log, errors="replace").read() if os.path.exists(log) else ""
    first = None
    errors = 0
    for line in text.splitlines():
        m = LOC.match(line)
        if m and not line.startswith(" "):
            errors += 1
            if first is None:
                first = (m.group(1), int(m.group(2)), m.group(3))
        elif line.startswith("! ") and first is None:
            errors += 1
            first = (None, None, line[2:])
    pdf = os.path.join(work, os.path.splitext(main)[0] + ".pdf")
    status = "fatal" if not os.path.exists(pdf) else ("error" if errors else "ok")
    return status, first


def rtex(case, work, tmp):
    o = os.path.join(tmp, "rtex")
    try:
        subprocess.run([replay, "check", "--project", work, "--main", case["main"], "--out", o, "--timeout", "90"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=180)
    except subprocess.TimeoutExpired:
        return {"status": "harness-timeout", "errors": []}
    try:
        return json.load(open(os.path.join(o, "report.json")))
    except Exception as e:
        return {"status": f"no report ({e})", "errors": []}


def norm(f):
    if f is None:
        return None
    f = f[2:] if f.startswith("./") else f
    return os.path.basename(f) if "/build/" in f or f.startswith("/") else f


rows = []
for case in cases:
    if only and case["id"] not in only:
        continue
    tmp = tempfile.mkdtemp(prefix="broken-")
    work = os.path.join(tmp, "ref")
    shutil.copytree(suite, work, symlinks=True)
    t = time.time()
    ref_status, ref_first = lualatex(case, work)
    shutil.rmtree(work)
    shutil.copytree(suite, work, symlinks=True)
    r = rtex(case, work, tmp)
    errs = r.get("errors") or []
    first = errs[0] if errs else None
    loc = (norm(first["file"]), first["line"]) if first else None
    ref_loc = (norm(ref_first[0]), ref_first[1]) if ref_first else None
    exp = case["expect"]
    row = {
        "id": case["id"], "engine": case.get("engine", "pdflatex"),
        "expect_status": exp.get("status"), "expect_location": exp.get("error_location"),
        "lualatex_status": ref_status, "lualatex_location": ref_loc, "lualatex_message": ref_first[2] if ref_first else None,
        "rtex_status": r.get("status"), "rtex_location": loc,
        "rtex_message": (first or {}).get("message", "")[:200] if first else None,
        "rtex_errors": len(errs), "rtex_wall_ms": r.get("wall_ms"), "secs": round(time.time() - t, 1),
    }
    row["status_match"] = row["rtex_status"] == ref_status
    row["location_match"] = (loc == ref_loc) if ref_loc and ref_loc[0] else None
    rows.append(row)
    print(f"{case['id']:40} lualatex {ref_status:7} {str(ref_loc):32} rtex {str(row['rtex_status']):7} {str(loc):32}"
          f" {'' if row['status_match'] else 'STATUS'} {'' if row['location_match'] in (True, None) else 'LOCATION'}", flush=True)
    shutil.rmtree(tmp, ignore_errors=True)

json.dump(rows, open(os.path.join(out, "report.json"), "w"), indent=1)
n = len(rows)
print(f"\n{n} cases: status as lualatex {sum(r['status_match'] for r in rows)}/{n}; "
      f"first error location as lualatex {sum(1 for r in rows if r['location_match'])}"
      f"/{sum(1 for r in rows if r['location_match'] is not None)}")
