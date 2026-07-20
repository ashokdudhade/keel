#!/usr/bin/env bash
# Real-world accuracy benchmark: popular GitHub repos × Keel languages.
#
# Clones (shallow) one popular repo per language, indexes with Keel, scores
# definition lookup for curated gold symbols against a naive grep baseline,
# then writes reports/realworld-accuracy-benchmark.{html,json}.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${KEEL_BENCH_CACHE:-$ROOT/benchmarks/realworld/cache}"
MANIFEST="$ROOT/benchmarks/realworld/repos.json"
GOLD="$ROOT/benchmarks/realworld/gold.json"
OUT_DIR="$ROOT/reports"
OUT_HTML="$OUT_DIR/realworld-accuracy-benchmark.html"
OUT_JSON="$OUT_DIR/realworld-accuracy-benchmark.json"

KEEL_BIN="${KEEL_BIN:-}"
if [ -z "$KEEL_BIN" ]; then
  if [ -x "$ROOT/keel/target/release/keel" ]; then
    KEEL_BIN="$ROOT/keel/target/release/keel"
  else
    # Prefer CARGO_TARGET_DIR release binary when present.
    CAND="$(find "${CARGO_TARGET_DIR:-$ROOT/keel/target}" -path '*/release/keel' -type f 2>/dev/null | head -1 || true)"
    if [ -n "${CAND:-}" ] && [ -x "$CAND" ]; then
      KEEL_BIN="$CAND"
    elif command -v keel >/dev/null 2>&1; then
      KEEL_BIN="$(command -v keel)"
    else
      echo "Building keel (release)…"
      DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}" \
        cargo build --release --manifest-path "$ROOT/keel/Cargo.toml"
      KEEL_BIN="$ROOT/keel/target/release/keel"
    fi
  fi
fi

mkdir -p "$CACHE" "$OUT_DIR"

echo "Using keel: $KEEL_BIN"
echo "Cloning / updating repos…"

python3 - "$MANIFEST" "$CACHE" <<'PY'
import json, os, subprocess, sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
cache = Path(sys.argv[2])
cache.mkdir(parents=True, exist_ok=True)

for repo in manifest["repos"]:
    dest = cache / repo["id"]
    url = repo["url"]
    ref = repo.get("ref", "HEAD")
    if (dest / ".git").is_dir():
        print(f"  refresh {repo['id']} @ {ref}")
        subprocess.run(["git", "-C", str(dest), "fetch", "--depth", "1", "origin", ref], check=False)
        subprocess.run(["git", "-C", str(dest), "checkout", "-q", "FETCH_HEAD"], check=False)
    else:
        print(f"  clone  {repo['id']} <- {url} @ {ref}")
        if dest.exists():
            import shutil
            shutil.rmtree(dest)
        # Prefer tagged/shallow clone when ref looks like a tag.
        cmd = ["git", "clone", "--depth", "1"]
        if ref and ref != "HEAD":
            cmd += ["--branch", ref]
        cmd += [url, str(dest)]
        subprocess.run(cmd, check=True)
    head = subprocess.check_output(["git", "-C", str(dest), "rev-parse", "--short", "HEAD"], text=True).strip()
    print(f"         {repo['id']} => {head}")
PY

echo "Building / refreshing gold locations from clones (when needed)…"
# Gold is curated; verify files exist.
python3 - "$GOLD" "$CACHE" <<'PY'
import json, sys
from pathlib import Path
gold = json.loads(Path(sys.argv[1]).read_text())
cache = Path(sys.argv[2])
missing = []
for q in gold["queries"]:
    repo = cache / q["repo"]
    for d in q["definitions"]:
        p = repo / d["file"]
        if not p.is_file():
            missing.append(f"{q['repo']}:{d['file']}")
if missing:
    print("ERROR: gold files missing:\n  " + "\n  ".join(missing))
    sys.exit(1)
print(f"  gold ok: {len(gold['queries'])} queries across {len({q['repo'] for q in gold['queries']})} repos")
PY

echo "Indexing and scoring…"
python3 - "$MANIFEST" "$GOLD" "$CACHE" "$KEEL_BIN" "$OUT_JSON" "$OUT_HTML" <<'PY'
import json, re, subprocess, sys, time
from collections import defaultdict
from pathlib import Path

manifest_path, gold_path, cache, keel_bin, out_json, out_html = sys.argv[1:7]
manifest = json.loads(Path(manifest_path).read_text())
gold = json.loads(Path(gold_path).read_text())
cache = Path(cache)
repos_by_id = {r["id"]: r for r in manifest["repos"]}

