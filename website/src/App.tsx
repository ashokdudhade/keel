import { useState, type ReactNode } from "react";
import "./App.css";

type PanelId = "config" | "prompts";

const mcpConfig = `{
  "mcpServers": {
    "keel": {
      "command": "/absolute/path/to/keel",
      "args": ["mcp"],
      "env": {
        "KEEL_INDEX_DB": "/absolute/path/to/project/.keel/index.db"
      }
    }
  }
}`;

const cursorPrompts = `Where is AuthService defined?
Who references create_order?
Who calls create_order?
What is impacted if WireFormat changes?`;

const claudePrompts = `Where is LanguagePlugin defined?
Who references read_message?
What implements LanguagePlugin?
What is impacted if Registry changes?`;

const tools: { name: string; summary: string }[] = [
  {
    name: "definition",
    summary: "Definition location(s) for a symbol name",
  },
  {
    name: "references",
    summary: "Reference sites for a name",
  },
  {
    name: "callers",
    summary:
      "Call/use sites; import-aware when a unique definition module is known",
  },
  {
    name: "implementations",
    summary: "Rust trait implementations for a trait name",
  },
  {
    name: "dependencies",
    summary: "Modules/files a module or symbol depends on",
  },
  {
    name: "impact",
    summary: "Symbols transitively impacted by changing a name",
  },
  {
    name: "index",
    summary: "Index a repository path; returns indexing stats",
  },
];

const languages = [
  "Rust",
  "TypeScript / TSX",
  "JavaScript / JSX",
  "Python",
  "Go",
] as const;

function Terminal({ label, code }: { label: string; code: string }) {
  return (
    <div className="terminal">
      <div className="terminal__bar" aria-hidden="true">
        <span />
        <span />
        <span />
        <em>{label}</em>
      </div>
      <pre>
        <code>{code}</code>
      </pre>
    </div>
  );
}

function AgentCard({
  title,
  badge,
  blurb,
  configLabel,
  config,
  prompts,
}: {
  title: string;
  badge: string;
  blurb: ReactNode;
  configLabel: string;
  config: string;
  prompts: string;
}) {
  const [panel, setPanel] = useState<PanelId>("config");
  const baseId = title.toLowerCase().replace(/\s+/g, "-");

  return (
    <article className="agent">
      <div className="agent__head">
        <h3>{title}</h3>
        <span className="agent__badge">{badge}</span>
      </div>
      <p>{blurb}</p>
      <div className="tabs" role="tablist" aria-label={`${title} examples`}>
        <button
          type="button"
          role="tab"
          id={`${baseId}-tab-config`}
          className="tab"
          aria-selected={panel === "config"}
          aria-controls={`${baseId}-panel`}
          onClick={() => setPanel("config")}
        >
          Config
        </button>
        <button
          type="button"
          role="tab"
          id={`${baseId}-tab-prompts`}
          className="tab"
          aria-selected={panel === "prompts"}
          aria-controls={`${baseId}-panel`}
          onClick={() => setPanel("prompts")}
        >
          Example prompts
        </button>
      </div>
      <div
        id={`${baseId}-panel`}
        role="tabpanel"
        aria-labelledby={
          panel === "config"
            ? `${baseId}-tab-config`
            : `${baseId}-tab-prompts`
        }
      >
        {panel === "config" ? (
          <Terminal label={configLabel} code={config} />
        ) : (
          <Terminal label="chat" code={prompts} />
        )}
      </div>
    </article>
  );
}

