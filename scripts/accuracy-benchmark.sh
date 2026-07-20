#!/usr/bin/env bash
# Accuracy benchmark: naive grep vs Keel on a multi-language fixture.
#
# Measures definition lookup precision/recall against a gold set, then writes
# reports/accuracy-benchmark.html (and prints a summary).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIX="$ROOT/benchmarks/accuracy/fixture"
GOLD="$ROOT/benchmarks/accuracy/gold.json"
OUT_DIR="$ROOT/reports"
OUT_HTML="$OUT_DIR/accuracy-benchmark.html"
OUT_JSON="$OUT_DIR/accuracy-benchmark.json"

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

def grep_definitions(symbol):
    # Naive baseline: any line matching `symbol` as a whole word that also
    # looks like a definition keyword. Intentionally imperfect.
    pats = [
        rf"\b(fn|func|function|class|struct|type|def|async def|export (default )?function|export class)\s+{re.escape(symbol)}\b",
        rf"\b(pub\s+)?(struct|enum|trait|fn)\s+{re.escape(symbol)}\b",
        rf"\b{re.escape(symbol)}\s*=\s*(async\s*)?\(",
    ]
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
            for pat in pats:
                if re.search(pat, line):
                    hits.add(f"{path.as_posix()}:{i}")
                    break
    return hits

def keel_definitions(symbol):
    out = subprocess.check_output([keel_bin, "definition", symbol], text=True)
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
    truth = {f"{t['file']}:{t['line']}" for t in q["definitions"]}
    g = score(grep_definitions(symbol), truth)
    k = score(keel_definitions(symbol), truth)
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
<title>Keel accuracy benchmark</title>
<style>
  :root {{ --bg:#0f1419; --panel:#1a222c; --text:#e7ecf1; --muted:#9aa7b5; --good:#3dd68c; --bad:#f07178; --accent:#5ccfe6; }}
  body {{ margin:0; font:16px/1.5 "IBM Plex Sans", system-ui, sans-serif; background:radial-gradient(1200px 600px at 10% -10%, #1d2a38, var(--bg)); color:var(--text); }}
  main {{ max-width:960px; margin:0 auto; padding:48px 24px 80px; }}
  h1 {{ font:700 2.2rem/1.1 "IBM Plex Serif", Georgia, serif; margin:0 0 8px; }}
  .lede {{ color:var(--muted); max-width:42rem; }}
  .cards {{ display:grid; grid-template-columns:1fr 1fr; gap:16px; margin:32px 0; }}
  .card {{ background:var(--panel); border:1px solid #2a3542; border-radius:12px; padding:20px; }}
  .card h2 {{ margin:0 0 12px; font-size:1rem; color:var(--muted); text-transform:uppercase; letter-spacing:.06em; }}
  .metric {{ font-size:2rem; font-weight:700; }}
  .good {{ color:var(--good); }} .bad {{ color:var(--bad); }}
  table {{ width:100%; border-collapse:collapse; background:var(--panel); border-radius:12px; overflow:hidden; }}
  th, td {{ padding:10px 12px; text-align:left; border-bottom:1px solid #2a3542; }}
  th {{ color:var(--muted); font-size:.85rem; }}
  code {{ color:var(--accent); }}
  footer {{ margin-top:28px; color:var(--muted); font-size:.9rem; }}
</style>
</head>
<body>
<main>
  <h1>Definition lookup accuracy</h1>
  <p class="lede">Gold-labeled multi-language fixture. Baseline is keyword+identifier grep;
  Keel uses the structural index (<code>keel definition</code>). {summary['queries']} queries.</p>
  <div class="cards">
    <section class="card">
      <h2>Without Keel (grep)</h2>
      <div class="metric bad">{pct(wo['f1'])} F1</div>
      <div>Precision {pct(wo['precision'])} · Recall {pct(wo['recall'])}</div>
      <div>TP {wo['tp']} · FP {wo['fp']} · FN {wo['fn']}</div>
    </section>
    <section class="card">
      <h2>With Keel</h2>
      <div class="metric good">{pct(wk['f1'])} F1</div>
      <div>Precision {pct(wk['precision'])} · Recall {pct(wk['recall'])}</div>
      <div>TP {wk['tp']} · FP {wk['fp']} · FN {wk['fn']}</div>
    </section>
  </div>
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
  <footer>Generated by <code>scripts/accuracy-benchmark.sh</code>. Higher F1 is better.</footer>
</main>
</body>
</html>
"""
Path(out_html).write_text(html)
print(json.dumps(summary, indent=2))
print(f"Wrote {out_html}")
PY
