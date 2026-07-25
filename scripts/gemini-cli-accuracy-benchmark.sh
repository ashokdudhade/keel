#!/usr/bin/env bash
# Benchmark Keel on google-gemini/gemini-cli.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${KEEL_BENCH_CACHE:-$ROOT/benchmarks/realworld/cache}"
REPO_ID="google-gemini-gemini-cli"
REPO_DIR="$CACHE/$REPO_ID"
GOLD="$ROOT/benchmarks/realworld/gemini-cli-gold.json"
OUT_DIR="$ROOT/reports"
OUT_HTML="$OUT_DIR/gemini-cli-accuracy-benchmark.html"
OUT_JSON="$OUT_DIR/gemini-cli-accuracy-benchmark.json"
SSH_URL="git@github.com:google-gemini/gemini-cli.git"

KEEL_BIN="${KEEL_BIN:-}"
if [ -z "$KEEL_BIN" ]; then
  if [ -x "$ROOT/keel/target/release/keel" ]; then
    KEEL_BIN="$ROOT/keel/target/release/keel"
  else
    CAND="$(find "${CARGO_TARGET_DIR:-$ROOT/keel/target}" -path '*/release/keel' -type f 2>/dev/null | head -1 || true)"
    if [ -n "${CAND:-}" ] && [ -x "$CAND" ]; then
      KEEL_BIN="$CAND"
    else
      echo "Building keel (release)…"
      DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}" \
        cargo build --release --manifest-path "$ROOT/keel/Cargo.toml"
      KEEL_BIN="$ROOT/keel/target/release/keel"
    fi
  fi
fi

mkdir -p "$CACHE" "$OUT_DIR"

if [ ! -d "$REPO_DIR/.git" ]; then
  echo "Cloning $SSH_URL …"
  git clone --depth 1 "$SSH_URL" "$REPO_DIR"
else
  echo "Using existing clone at $REPO_DIR"
fi

HEAD="$(git -C "$REPO_DIR" rev-parse --short HEAD)"
echo "HEAD=$HEAD"
echo "Indexing with $KEEL_BIN …"

python3 - "$REPO_DIR" "$GOLD" "$KEEL_BIN" "$OUT_JSON" "$OUT_HTML" "$HEAD" <<'PY'
import json, re, subprocess, sys, time
from pathlib import Path