export default function App() {
  return (
    <>
      <header className="hero">
        <div className="hero__glow" aria-hidden="true" />
        <div className="hero__texture" aria-hidden="true" />

        <div className="hero__inner">
          <nav className="nav" aria-label="Primary">
            <a className="nav__mark" href="#top">
              <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path
                  d="M3 19h18M5 19C8.5 10 15.5 10 19 19"
                  stroke="currentColor"
                  strokeWidth="1.75"
                />
              </svg>
              Keel
            </a>
            <div className="nav__links">
              <a href="#tools">Tools</a>
              <a href="#install">Install</a>
              <a href="#mcp">MCP</a>
              <a href="https://github.com/ashokdudhade/keel">GitHub</a>
            </div>
          </nav>

          <div className="hero__stage" id="top">
            <div className="hero__copy">
              <p className="hero__eyebrow">Local index · MCP · CLI</p>
              <p className="hero__brand">Keel</p>
              <h1 className="hero__headline">
                Local code intelligence for AI coding agents
              </h1>
              <p className="hero__lede">
                Indexes your repository with Tree-sitter and answers structural
                queries from a local on-disk index—no LLMs, embeddings, or cloud
                index. Use it from Cursor, Claude Code, any MCP client, or the
                CLI.
              </p>
              <div className="cta-row">
                <a className="btn btn--solid" href="#install">
                  Install
                </a>
                <a
                  className="btn btn--line"
                  href="https://github.com/ashokdudhade/keel#quick-start-cursor"
                >
                  Quick start
                </a>
              </div>
            </div>

            <div className="hero__visual" aria-hidden="true">
              <div className="hero__panel">
                <div className="hero__panel-bar">
                  <span className="hero__dots">
                    <i />
                    <i />
                    <i />
                  </span>
                  <em>terminal</em>
                </div>
                <pre className="hero__panel-body">
                  <code>
                    <span className="tok-cmd">$ keel definition AuthService</span>
                    {"\n"}
                    <span className="tok-path">src/auth/service.rs</span>
                    <span className="tok-dim">:12:12</span>
                    {"  "}
                    <span className="tok-kind">struct</span>
                    {"  "}
                    <span className="tok-sym">AuthService</span>
                    {"\n\n"}
                    <span className="tok-cmd">$ keel callers create_order</span>
                    {"\n"}
                    <span className="tok-path">src/api/orders.rs</span>
                    <span className="tok-dim">:88:14</span>
                    {"  "}
                    <span className="tok-kind">call</span>
                    {"  "}
                    <span className="tok-sym">create_order</span>
                    {"\n"}
                    <span className="tok-path">src/jobs/fulfill.rs</span>
                    <span className="tok-dim">:41:7</span>
                    {"  "}
                    <span className="tok-kind">call</span>
                    {"  "}
                    <span className="tok-sym">create_order</span>
                  </code>
                </pre>
              </div>
            </div>
          </div>
        </div>
      </header>

      <section id="why">
        <div className="wrap">
          <p className="kicker">What it is</p>
          <h2 className="title">
            Name-based structural search, not semantic search
          </h2>
          <p className="lede">
            Grep finds text. Language servers resolve types in an IDE. Keel is a
            persistent symbol index your agent queries by name. For a given
            index state, answers are stable and repeatable.
          </p>
          <ul className="why-list">
            <li>
              <span className="num">01</span>
              <h3>Deterministic</h3>
              <p>
                Same index, same answers. Results come from the local index—not
                model recall, embeddings, or a remote knowledge base.
              </p>
            </li>
            <li>
              <span className="num">02</span>
              <h3>Local-first</h3>
              <p>
                Parsing and the index stay on your machine. Default path:{" "}
                <span className="mono">.keel/index.db</span>. Add{" "}
                <span className="mono">.keel/</span> to{" "}
                <span className="mono">.gitignore</span>.
              </p>
            </li>
            <li>
              <span className="num">03</span>
              <h3>Agent-ready</h3>
              <p>
                Primary interface: <span className="mono">keel mcp</span>. Same
                queries via CLI. Optional JSON API bound to{" "}
                <span className="mono">127.0.0.1</span>.
              </p>
            </li>
          </ul>
        </div>
      </section>

      <section className="tools" id="tools">
        <div className="wrap">
          <p className="kicker">MCP tools</p>
          <h2 className="title">Seven tools over one index</h2>
          <p className="lede">
            Prefer Keel when you know a symbol or trait name; use text search
            for regex. <span className="mono">callers</span> is import-aware
            when a unique definition module is known.{" "}
            <span className="mono">implementations</span> covers Rust traits
            today.
          </p>
          <ul className="tool-list">
            {tools.map((tool) => (
              <li key={tool.name}>
                <code>{tool.name}</code>
                <span>{tool.summary}</span>
              </li>
            ))}
          </ul>
          <p className="lang-label">
            Languages indexed in one pass (mixed monorepos supported)
          </p>
          <ul className="langs">
            {languages.map((lang) => (
              <li key={lang}>{lang}</li>
            ))}
          </ul>
        </div>
      </section>

      <section className="install" id="install">
        <div className="wrap">
          <p className="kicker">Get started</p>
          <h2 className="title">Install, daemon, index, then MCP</h2>
          <p className="lede">
            Homebrew on macOS is the smoothest path (PATH +{" "}
            <span className="mono">brew services</span>). Elsewhere, the curl
            installer detects OS/arch, verifies SHA-256, installs to{" "}
            <span className="mono">~/.local/bin</span>, and updates shell
            profiles—open a new terminal afterward. Native Windows is not
            supported; use WSL2.
          </p>

          <div className="install-grid">
            <div className="install-block">
              <Terminal
                label="Homebrew (macOS)"
                code={`brew tap ashokdudhade/keel https://github.com/ashokdudhade/keel
brew install ashokdudhade/keel/keel
brew services start keel
cd /path/to/project && keel start
keel status`}
              />
            </div>
            <div className="install-block">
              <Terminal
                label="curl (macOS / Linux / WSL2)"
                code={`curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
# new terminal, then:
keel daemon
cd /path/to/project && keel start
keel status`}
              />
            </div>
          </div>

          <p className="install__note">
            <span className="mono">keel start</span> registers the project,
            builds <span className="mono">.keel/index.db</span>, and watches for
            changes while the global daemon is running. Without the daemon:{" "}
            <span className="mono">keel index .</span> or{" "}
            <span className="mono">keel watch .</span>.
          </p>
          <p className="install__note">
            CLI: <span className="mono">keel definition AuthService</span>,{" "}
            <span className="mono">keel callers create_order</span>. The
            crates.io name <span className="mono">keel</span> is taken—use
            GitHub binaries or Homebrew, not{" "}
            <span className="mono">cargo install keel</span> from crates.io.
          </p>

          <div className="cta-row">
            <a
              className="btn btn--sea"
              href="https://github.com/ashokdudhade/keel"
            >
              GitHub repository
            </a>
            <a
              className="btn btn--line-dark"
              href="https://github.com/ashokdudhade/keel#quick-start-cursor"
            >
              Full quick start
            </a>
          </div>
        </div>
      </section>

      <section className="agents" id="mcp">
        <div className="wrap">
          <p className="kicker">Wire into your agent</p>
          <h2 className="title">MCP config for Cursor and Claude Code</h2>
          <p className="lede">
            Point <span className="mono">command</span> at an absolute{" "}
            <span className="mono">keel</span> path (
            <span className="mono">which keel</span>; expand{" "}
            <span className="mono">~</span>). Set{" "}
            <span className="mono">KEEL_INDEX_DB</span> to the project index.
            Prefer <span className="mono">env</span> over{" "}
            <span className="mono">cwd</span>—Cursor often ignores working
            directory. Refresh MCP after saving; you should see all seven tools.
          </p>

          <div className="agent-grid">
            <AgentCard
              title="Cursor"
              badge="mcp.json"
              blurb={
                <>
                  Global <span className="mono">~/.cursor/mcp.json</span> or
                  project <span className="mono">.cursor/mcp.json</span>.
                  Optional: a project rule so the agent prefers Keel for
                  structural lookups without being asked.
                </>
              }
              configLabel="mcp.json"
              config={mcpConfig}
              prompts={cursorPrompts}
            />
            <AgentCard
              title="Claude Code"
              badge="mcpServers"
              blurb={
                <>
                  Same <span className="mono">mcpServers</span> entry and
                  binary. Point <span className="mono">KEEL_INDEX_DB</span> at
                  that project's{" "}
                  <span className="mono">.keel/index.db</span>.
                </>
              }
              configLabel="mcpServers"
              config={mcpConfig}
              prompts={claudePrompts}
            />
          </div>
        </div>
      </section>

      <footer>
        <div className="wrap">
          <div>
            <strong>Keel</strong> — deterministic, local-first code intelligence
            for AI coding agents.
          </div>
          <nav aria-label="Footer">
            <a href="https://github.com/ashokdudhade/keel">GitHub</a>
            <a href="https://github.com/ashokdudhade/keel/blob/main/README.md">
              README
            </a>
            <a href="#install">Install</a>
          </nav>
        </div>
      </footer>
    </>
  );
}
