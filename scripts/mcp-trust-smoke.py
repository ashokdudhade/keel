#!/usr/bin/env python3
"""Agent-trust MCP smoke test for Keel.

Resolves the same binary as scripts/keel-mcp.sh (newest workspace build),
exercises the trust envelope, and exits non-zero on regressions that poison agents.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INDEX = ROOT / ".keel" / "index.db"


def resolve_keel() -> Path:
    env_bin = os.environ.get("KEEL_BIN")
    if env_bin and Path(env_bin).is_file() and os.access(env_bin, os.X_OK):
        return Path(env_bin)
    candidates = [
        ROOT / "keel" / "target" / "release" / "keel",
        ROOT / "keel" / "target" / "debug" / "keel",
        Path("/opt/homebrew/bin/keel"),
        Path("/usr/local/bin/keel"),
    ]
    newest: Path | None = None
    for c in candidates[:2]:
        if c.is_file() and os.access(c, os.X_OK):
            if newest is None or c.stat().st_mtime > newest.stat().st_mtime:
                newest = c
    if newest:
        return newest
    for c in candidates[2:]:
        if c.is_file() and os.access(c, os.X_OK):
            return c
    which = subprocess.run(["command", "-v", "keel"], capture_output=True, text=True)
    if which.returncode == 0 and which.stdout.strip():
        return Path(which.stdout.strip())
    sys.exit("keel binary not found; build with: (cd keel && cargo build)")


def mcp_call(bin_path: Path, msgs: list[dict]) -> tuple[list[dict], str]:
    env = os.environ.copy()
    env["KEEL_INDEX_DB"] = str(INDEX)
    env["KEEL_MCP_DEBUG"] = "1"
    stdin = "".join(json.dumps(m) + "\n" for m in msgs)
    proc = subprocess.run(
        [str(bin_path), "mcp"],
        input=stdin,
        text=True,
        capture_output=True,
        env=env,
        timeout=90,
        cwd=str(ROOT),
    )
    if proc.returncode != 0:
        sys.exit(f"keel mcp exited {proc.returncode}\nstderr:\n{proc.stderr}")
    out = []
    for line in proc.stdout.splitlines():
        if line.strip():
            out.append(json.loads(line))
    return out, proc.stderr


def payload_from(resp: dict) -> dict:
    text = resp["result"]["content"][0]["text"]
    return json.loads(text)


def main() -> None:
    if not INDEX.is_file():
        sys.exit(f"missing index at {INDEX}; run: keel index .")

    bin_path = resolve_keel()
    print(f"using {bin_path}")

    msgs = [
        {
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "keel-trust-smoke", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {"name": "NonexistentSymbolXYZ123"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {"name": "serve"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {"name": "crate::mcp::serve"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {"name": "serve", "module": "crate::mcp"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "impact",
                "arguments": {"name": "WireFormat"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "dependencies",
                "arguments": {"name": "crate::mcp"},
            },
        },
    ]

    responses, stderr = mcp_call(bin_path, msgs)
    by_id = {r["id"]: r for r in responses if "id" in r}

    failures: list[str] = []

    tools = by_id[1]["result"]["tools"]
    names = [t["name"] for t in tools]
    for required in (
        "definition",
        "references",
        "callers",
        "implementations",
        "dependencies",
        "impact",
        "index",
    ):
        if required not in names:
            failures.append(f"missing tool {required}")
    definition = next(t for t in tools if t["name"] == "definition")
    if "confidence" not in definition.get("description", ""):
        failures.append("definition tool description missing confidence guidance")
    if "module" not in definition.get("inputSchema", {}).get("properties", {}):
        failures.append("definition missing module argument")
    if not definition.get("annotations", {}).get("readOnlyHint"):
        failures.append("definition missing readOnlyHint annotation")

    miss = payload_from(by_id[2])
    if miss.get("results"):
        failures.append(f"expected empty miss, got {miss['results']}")
    if miss.get("confidence") != "high":
        failures.append(f"miss confidence want high got {miss.get('confidence')}")
    if miss.get("notes") != ["No matching symbols found."]:
        failures.append(f"miss notes lie or wrong: {miss.get('notes')}")
    if any("name-only fallback" in n for n in miss.get("notes", [])):
        failures.append("empty miss still claims name-only fallback")

    multi = payload_from(by_id[3])
    if len(multi.get("results", [])) < 2:
        failures.append(f"serve should be multi-def, got {multi}")
    if any("over-approximate impact" in n for n in multi.get("notes", [])):
        failures.append("definition multi-def still bleeds impact note")
    if not any("disambiguate" in n for n in multi.get("notes", [])):
        failures.append("multi-def missing disambiguate guidance")

    qualified = payload_from(by_id[4])
    if len(qualified.get("results", [])) != 1:
        failures.append(f"qualified serve want 1 hit got {qualified}")
    elif qualified["results"][0].get("module_path") != "crate::mcp":
        failures.append(f"qualified wrong module {qualified}")

    mod_arg = payload_from(by_id[5])
    if len(mod_arg.get("results", [])) != 1:
        failures.append(f"module arg serve want 1 hit got {mod_arg}")

    impact = payload_from(by_id[6])
    if impact.get("confidence") == "high" and impact.get("results"):
        failures.append("non-empty impact must not be high confidence")
    if impact.get("results") and not any(
        "candidate blast radius" in n for n in impact.get("notes", [])
    ):
        failures.append("impact missing candidate blast radius note")

    deps = payload_from(by_id[7])
    if not deps.get("results"):
        failures.append(f"dependencies crate::mcp empty: {deps}")
    if deps.get("confidence") != "high":
        failures.append(f"deps confidence want high got {deps.get('confidence')}")

    print("stderr:", stderr.strip()[:500])
    if failures:
        print("FAIL:")
        for f in failures:
            print(" -", f)
        sys.exit(1)
    print("PASS: agent trust envelope OK")


if __name__ == "__main__":
    main()
