# Keel repo guidance for Claude Code

This repo builds **Keel** (see `AGENTS.md` for architecture, build, and
contributor conventions). Keel is also usable *in* this repo via MCP: when the
`keel` MCP server is registered, prefer its tools for structural questions
about this repository's code — do not wait to be told to use them.

## Use Keel first for

- Where is X defined? → `definition`
- Who references / uses X? → `references`
- Who calls X? → `callers`
- What implements trait T? → `implementations`
- What depends on X? → `dependencies`
- Who depends on X? → `dependents` (module, directory, file, or symbol)
- What breaks if X changes? → `impact`
- What does F define? → `outline` (file, module, or directory)
- What symbols mention P? → `search` (substring, case-insensitive, exact first)
- What code is dead? → `unused` (optional file/module/dir scope; candidates only)

Pass the exact, case-sensitive symbol or module name. Prefer Keel over Grep
for these intents.

## Tune calls, skip round-trips

- Pass `"preview": true` on hit tools (`definition`, `references`,
  `callers`, `implementations`, `impact`, `outline`, `search`, `unused`)
  to get each hit's source line inline — no follow-up Read just to see
  context.
- Pass `"limit": N` on broad queries (`search`, `impact`); capped replies
  name the true total in `notes`.

## Read the trust envelope

Keel returns JSON with `results`, `confidence` (`high` | `medium` | `low`),
`resolution_tier` (1 = strongest evidence), and `notes`. Always read those fields before acting.

- **Empty + "No matching symbols found"** → confident miss. Try another exact
  name, `module`, or a qualified name (`crate::mcp::serve`). Do not treat
  this as "Keel is unreliable."
- **`confidence: low` or ambiguity notes** → do not treat hits as exclusive
  truth. Disambiguate with the `module` argument or a qualified `name`, then
  retry. Fall back to Grep only if still ambiguous.
- **`impact` with multi-def / over-approx notes** → treat as a candidate blast
  radius, not a delete list. Narrow with `module` / qualified name before edits.

## Fall back to Grep / Read when

- Keel returns empty after a qualified / module-disambiguated retry
- Searching comments, docs, config, strings, or unknown substrings / regex
- The symbol name is unknown and must be discovered from text first
- Results look clearly wrong after reading `notes`

Register the server with: `claude mcp add keel -e KEEL_INDEX_DB=/absolute/path/to/repo/.keel/index.db -- /absolute/path/to/keel mcp` (or run `keel init` for the exact commands)
