# Orcherd (`orcherd`)

**Orch**estration + **Herd**r + an **Orchard** of git **worktrees**.

`orcherd` is a standalone multi-agent team orchestrator built directly on top of unmodified [`herdr-core` (`hc`)](https://herdr.dev/). It fuses:

1. **Orca's orchestration ergonomics and skill architecture**:
   - **Two-tier version-matched skill (`orcherd skills get`)**: A tiny static discovery stub in `~/.agents/skills/orcherd/SKILL.md` resolves the session's `orcherd` binary once and loads the version-matched base guide and progressive disclosure references (`--reference references/<file>.md`) compiled directly into the binary.
   - **Turn-ending reactive push wakes (`done`, `ask`, `reply`, `escalation`)**: Coordinators and workers never poll or run cron sweeps. Workers end their turn after reporting `done` or asking a question via `ask`; `orcherd daemon` watches `herdr-core`'s `events.subscribe` stream and rings the target seat with an idle-gated doorbell (`terminal.send { if: "idle" }`) only when it can accept input.
   - **File-first, idempotent CLI (`--spec-file`, `--body-file`, `--result-file`)**: Eliminates shell quoting and PTY paste mangling. Every mutation returns a structured JSON receipt with `request_id`, `failed_stage`, and a conditional `next` action hint.
   - **Managed worktree placement & bootstrap**: Creates child or top-level git worktrees locally or on remote `hc` machines (`--on <machine>`) and symlinks git-ignored runtime dependencies (`.env`, `.venv`, `node_modules`) from the root checkout.

2. **Herding Teams' persistent crew, task board, and bulletin model**:
   - **Persistent named seats & templates**: Keep warm, role-aware seats (`director`/`super`, `vicesuper`/`gauntlet`, `planner`, `builder-a`, `builder-b`) alive across tasks so signed-in agent sessions keep their auth and context.
   - **Task Board with DAG dependencies & review gates**: Claimable backlog with `--deps`, GitHub issue references, blind review routing (`--reviewer`), and a three-strike circuit breaker.
   - **Shared Bulletin & Durable Findings**: Team-wide threaded discussions (`@seat`, `@all`, `@idle`), explicit `AUTHORIZED: push` gates, and evidence-backed findings.
   - **Zero-GUI-Fork Maintenance**: All state lives in a local WAL SQLite database (`store.sqlite3`) opened in-process by the CLI, with live projections published to `herdr-core` documents (`doc.set`) and rendered by a lightweight declarative Herdr UI Mod (`mods/orcherd/`).

---

## Repository Layout

```text
orcherd/
├── AGENTS.md                                  # Engineering & agent conventions for this repo
├── README.md                                  # Overview & quickstart
├── SPEC.md                                    # Locked v1.0 architecture, schema, CLI, cross-machine & mod spec
├── docs/
│   └── ORIGIN-AND-LESSONS.md                  # Scar tissue from Herding v1 & Orca distilled into 6 design laws
├── skills/
│   └── orcherd/
│       ├── SKILL.md                           # Static discovery stub installed to ~/.agents/skills/
│       ├── GUIDE.md                           # Bundled base guide printed by `orcherd skills get orcherd`
│       └── references/                        # Progressive disclosure references (`--reference`)
│           ├── supervisor-loop.md             # Backlog, DAGs, tiered review, AUTHORIZED gate
│           ├── seats-and-templates.md         # Named seats, warm reuse, templates, A/B cuts
│           ├── placement-and-remote.md        # Worktrees, bootstrap symlinks, remote `hc` machines
│           ├── messaging-and-gates.md         # Bulletin, @groups, decision gates (ask/reply)
│           ├── recovery-and-cleanup.md        # Blocked dialogs, circuit breaker, stop/abandon/resume
│           └── parallel-start.md              # Starting up to 4 workers in one shell turn
└── mods/
    └── orcherd/
        ├── mod.toml                           # Declarative Herdr UI mod manifest (panel + activity app)
        └── main.luau                          # Sandboxed Luau view for Roster, Board, Gates & Bulletin
```

## Quickstart Flow

```sh
# 1. Install the static discovery stub once
orcherd skills install

# 2. Initialize a team in the current repo (optionally from a preset crew template)
orcherd init --name ksquared --template studio-crew --goal "Ship W38 Shorts"

# 3. Spawn or dispatch to a named seat with a file-backed brief
orcherd spawn --seat builder-a --role worker --agent antigravity --model gemini-4-neon-high \
  --worktree new-child --spec-file briefs/cut-a.md --json

# 4. Inside a worker seat: ask a blocking question or complete the task, then end turn
orcherd ask --question "Should Cut A keep the cold-open telemetry HUD?"
orcherd done --result-file reports/cut-a-done.md

# 5. Inside the supervisor seat: read unread wakes, answer gates, or post verdicts
orcherd inbox
orcherd reply G-01 --body-file answers/hud-yes.md
```

See [`SPEC.md`](SPEC.md) for the complete wire, schema, and runtime specification.
