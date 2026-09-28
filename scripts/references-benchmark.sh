#!/usr/bin/env bash
# References benchmark: whole-word grep vs Keel module-scoped references on a
# multi-language fixture.
#
# Measures use-site precision/recall against a gold set, then writes
# reports/references-benchmark.html (and prints a summary).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIX="$ROOT/benchmarks/references/fixture"
GOLD="$ROOT/benchmarks/references/gold.json"
OUT_DIR="$ROOT/reports"
OUT_HTML="$OUT_DIR/references-benchmark.html"
OUT_JSON="$OUT_DIR/references-benchmark.json"

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
import json, re, subprocess, sys
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

def grep_uses(symbol):
    # Naive baseline: any line matching `symbol` as a whole word.
    # Catches definitions, imports, comments, strings, writes, and
    # cross-module shadows alike — intentionally imprecise.
    pat = re.compile(rf"\b{re.escape(symbol)}\b")
    hits = set()
    for path in Path(".").rglob("*"):
        if not path.is_file():
            continue
        if path.suffix not in {".rs", ".ts", ".tsx", ".js", ".jsx", ".go", ".py", ".pyi"}:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except Exception:
            continue
        for i, line in enumerate(text.splitlines(), 1):
            if pat.search(line):
                hits.add(f"{path.as_posix()}:{i}")
    return hits

def keel_uses(symbol, module):
    out = subprocess.check_output(
        [keel_bin, "references", symbol, "--module", module], text=True
    )
    hits = set()
    for line in out.splitlines():
        # path:line:col\tkind\tname
        loc = line.split("\t", 1)[0]
        parts = loc.rsplit(":", 2)
        if len(parts) >= 2:
            hits.add(f"{parts[0]}:{parts[1]}")
    return hits

rows = []
agg = {
    "without_keel": {"tp": 0, "fp": 0, "fn": 0},
    "with_keel": {"tp": 0, "fp": 0, "fn": 0},
}

for q in queries:
    symbol = q["symbol"]
    truth = {f"{t['file']}:{t['line']}" for t in q["uses"]}
    g = score(grep_uses(symbol), truth)
    k = score(keel_uses(symbol, q["module"]), truth)
    for side, s in (("without_keel", g), ("with_keel", k)):
        agg[side]["tp"] += s["tp"]
        agg[side]["fp"] += s["fp"]
        agg[side]["fn"] += s["fn"]
    rows.append({"symbol": symbol, "language": q["language"], "without_keel": g, "with_keel": k})

def finalize(c):
    tp, fp, fn = c["tp"], c["fp"], c["fn"]
    precision = tp / (tp + fp) if (tp + fp) else 1.0
    recall = tp / (tp + fn) if (tp + fn) else 1.0
    f1 = (2 * precision * recall / (precision + recall)) if (precision + recall) else 0.0
    return {**c, "precision": precision, "recall": recall, "f1": f1}

summary = {
    "without_keel": finalize(agg["without_keel"]),
    "with_keel": finalize(agg["with_keel"]),
    "queries": len(queries),
}

report = {"summary": summary, "rows": rows}
Path(out_json).write_text(json.dumps(report, indent=2) + "\n")

def pct(x):
    return f"{100.0 * x:.1f}%"

wo, wk = summary["without_keel"], summary["with_keel"]
rows_html = []
for r in rows:
    rows_html.append(
        "<tr>"
        f"<td>{r['language']}</td><td><code>{r['symbol']}</code></td>"
        f"<td>{pct(r['without_keel']['precision'])}</td><td>{pct(r['without_keel']['recall'])}</td><td>{pct(r['without_keel']['f1'])}</td>"
        f"<td>{pct(r['with_keel']['precision'])}</td><td>{pct(r['with_keel']['recall'])}</td><td>{pct(r['with_keel']['f1'])}</td>"
        "</tr>"
    )

html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Keel references benchmark</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 2rem; color: #111; }}
table {{ border-collapse: collapse; }}
th, td {{ border: 1px solid #ccc; padding: 0.35rem 0.7rem; text-align: left; }}
th {{ background: #f4f4f4; }}
.lede {{ color: #444; max-width: 60rem; }}
footer {{ margin-top: 1rem; color: #666; }}
</style>
</head>
<body>
<main>
  <h1>Keel references benchmark</h1>
  <p class="lede">Module-scoped use-site lookup: whole-word grep vs Keel <code>references --module</code>
  across {summary['queries']} queries. Grep: P {pct(wo['precision'])} R {pct(wo['recall'])} F1 {pct(wo['f1'])}.
  Keel: P {pct(wk['precision'])} R {pct(wk['recall'])} F1 {pct(wk['f1'])}.</p>
  <table>
    <thead>
      <tr>
        <th>Language</th><th>Symbol</th>
        <th>Grep P</th><th>Grep R</th><th>Grep F1</th>
        <th>Keel P</th><th>Keel R</th><th>Keel F1</th>
      </tr>
    </thead>
    <tbody>
      {''.join(rows_html)}
    </tbody>
  </table>
  <footer>Generated by <code>scripts/references-benchmark.sh</code>. Higher F1 is better.</footer>
</main>
</body>
</html>
"""
Path(out_html).write_text(html)
print(json.dumps(summary, indent=2))
print(f"Wrote {out_html}")
# CI gate: the grep baseline is informational; Keel must be perfect.
if wk["f1"] < 1.0:
    print(f"GATE FAILED: references F1 {wk['f1']:.4f} < 1.0 (fp={wk['fp']}, fn={wk['fn']})", file=sys.stderr)
    raise SystemExit(1)
print(f"GATE PASSED: references F1 1.0 across {summary['queries']} queries")
PY