repo, gold_path, keel_bin, out_json, out_html, head = sys.argv[1:7]
repo = Path(repo)
gold = json.loads(Path(gold_path).read_text())
EXTS = {".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".pyi", ".go", ".rs"}

def score(preds, truth):
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

def finalize(c):
    tp, fp, fn = c["tp"], c["fp"], c["fn"]
    precision = tp / (tp + fp) if (tp + fp) else 1.0
    recall = tp / (tp + fn) if (tp + fn) else 1.0
    f1 = (2 * precision * recall / (precision + recall)) if (precision + recall) else 0.0
    return {**c, "precision": precision, "recall": recall, "f1": f1}

def grep_definitions(symbol: str):
    pats = [
        rf"\b(export\s+)?(default\s+)?(abstract\s+)?(class|interface|enum|type|function|const)\s+{re.escape(symbol)}\b",
        rf"\b(export\s+)?(async\s+)?function\s+{re.escape(symbol)}\b",
        rf"\b{re.escape(symbol)}\s*=\s*(async\s*)?(\(|function\b)",
    ]
    hits = set()
    for path in repo.rglob("*"):
        if not path.is_file() or path.suffix not in EXTS:
            continue
        if set(path.parts) & {"node_modules", "vendor", "target", ".git", "dist", "build", "coverage", ".keel"}:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except Exception:
            continue
        rel = path.relative_to(repo).as_posix()
        for i, line in enumerate(text.splitlines(), 1):
            for pat in pats:
                if re.search(pat, line):
                    hits.add(f"{rel}:{i}")
                    break
    return hits

def keel_definitions(symbol: str):
    out = subprocess.check_output([keel_bin, "definition", symbol], cwd=repo, text=True)
    hits = set()
    for line in out.splitlines():
        if line.startswith("No definition"):
            continue
        loc = line.split("\t", 1)[0]
        parts = loc.rsplit(":", 2)
        if len(parts) >= 2:
            hits.add(f"{parts[0]}:{parts[1]}")
    return hits

def keel_reference_count(symbol: str) -> int:
    out = subprocess.check_output([keel_bin, "references", symbol], cwd=repo, text=True)
    if out.strip().startswith("No "):
        return 0
    return len([l for l in out.splitlines() if l.strip()])

# Index
t0 = time.time()
proc = subprocess.run([keel_bin, "index", "."], cwd=repo, capture_output=True, text=True)
index_seconds = time.time() - t0
if proc.returncode != 0:
    raise SystemExit(proc.stderr or proc.stdout)
index_line = proc.stdout.strip()
print(f"  {index_line} ({index_seconds:.2f}s)")

# File counts
counts = {}
for path in repo.rglob("*"):
    if not path.is_file() or path.suffix not in EXTS:
        continue
    if set(path.parts) & {"node_modules", ".git", "dist", "build", "coverage", ".keel"}:
        continue
    counts[path.suffix] = counts.get(path.suffix, 0) + 1

rows = []
agg = {"without_keel": {"tp": 0, "fp": 0, "fn": 0}, "with_keel": {"tp": 0, "fp": 0, "fn": 0}}
for q in gold["queries"]:
    symbol = q["symbol"]
    truth = {f"{d['file']}:{d['line']}" for d in q["definitions"]}
    g = score(grep_definitions(symbol), truth)
    k = score(keel_definitions(symbol), truth)
    refs = keel_reference_count(symbol)
    for side, s in (("without_keel", g), ("with_keel", k)):
        agg[side]["tp"] += s["tp"]
        agg[side]["fp"] += s["fp"]
        agg[side]["fn"] += s["fn"]
    rows.append({
        "symbol": symbol,
        "language": q["language"],
        "note": q.get("note", ""),
        "without_keel": g,
        "with_keel": k,
        "keel_reference_count": refs,
    })
    print(f"  {symbol:28} grep F1={g['f1']:.2f}  keel F1={k['f1']:.2f}  refs={refs}")

summary = {
    "without_keel": finalize(agg["without_keel"]),
    "with_keel": finalize(agg["with_keel"]),
    "queries": len(gold["queries"]),
}
report = {
    "title": "Keel accuracy on google-gemini/gemini-cli",
    "repo": gold["repo"],
    "head": head,
    "index": {"seconds": round(index_seconds, 3), "stdout": index_line, "extension_counts": counts},
    "summary": summary,
    "rows": rows,
}
Path(out_json).write_text(json.dumps(report, indent=2) + "\n")

def pct(x):
    return f"{100.0 * x:.1f}%"

wo, wk = summary["without_keel"], summary["with_keel"]
ext_bits = ", ".join(f"{k[1:]}={v}" for k, v in sorted(counts.items(), key=lambda kv: -kv[1]))
query_rows = []
for r in rows:
    query_rows.append(
        "<tr>"
        f"<td><code>{r['symbol']}</code></td>"
        f"<td>{r['note']}</td>"
        f"<td>{pct(r['without_keel']['precision'])}</td><td>{pct(r['without_keel']['recall'])}</td><td>{pct(r['without_keel']['f1'])}</td>"
        f"<td>{pct(r['with_keel']['precision'])}</td><td>{pct(r['with_keel']['recall'])}</td><td>{pct(r['with_keel']['f1'])}</td>"
        f"<td>{r['keel_reference_count']}</td>"
        "</tr>"
    )

html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Keel × gemini-cli accuracy</title>
<style>
  :root {{ --bg:#0e151c; --panel:#18222c; --text:#e8eef4; --muted:#97a4b2; --good:#3dd68c; --bad:#f07178; --accent:#6ec1ff; --line:#2a3542; }}
  body {{ margin:0; font:16px/1.5 "Source Sans 3", system-ui, sans-serif;
    background: radial-gradient(1000px 520px at 8% -10%, #1c3348, transparent 55%), var(--bg); color:var(--text); }}
  main {{ max-width:1080px; margin:0 auto; padding:48px 24px 88px; }}
  h1 {{ font:700 2.3rem/1.1 "Fraunces", Georgia, serif; margin:0 0 10px; }}
  h2 {{ font:650 1.2rem/1.2 "Fraunces", Georgia, serif; margin:34px 0 12px; }}
  .lede {{ color:var(--muted); max-width:48rem; }}
  .cards {{ display:grid; grid-template-columns:1fr 1fr 1fr; gap:14px; margin:28px 0; }}
  .card {{ background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:20px; }}
  .card h3 {{ margin:0 0 8px; font-size:.78rem; color:var(--muted); text-transform:uppercase; letter-spacing:.08em; }}
  .metric {{ font-size:2rem; font-weight:700; }}
  .good {{ color:var(--good); }} .bad {{ color:var(--bad); }}
  table {{ width:100%; border-collapse:collapse; background:var(--panel); border-radius:12px; overflow:hidden; }}
  th, td {{ padding:10px 12px; text-align:left; border-bottom:1px solid var(--line); vertical-align:top; }}
  th {{ color:var(--muted); font-size:.8rem; }}
  code {{ color:var(--accent); font-family:ui-monospace, Menlo, monospace; font-size:.9em; }}
  a {{ color:var(--accent); }}
  footer {{ margin-top:28px; color:var(--muted); font-size:.9rem; }}
  @media (max-width:800px) {{ .cards {{ grid-template-columns:1fr; }} }}
</style>
</head>
<body>
<main>
  <h1>google-gemini/gemini-cli</h1>
  <p class="lede">
    Definition lookup accuracy on
    <a href="https://github.com/google-gemini/gemini-cli">google-gemini/gemini-cli</a>
    (<code>{head}</code>). Baseline is keyword grep; Keel uses the structural index.
    {summary['queries']} curated gold symbols · indexed files by extension: {ext_bits}.
  </p>

  <div class="cards">
    <section class="card">
      <h3>Without Keel (grep)</h3>
      <div class="metric bad">{pct(wo['f1'])} F1</div>
      <div>P {pct(wo['precision'])} · R {pct(wo['recall'])}</div>
      <div>TP {wo['tp']} · FP {wo['fp']} · FN {wo['fn']}</div>
    </section>
    <section class="card">
      <h3>With Keel</h3>
      <div class="metric good">{pct(wk['f1'])} F1</div>
      <div>P {pct(wk['precision'])} · R {pct(wk['recall'])}</div>
      <div>TP {wk['tp']} · FP {wk['fp']} · FN {wk['fn']}</div>
    </section>
    <section class="card">
      <h3>Index</h3>
      <div class="metric">{index_seconds:.1f}s</div>
      <div>{index_line}</div>
    </section>
  </div>

  <h2>Per-symbol results</h2>
  <table>
    <thead>
      <tr>
        <th>Symbol</th><th>Notes</th>
        <th>Grep P</th><th>Grep R</th><th>Grep F1</th>
        <th>Keel P</th><th>Keel R</th><th>Keel F1</th>
        <th>Keel refs</th>
      </tr>
    </thead>
    <tbody>
      {''.join(query_rows)}
    </tbody>
  </table>

  <footer>
    Generated by <code>scripts/gemini-cli-accuracy-benchmark.sh</code>.
    Higher F1 is better.
  </footer>
</main>
</body>
</html>
"""
Path(out_html).write_text(html)
print(json.dumps(summary, indent=2))
print(f"Wrote {out_html}")
print(f"Wrote {out_json}")
PY
