# CodeTrail Agent Templates

This directory contains agent-layer templates that use CodeTrail as a search
tool. They are intentionally separate from the CLI/MCP command surface.

CodeTrail owns:

- source, path, symbol, reference, call-candidate, status, and freshness facts;
- output budgets, pagination, caveats, and reliability labels;
- exact range reads for verification.

Subagents own:

- deciding which CodeTrail primitive to call next;
- stopping multi-step investigations;
- compressing evidence into a compact package for a primary agent;
- adapting generic evidence collection to architecture, data model, debugging,
  review, or implementation tasks.

Do not add task-specific CLI commands such as `brief`, `context`, or
`analyze-*` to CodeTrail. Add task behavior to agent templates, and keep
CodeTrail's public commands as composable search primitives.

## Codex

Install the Codex template by copying:

```text
skills/codetrail/agents/codex/codetrail-evidence.toml
```

to:

```text
~/.codex/agents/codetrail-evidence.toml
```

The template registers the `codetrail-evidence` subagent. It should be invoked
for repository investigations that would otherwise consume many turns of search
and read output in the primary session.

## OpenCode

Install the OpenCode template by copying:

```text
skills/codetrail/agents/opencode/codetrail-evidence.md
```

to:

```text
.opencode/agents/codetrail-evidence.md
```

or:

```text
~/.config/opencode/agents/codetrail-evidence.md
```

The template is a `mode: subagent` agent. It should be invoked for repository
investigations that would otherwise consume many turns of search and read
output in the primary session.