EXTS = {".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".go", ".py", ".pyi"}

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

def grep_definitions(repo_root: Path, symbol: str):
    pats = [
        rf"\b(fn|func|function|class|struct|type|def|async def|export (default )?function|export class)\s+{re.escape(symbol)}\b",
        rf"\b(pub\s+)?(struct|enum|trait|fn|type)\s+{re.escape(symbol)}\b",
        rf"\b{re.escape(symbol)}\s*=\s*(async\s*)?(\(|function\b)",
        rf"\b(const|let|var)\s+{re.escape(symbol)}\s*=",
    ]
    hits = set()
    for path in repo_root.rglob("*"):
        if not path.is_file() or path.suffix not in EXTS:
            continue
        # Skip vendored / huge generated trees for fair-ish grep cost.
        parts = set(path.parts)
        if parts & {"node_modules", "vendor", "target", ".git", "dist", "build", "__pycache__"}:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except Exception:
            continue
        rel = path.relative_to(repo_root).as_posix()
        for i, line in enumerate(text.splitlines(), 1):
            for pat in pats:
                if re.search(pat, line):
                    hits.add(f"{rel}:{i}")
                    break
    return hits

def keel_definitions(repo_root: Path, symbol: str):
    out = subprocess.check_output(
        [keel_bin, "definition", symbol],
        cwd=repo_root,
        text=True,
    )
    hits = set()
    for line in out.splitlines():
        loc = line.split("\t", 1)[0]
        parts = loc.rsplit(":", 2)
        if len(parts) >= 2:
            hits.add(f"{parts[0]}:{parts[1]}")
    return hits

# Index each repo once.
index_stats = {}
for repo_id, meta in repos_by_id.items():
    root = cache / repo_id
    t0 = time.time()
    proc = subprocess.run(
        [keel_bin, "index", "."],
        cwd=root,
        capture_output=True,
        text=True,
    )
    elapsed = time.time() - t0
    if proc.returncode != 0:
        raise SystemExit(f"keel index failed for {repo_id}:\n{proc.stderr}\n{proc.stdout}")
    index_stats[repo_id] = {
        "seconds": round(elapsed, 3),
        "stdout": proc.stdout.strip(),
        "language": meta["language"],
        "url": meta["url"],
        "ref": meta.get("ref"),
        "description": meta.get("description", ""),
    }
    print(f"  indexed {repo_id} in {elapsed:.2f}s — {proc.stdout.strip()}")

rows = []
agg = {
    "without_keel": {"tp": 0, "fp": 0, "fn": 0},
    "with_keel": {"tp": 0, "fp": 0, "fn": 0},
}
by_lang = defaultdict(lambda: {
    "without_keel": {"tp": 0, "fp": 0, "fn": 0},
    "with_keel": {"tp": 0, "fp": 0, "fn": 0},
    "queries": 0,
})

for q in gold["queries"]:
    repo_id = q["repo"]
    root = cache / repo_id
    symbol = q["symbol"]
    language = q["language"]
    truth = {f"{t['file']}:{t['line']}" for t in q["definitions"]}
    g = score(grep_definitions(root, symbol), truth)
    k = score(keel_definitions(root, symbol), truth)
    for side, s in (("without_keel", g), ("with_keel", k)):
        agg[side]["tp"] += s["tp"]
        agg[side]["fp"] += s["fp"]
        agg[side]["fn"] += s["fn"]
        by_lang[language][side]["tp"] += s["tp"]
        by_lang[language][side]["fp"] += s["fp"]
        by_lang[language][side]["fn"] += s["fn"]
    by_lang[language]["queries"] += 1
    rows.append({
        "repo": repo_id,
        "language": language,
        "symbol": symbol,
        "note": q.get("note", ""),
        "without_keel": g,
        "with_keel": k,
    })
    print(f"  {language:12} {symbol:24} grep F1={g['f1']:.2f}  keel F1={k['f1']:.2f}")

summary = {
    "without_keel": finalize(agg["without_keel"]),
    "with_keel": finalize(agg["with_keel"]),
    "queries": len(gold["queries"]),
    "repos": len(repos_by_id),
}
lang_summary = {
    lang: {
        "queries": data["queries"],
        "without_keel": finalize(data["without_keel"]),
        "with_keel": finalize(data["with_keel"]),
    }
    for lang, data in sorted(by_lang.items())
}

report = {
    "title": "Keel real-world accuracy benchmark",
    "summary": summary,
    "by_language": lang_summary,
    "index_stats": index_stats,
    "repos": repos_by_id,
    "rows": rows,
}
Path(out_json).write_text(json.dumps(report, indent=2) + "\n")

def pct(x):
    return f"{100.0 * x:.1f}%"

wo, wk = summary["without_keel"], summary["with_keel"]

lang_rows = []
for lang, data in lang_summary.items():
    a, b = data["without_keel"], data["with_keel"]
    lang_rows.append(
        "<tr>"
        f"<td>{lang}</td><td>{data['queries']}</td>"
        f"<td>{pct(a['precision'])}</td><td>{pct(a['recall'])}</td><td>{pct(a['f1'])}</td>"
        f"<td>{pct(b['precision'])}</td><td>{pct(b['recall'])}</td><td>{pct(b['f1'])}</td>"
        "</tr>"
    )

query_rows = []
for r in rows:
    query_rows.append(
        "<tr>"
        f"<td>{r['language']}</td><td><code>{r['repo']}</code></td>"
        f"<td><code>{r['symbol']}</code></td>"
        f"<td>{pct(r['without_keel']['f1'])}</td>"
        f"<td>{pct(r['with_keel']['f1'])}</td>"
        f"<td>{r['note']}</td>"
        "</tr>"
    )

repo_cards = []
for repo_id, st in index_stats.items():
    meta = repos_by_id[repo_id]
    repo_cards.append(
        f"<li><strong>{meta['language']}</strong> — "
        f"<a href=\"{meta['url']}\">{repo_id}</a> "
        f"({meta.get('ref','')}) · indexed in {st['seconds']}s · {st['stdout']}</li>"
    )

html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Keel real-world accuracy benchmark</title>
<style>
  :root {{ --bg:#101820; --panel:#1b2530; --text:#e8eef4; --muted:#9aa7b5; --good:#3dd68c; --bad:#f07178; --accent:#5ccfe6; --line:#2a3542; }}
  body {{ margin:0; font:16px/1.5 "Source Sans 3", "IBM Plex Sans", system-ui, sans-serif;
    background:
      radial-gradient(900px 480px at 0% 0%, #1e3348 0%, transparent 55%),
      radial-gradient(700px 420px at 100% 10%, #243028 0%, transparent 50%),
      var(--bg);
    color:var(--text); }}
  main {{ max-width:1040px; margin:0 auto; padding:48px 24px 88px; }}
  h1 {{ font:700 2.4rem/1.1 "Fraunces", "IBM Plex Serif", Georgia, serif; margin:0 0 10px; letter-spacing:-0.02em; }}
  h2 {{ font:650 1.25rem/1.2 "Fraunces", Georgia, serif; margin:36px 0 12px; }}
  .lede {{ color:var(--muted); max-width:46rem; }}
  .cards {{ display:grid; grid-template-columns:1fr 1fr; gap:16px; margin:28px 0; }}
  .card {{ background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:22px; }}
  .card h3 {{ margin:0 0 10px; font-size:.8rem; color:var(--muted); text-transform:uppercase; letter-spacing:.08em; }}
  .metric {{ font-size:2.2rem; font-weight:700; }}
  .good {{ color:var(--good); }} .bad {{ color:var(--bad); }}
  table {{ width:100%; border-collapse:collapse; background:var(--panel); border-radius:12px; overflow:hidden; margin:12px 0 8px; }}
  th, td {{ padding:10px 12px; text-align:left; border-bottom:1px solid var(--line); vertical-align:top; }}
  th {{ color:var(--muted); font-size:.82rem; font-weight:600; }}
  code {{ color:var(--accent); font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size:.92em; }}
  a {{ color:var(--accent); }}
  ul.repos {{ color:var(--muted); padding-left:1.2rem; }}
  footer {{ margin-top:32px; color:var(--muted); font-size:.9rem; }}
  @media (max-width:720px) {{ .cards {{ grid-template-columns:1fr; }} }}
</style>
</head>
<body>
<main>
  <h1>Real-world definition accuracy</h1>
  <p class="lede">
    Curated gold symbols from popular GitHub repositories — one per Keel language.
    Baseline is keyword/identifier grep; Keel uses the structural index
    (<code>keel definition</code>). {summary['queries']} queries · {summary['repos']} repos.
  </p>

  <div class="cards">
    <section class="card">
      <h3>Without Keel (grep)</h3>
      <div class="metric bad">{pct(wo['f1'])} F1</div>
      <div>Precision {pct(wo['precision'])} · Recall {pct(wo['recall'])}</div>
      <div>TP {wo['tp']} · FP {wo['fp']} · FN {wo['fn']}</div>
    </section>
    <section class="card">
      <h3>With Keel</h3>
      <div class="metric good">{pct(wk['f1'])} F1</div>
      <div>Precision {pct(wk['precision'])} · Recall {pct(wk['recall'])}</div>
      <div>TP {wk['tp']} · FP {wk['fp']} · FN {wk['fn']}</div>
    </section>
  </div>

  <h2>Repositories</h2>
  <ul class="repos">
    {''.join(repo_cards)}
  </ul>

  <h2>By language</h2>
  <table>
    <thead>
      <tr>
        <th>Language</th><th>Queries</th>
        <th>Grep P</th><th>Grep R</th><th>Grep F1</th>
        <th>Keel P</th><th>Keel R</th><th>Keel F1</th>
      </tr>
    </thead>
    <tbody>
      {''.join(lang_rows)}
    </tbody>
  </table>

  <h2>Per-symbol results</h2>
  <table>
    <thead>
      <tr>
        <th>Language</th><th>Repo</th><th>Symbol</th>
        <th>Grep F1</th><th>Keel F1</th><th>Notes</th>
      </tr>
    </thead>
    <tbody>
      {''.join(query_rows)}
    </tbody>
  </table>

  <footer>
    Generated by <code>scripts/realworld-accuracy-benchmark.sh</code>.
    Higher F1 is better. Gold locations are hand-verified public API symbols.
  </footer>
</main>
</body>
</html>
"""
Path(out_html).write_text(html)
print(json.dumps({"summary": summary, "by_language": {
    k: {"grep_f1": v["without_keel"]["f1"], "keel_f1": v["with_keel"]["f1"]}
    for k, v in lang_summary.items()
}}, indent=2))
print(f"Wrote {out_html}")
print(f"Wrote {out_json}")
PY
