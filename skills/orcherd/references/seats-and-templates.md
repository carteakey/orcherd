# Persistent seats, warm reuse, templates, and A/B workflows

## Why persistent seats exist

Unlike ephemeral subagents that vanish after one task, an Orcherd **Seat** (`--seat <name>`) is a persistent identity and warm terminal on `herdr-core`:

1. **Keeps OAuth and CLI sessions warm**: Harnesses like Antigravity (`agy`), Claude Code, and ZCode hold active credentials and repo caches in memory. Spawning a fresh OS process an hour later can hit expired OAuth refresh tokens (`start_blocked`), whereas sending a new task or follow-up to an already-running warm seat continues smoothly in the same signed-in session.
2. **Preserves role and review memory**: A seat like `vicesuper` or `gauntlet` retains its standing brief, role constraints, and audit trail across the entire run.
3. **Survives reboots via `--resume`**: Each seat stores its `resume_session` and `worktree_path`. After a machine reboot, `ORCHERD spawn --seat <name> --resume` restarts the harness with `--resume <id>` in its existing worktree and rebinds its `herdr-core` terminal address without losing task assignments or inbox items.

## Built-in team templates

Initialize a roster in one command with `ORCHERD init --name <team> --template <template>`:

| Template | Seats Created | Best For |
| --- | --- | --- |
| `review-crew` | `super` (supervisor), `vicesuper` (reviewer), `planner` (planner), `zcode1..3` (workers) | Software engineering repositories with full-diff review gates and parallel worktree feature branches. |
| `studio-crew` | `director` (supervisor), `builder-a` & `builder-b` (workers), `gauntlet` (blind reviewer), `viewer-value` (reviewer) | Creative, media, or A/B implementation workflows where two builders produce intentional variants and blind reviewers grade them. |
| `small` | `super` (supervisor), `w1..w3` (workers) | General parallel task execution without a separate reviewer seat. |

You can also define custom templates and `bootstrap_symlinks` in `.orcherd.toml` at the repository root:

```toml
[team]
name = "ksquared"
template = "studio-crew"
bootstrap_symlinks = [
  ".env",
  ".venv",
  "node_modules",
  "formats/gta6/.venv",
  "formats/gta6/work",
]
```

## Intentional A/B variants and cross-pollination (`studio-crew`)

When running two builders in parallel on two intentional variants of the same deliverable (`builder-a` and `builder-b`, each in its own `--worktree new-child`):

1. **Distinct briefs**: Give `builder-a` and `builder-b` different `--spec-file` briefs so the variants differ by design intent rather than chance.
2. **Cross-pollinate between rounds**: After Round 1 reviews, the supervisor (`director`) sends each builder a concise `"From the other variant"` note via `ORCHERD send --to builder-a --subject "From Cut B" --body-file notes/from-b.md` naming specific fixes or techniques to borrow while keeping its own brief's intent.
3. **Keep reviewers strictly blind**: `gauntlet` and `viewer-value` run read-only in the target variant's worktree (`--worktree path:<variant_worktree>`) and never receive the other variant's notes.
