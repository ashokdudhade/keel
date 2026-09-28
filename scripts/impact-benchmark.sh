#!/usr/bin/env bash
# Impact benchmark: Keel transitive impact recall on a per-language fixture.
#
# Each language ships a call chain (direct + transitive + method callers);
# gold pins the exact impacted def lines. There is no grep baseline: textual
# search cannot compute transitive impact. Writes
# reports/impact-benchmark.{html,json} (and prints a summary).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIX="$ROOT/benchmarks/impact/fixture"
GOLD="$ROOT/benchmarks/impact/gold.json"
OUT_DIR="$ROOT/reports"
OUT_HTML="$OUT_DIR/impact-benchmark.html"
OUT_JSON="$OUT_DIR/impact-benchmark.json"

KEEL_BIN="${KEEL_BIN:-}"
if [ -z "$KEEL_BIN" ]; then
  if [ -x "$ROOT/keel/target/release/keel" ]; then
    KEEL_BIN="$ROOT/keel/target/release/keel"
  elif command -v keel >/dev/null 2>&1; then
    KEEL_BIN="$(command -v keel)"
  else
    echo "Building keel (release)…"
    DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}" \
      cargo build --release --manifest-path "$ROOT/keel/Cargo.toml"
    KEEL_BIN="$ROOT/keel/target/release/keel"
  fi
fi

mkdir -p "$OUT_DIR"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -R "$FIX" "$WORK/repo"
cd "$WORK/repo"

"$KEEL_BIN" index . >/dev/null

python3 - "$GOLD" "$KEEL_BIN" "$OUT_JSON" "$OUT_HTML" <<'PY'
import json, subprocess, sys
from pathlib import Path

gold_path, keel_bin, out_json, out_html = sys.argv[1:5]
gold = json.loads(Path(gold_path).read_text())
queries = gold["queries"]

def score(preds, truth):
    """preds/truth: set of 'path:line' strings."""
    tp = len(preds & truth)
    fp = len(preds - truth)
    fn = len(truth - preds)
    precision = tp / (tp + fp) if (tp + fp) else 1.0
    recall = tp / (tp + fn) if (tp + fn) else 1.0
    f1 = (2 * precision * recall / (precision + recall)) if (precision + recall) else 0.0
    return {
        "tp": tp, "fp": fp, "fn": fn,
        "precision": precision, "recall": recall, "f1": f1,
        "predicted": sorted(preds), "expected": sorted(truth),
    }

def keel_impact(symbol, module):
    cmd = [keel_bin, "impact", symbol, "--json"]
    if module:
        cmd += ["--module", module]
    out = subprocess.check_output(cmd, text=True)
    hits = set()
    for r in json.loads(out)["results"]:
        hits.add(f"{r['file']}:{r['start_line']}")
    return hits

rows = []
agg = {"tp": 0, "fp": 0, "fn": 0}

for q in queries:
    truth = {f"{t['file']}:{t['line']}" for t in q["impacted"]}
    s = score(keel_impact(q["symbol"], q.get("module")), truth)
    agg["tp"] += s["tp"]
    agg["fp"] += s["fp"]
    agg["fn"] += s["fn"]
    rows.append({
        "symbol": q["symbol"], "language": q["language"],
        "module": q.get("module"), "keel": s,
    })

def finalize(c):
    tp, fp, fn = c["tp"], c["fp"], c["fn"]
    precision = tp / (tp + fp) if (tp + fp) else 1.0
    recall = tp / (tp + fn) if (tp + fn) else 1.0
    f1 = (2 * precision * recall / (precision + recall)) if (precision + recall) else 0.0
    return {**c, "precision": precision, "recall": recall, "f1": f1}

summary = {"keel": finalize(agg), "queries": len(queries)}
report = {"summary": summary, "rows": rows}
Path(out_json).write_text(json.dumps(report, indent=2) + "\n")

def pct(x):
    return f"{100.0 * x:.1f}%"

k = summary["keel"]
rows_html = []
for r in rows:
    mod = f"<code>{r['module']}</code>" if r["module"] else "<i>bare</i>"
    rows_html.append(
        "<tr>"
        f"<td>{r['language']}</td><td><code>{r['symbol']}</code></td><td>{mod}</td>"
        f"<td>{pct(r['keel']['precision'])}</td><td>{pct(r['keel']['recall'])}</td><td>{pct(r['keel']['f1'])}</td>"
        "</tr>"
    )

html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Keel impact benchmark</title>
<style>
  :root {{ --bg:#0f1419; --panel:#1a222c; --text:#e7ecf1; --muted:#9aa7b5; --good:#3dd68c; --bad:#f07178; --accent:#5ccfe6; }}
  body {{ margin:0; font:16px/1.5 "IBM Plex Sans", system-ui, sans-serif; background:radial-gradient(1200px 600px at 10% -10%, #1d2a38, var(--bg)); color:var(--text); }}
  main {{ max-width:960px; margin:0 auto; padding:48px 24px 80px; }}
  h1 {{ font:700 2.2rem/1.1 "IBM Plex Serif", Georgia, serif; margin:0 0 8px; }}
  .lede {{ color:var(--muted); max-width:42rem; }}
  .cards {{ display:grid; grid-template-columns:1fr; gap:16px; margin:32px 0; max-width:320px; }}
  .card {{ background:var(--panel); border:1px solid #2a3542; border-radius:12px; padding:20px; }}
  .card h2 {{ margin:0 0 12px; font-size:1rem; color:var(--muted); text-transform:uppercase; letter-spacing:.06em; }}
  .metric {{ font-size:2rem; font-weight:700; }}
  .good {{ color:var(--good); }} .bad {{ color:var(--bad); }}
  table {{ width:100%; border-collapse:collapse; background:var(--panel); border-radius:12px; overflow:hidden; }}
  th, td {{ padding:10px 12px; text-align:left; border-bottom:1px solid #2a3542; }}
  th {{ color:var(--muted); font-size:.85rem; }}
  code {{ color:var(--accent); }}
</style>
</head>
<body>
<main>
  <h1>Keel impact benchmark</h1>
  <p class="lede">Transitive impact recall per language (direct + transitive + method callers). No grep baseline: textual search cannot compute transitive impact.</p>
  <div class="cards">
    <div class="card"><h2>Keel F1</h2><div class="metric good">{pct(k['f1'])}</div>
    <div>precision {pct(k['precision'])} · recall {pct(k['recall'])} · {summary['queries']} queries</div></div>
  </div>
  <table>
    <tr><th>Language</th><th>Symbol</th><th>Module</th><th>Precision</th><th>Recall</th><th>F1</th></tr>
    {''.join(rows_html)}
  </table>
</main>
</body>
</html>
"""
Path(out_html).write_text(html)
print(json.dumps(summary, indent=2))
print(f"Wrote {out_html}")
# CI gate: any false positive/negative fails the run.
if k["f1"] < 1.0:
    print(f"GATE FAILED: impact F1 {k['f1']:.4f} < 1.0 (fp={k['fp']}, fn={k['fn']})", file=sys.stderr)
    raise SystemExit(1)
print(f"GATE PASSED: impact F1 1.0 across {summary['queries']} queries")
PY
