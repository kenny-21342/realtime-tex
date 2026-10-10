"""Summarize scripts/stress-replay.sh output (OUT/<run>/report.json) as Markdown.
Usage: python3 scripts/stress_summary.py OUT"""
import json
import pathlib
import sys

out = pathlib.Path(sys.argv[1])


def ms(v):
    return "-" if v is None or v != v else f"{v:.1f}"


def stat(s):
    return f"{ms(s['p50'])} / {ms(s['p95'])} / {ms(s['max'])}" if s and s.get("n") else "-"


rows, steps = [], []
for rep in sorted(out.glob("*/report.json")):
    r = json.loads(rep.read_text())
    s = r["summary"]
    v = s["verdicts"]
    routes = ", ".join(f"{k} {n}" for k, n in s["edits_by_route"].items())
    rows.append(
        f"| {rep.parent.name} | {s['open'].get('pages')} | {ms(s['open'].get('first_run_ended_ms'))} | {routes} "
        f"| {stat(s['fast_result_ms'])} | {stat(s['fast_engine_ms'])} | {stat(s['settle_ms'])} "
        f"| {v.get('match', 0)} | {v.get('MISMATCH', 0)} | {sum(n for k, n in v.items() if k not in ('match', 'MISMATCH'))} |"
    )
    if r["mode"] == "ops":
        for st in r["steps"]:
            ref = st.get("reference", {}).get("expect", {})
            got = st.get("settled", {})
            errs = got.get("errors") or []
            loc = errs[0]["location"] if errs else None
            loc = loc[2:] if loc and loc.startswith("./") else loc
            same = ref.get("status") == got.get("status")
            ref_loc = ref.get("error_location")
            loc_ok = "" if not ref_loc else ("ok" if loc == ref_loc else f"{loc} (ref {ref_loc})")
            steps.append(
                f"| {r['script']} | {st['step']} {st['label']} | {ref.get('status', '-')} | {got.get('status', '-')}"
                f"{'' if same or not ref else ' **≠**'} | {loc_ok} | {got.get('pages')} / {ref.get('pages', '-')} "
                f"| {'ok' if st['files_match_script'] else '**DIFFER**'} |"
            )

print("# Stress replay\n")
print("Times in ms, p50 / p95 / max. *fast result*: host-observed, apply_edit to the paragraph event; "
      "*engine*: the engine's own time; *settle*: last edit to the layout covering it. Verdicts compare the "
      "last result served per paragraph with the settled layout.\n")
print("| run | pages | open (first run) | edits by route | fast result | engine | settle | match | MISMATCH | other |")
print("|---|---|---|---|---|---|---|---|---|---|")
print("\n".join(rows))
if steps:
    print("\n## Edit scenarios, step by step (reference: the suite's recorded expectation, mostly pdfLaTeX)\n")
    print("| scenario | step | ref status | rtex status | first error location | pages rtex / ref | files |")
    print("|---|---|---|---|---|---|---|")
    print("\n".join(steps))
