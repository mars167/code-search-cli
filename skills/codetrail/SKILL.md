---
name: codetrail
description: Use when searching, navigating, validating, or documenting the CodeTrail repository with the local codetrail CLI; especially when an agent needs reliable source evidence, saved query replay, freshness-aware index results, remote snapshot verification, or MCP/JSON command contracts.
---

# CodeTrail

Use `codetrail` for narrow, verifiable source evidence in this repository. Prefer JSON output for agent work, and verify important matches with `read` before editing.

## Boundary

CodeTrail is the search and navigation tool layer. It should not take over
task planning, decide when an investigation is complete, or produce final
task answers on its own.

For a single narrow lookup, call the CLI directly. For multi-step repository
investigations, delegate to a CodeTrail evidence subagent when one is
available, then use its compact evidence package in the main task. The
subagent owns query sequencing and compression; CodeTrail still only returns
source, navigation, relationship, status, and freshness facts.

## Command Prefix

Prefer the installed binary when available:

```bash
codetrail <command> ...
```

When the binary is not installed, run through Cargo from the repository root:

```bash
cargo run --quiet -- <command> ...
```

Use `--path <dir>` when searching from outside the repository root or when the user points at a different checkout.

## Core Workflow

1. Start with the narrowest command that can answer the question:
   - `codetrail find <literal>`
   - `codetrail grep <regex>`
   - `codetrail files <substring>`
   - `codetrail glob '<pattern>'`
   - `codetrail defs|refs|symbols <name>`
2. Inspect `reliability`, `index`, `warnings`, `suggestedReads`, and `nextActions`.
   - Treat `severity=info, category=capability` as an expected capability-level note, not a risk warning.
   - Treat `severity=warning, category=risk` and `severity=error, category=error` as requiring narrowing, verification, or remediation.
3. Before editing or making a strong claim, verify key ranges with `codetrail read <path[:start-end]>`.
4. Treat `calls` and `callers` as `inferred_candidate`; inspect the returned ranges before relying on them.
5. Treat `remote_unverified` as a lead only; verify with local `read`.

## Subagent Handoff

Use the repository's CodeTrail evidence subagent template for tasks that would
otherwise require a long loop of search/read/refine steps. Ask the subagent to
return only:

- the task it investigated;
- a short answer-oriented summary;
- path and line-range evidence;
- caveats about missing, ambiguous, stale, or inferred results;
- a concise query trace.

Every evidence location returned by the subagent must include a line number or
line range such as `src/lib.rs:12-40`. File-only paths are leads, not evidence.

Do not ask the subagent to edit files or make product decisions. Do not ask
the CodeTrail CLI to run task-specific analysis commands such as `brief`,
`context`, `analyze architecture`, or `analyze data-model`.

## Scope Controls

Use global options to keep output useful:

- `--include`, `--exclude`, `--lang`, and `--changed` narrow the search surface.
- `--limit`, `--cursor`, `--allow-broad`, and `--context` control paging and output budget.
- `--output json|compact-json|jsonl|text` selects the response shape; use `json` unless a human-readable transcript is requested.
- `--save-query <name>` records replay metadata for repeated investigations.

## Saved Queries

Use saved queries for repeatable investigations, not as a fact store.

```bash
codetrail find "needle" --include src --save-query needle-src
codetrail query replay needle-src
codetrail query replay needle-src --snapshot saved
codetrail query show needle-src
codetrail query list
codetrail query delete needle-src
```

Saved queries live in `.codetrail/queries/<name>.json` and store command, query, scope, snapshot, and cursor metadata. They do not store result bodies. If the current snapshot differs, default replay runs against the current workspace and warns; `--snapshot saved` rejects the mismatch.

## Index And Freshness

- `codetrail index build` writes the primary LanceDB store at `.codetrail/index.lance`.
- `codetrail index status` and `codetrail index verify` report freshness, stale files, and active snapshot state.
- Dirty worktrees can combine fresh indexed files with live overlay for changed files.
- `codetrail index pack` and `codetrail index unpack` support remote snapshot sharing under `.codetrail/remote/`.

## Reliability Levels

- `source_fact`: filesystem, text/path, Git, or `read`; usable as evidence after range verification.
- `precise_fact`: SCIP occurrence result; still verify before editing.
- `parser_fact`: tree-sitter syntax fact; useful syntax evidence, not semantic proof.
- `inferred_candidate`: heuristic or graph candidate; must verify.
- `freshness`: cache or watcher state only.
- `remote_verified`: remote snapshot matches local file proofs; still verify key edits.
- `remote_unverified`: remote snapshot does not match local files; lead only.

## MCP And JSON Contracts

When validating MCP or machine-readable behavior, compare against the command contract rather than prose summaries:

- Inspect `docs/02-command-contract.md` for command families and JSON response expectations.
- Inspect `src/cli.rs` for current CLI argument definitions.
- Inspect `src/output.rs` and the relevant command module when response fields or reliability metadata are in question.

## Project Validation

Use the repository scripts as the source of truth:

```bash
scripts/quality-gate.sh pr
scripts/quality-gate.sh main
scripts/quality-gate.sh bench
```

`quick` aliases `pr`; `cli` aliases `main`; `full` runs main then bench.
