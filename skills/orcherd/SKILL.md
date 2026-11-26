---
name: orcherd
description: >-
  Orchestrate multi-agent teams, persistent seats, task boards, bulletins, and
  supervised workers on herdr-core (hc) and get their results back. Use when
  the user invokes /orcherd, asks for team orchestration or supervision
  (supervisor/worker seats, task DAGs, blind review/gauntlet, ask/reply
  decision gates, bulletin board), or asks to split, parallelize, or fan out
  work across agents, git worktrees, or remote machines; use Orcherd seats and
  workers for those. Also use when a `[Orcherd]` doorbell notification arrives,
  when you are a spawned seat or dispatched worker (`ORCHERD_SEAT` or a prompt
  with Task, Seat, and Dispatch IDs: inbox, ask, done, verdict), or when work
  needs a different agent CLI (Claude, Codex, ZCode, OpenCode, Antigravity) or
  its own child worktree. Unless the user asks for Orcherd team workers or a
  named agent seat, searches, reads, and local research in your workspace stay
  with your own subagents.
---

# Orcherd Orchestration

This discovery stub loads the version-matched guide from the `orcherd` executable used for this session.

Never substitute a built-in subagent tool when the user asked for Orcherd team orchestration or named a seat.

## Resolve the CLI for this session

Choose the executable once and reuse it for every later command:

1. If the `ORCHERD_CLI_COMMAND` environment variable is set, use its value. `orcherd spawn` exports it in every terminal it starts on `herdr-core`.
2. Otherwise, in a development checkout whose session exposes `ORCHERD_DEV_REPO_ROOT`, use `$ORCHERD_DEV_REPO_ROOT/target/release/orcherd`.
3. Otherwise, use `orcherd`.

Below, `ORCHERD` is a placeholder for the executable you resolved. Substitute it before running anything; do not create a shell variable or run `ORCHERD` literally. This works the same way in POSIX shells, PowerShell, and `cmd.exe`.

If the selected executable cannot run, report its exact error and stop. Do not fall through to another binary (`herding` or `herdr-gpui`), which targets a different store and wire contract.

## Load the version-matched guide before running Orcherd commands

```text
ORCHERD skills get orcherd
```

That prints the compact, version-matched guide compiled into the exact binary that will handle your next commands. It covers the supervisor loop, persistent seats, and dispatched-worker obligations (`inbox`, `ask`, `done`, `verdict`). For a conditional action gate such as multi-machine placement, blind review DAGs, or uncertain recovery, load only the reference that gate names with:

```text
ORCHERD skills get orcherd --reference references/<file>.md
```

Use the selected executable's `--help` for commands or flags the guide does not cover. Before retrying any command that mutates state, check whether it already took effect (`ORCHERD status --json` or `ORCHERD task list --json`), and pass `--request-id <id>` when replaying an interrupted request.
