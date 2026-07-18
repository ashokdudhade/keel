# SecondBrain

## Deterministic Code Intelligence for AI Coding Agents

### Executive Summary

Modern AI coding assistants are excellent at generating code but still
struggle with one fundamental problem: **understanding large codebases
accurately**.

Current approaches rely primarily on semantic search and repository
maps, which work well for natural language but are less reliable for
structural questions such as:

-   Where is this interface implemented?
-   What breaks if I modify this API?
-   Which services depend on this module?
-   Is this symbol actually referenced?
-   What is the complete dependency chain?

SecondBrain is an **open source, local-first code intelligence engine**
that answers these questions deterministically using AST parsing and
symbol graphs instead of embedding similarity.

Rather than replacing AI coding assistants, SecondBrain becomes the
infrastructure powering them.

## Vision

Build the missing infrastructure layer between source code and AI
agents.

``` text
        Claude Code
        Cursor
        Codex CLI
        Continue
        OpenHands
             │
      MCP / JSON API
             │
    --------------------
      SecondBrain Core
    --------------------
             │
      Symbol Graph
      Dependency Graph
      AST Index
      Incremental Updates
             │
      Tree-sitter
      Stack Graphs
      SQLite
```

SecondBrain provides **deterministic repository knowledge** that any
coding agent can consume.

## Core Goals

SecondBrain focuses on one responsibility:

> Build the most accurate, language-aware representation of a software
> repository.

It intentionally does **not** generate code. It provides trusted
context.

## Features

### 1. Incremental Repository Indexing

Continuously parse repositories using Tree-sitter. Only modified files
are reprocessed.

Goals:

-   Local-first
-   Incremental updates
-   Low memory footprint
-   Fast startup
-   Large monorepo support

### 2. Deterministic Symbol Graph

Unlike vector search, SecondBrain understands actual program structure.

Example queries:

``` text
Find definition(AuthService)
Find references(User)
Find implementations(Storage)
Find callers(createOrder)
Find dependencies(PaymentService)
Find impact(AuthTrait)
```

### 3. Language-Agnostic Architecture

Language support is modular.

``` text
languages/
    rust/
    typescript/
    go/
    python/
    java/
    kotlin/
```

Each language contributes:

-   Tree-sitter grammar
-   Symbol extraction rules
-   Stack Graph mappings

### 4. Multiple Interfaces

#### Rust Library

``` rust
let graph = Index::open("./repo")?;

graph.find_definition("AuthService");
graph.find_references("User");
graph.find_callers("create_order");
graph.find_impact("PaymentAPI");
```

#### CLI

``` bash
sb definition AuthService
sb callers PaymentService
sb impact UserRepository
```

#### MCP Server

Expose repository intelligence through MCP while keeping MCP as an
interface rather than the product.

#### JSON API

``` http
GET /symbol/AuthService
```

``` json
{
  "definition": "...",
  "references": [],
  "implementations": [],
  "dependencies": [],
  "callers": []
}
```

## Technical Stack

  Component             Technology
  --------------------- ---------------------
  Language              Rust
  Parsing               Tree-sitter
  Symbol Resolution     GitHub Stack Graphs
  Storage               SQLite
  Parallel Processing   Rayon
  Protocol              MCP + JSON
  File Watching         notify

## What SecondBrain Is Not

It does **not**:

-   Generate code
-   Rewrite pull requests
-   Fix CI failures
-   Create ADRs
-   Replace coding assistants

## Roadmap

### v0.1

-   Rust support
-   Tree-sitter indexing
-   Symbol graph
-   CLI
-   SQLite storage

### v0.2

-   Incremental indexing
-   Cross-file references
-   Dependency graph
-   JSON API

### v0.3

-   MCP server
-   TypeScript support
-   Go support

### v1.0

-   Multi-language repositories
-   Impact analysis
-   Plugin system
-   Stable APIs
-   Documentation

## Success Metrics

Technical:

-   Incremental indexing under 500 ms for typical file changes
-   Support repositories with more than 1 million lines of code
-   Accurate cross-file symbol resolution
-   Low memory footprint

Adoption:

-   Integration with major AI coding assistants
-   Community-contributed language plugins
-   Reuse as a library by open-source developer tools

## Why Open Source?

Code intelligence is foundational infrastructure, not a product moat.
The goal is for SecondBrain to become the standard open-source
foundation for deterministic code intelligence, similar to how
Tree-sitter became the standard for incremental parsing.
