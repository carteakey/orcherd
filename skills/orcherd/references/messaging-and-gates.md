# Messaging, bulletin board, decision gates, and approvals

## Point-to-point `send` vs. shared `bulletin`

Orcherd provides two communication channels, both backed by the same atomic `inbox` and idle-gated doorbell loop:

| Channel | Command | When to Use |
| --- | --- | --- |
| **Direct Mail (`send`)** | `ORCHERD send --to <seat\|dispatch:id\|@group> --subject "..." --body-file <file>` | Steering a specific seat, sending follow-up instructions to a warm builder, or cross-pollinating notes between A/B variants without cluttering the team-wide board. |
| **Team Bulletin (`bulletin`)** | `ORCHERD bulletin post --to @<seat> [--reply-to <post_id>] [--kind post\|handoff\|authorized] --body-file <file>` | Milestones, architecture decisions, branch handoffs, review summaries, and `AUTHORIZED: push` notices that the entire team should see in history. |

Both commands write `inbox` rows in the same SQLite transaction for every recipient and return a JSON receipt.

### Group selectors

Use group selectors only for deliberate fan-out:
- `@all`: Every active, non-paused seat in the team.
- `@idle`: Every seat currently idle with no in-progress task.
- `@workers`: Every seat with `role = worker`.
- `@reviewers`: Every seat with `role = reviewer`.

## Decision gates (`ask` and `reply`)

When a worker encounters an ambiguity, architectural fork, or missing authorization that blocks its task:

1. **Worker creates a gate and stops**:
   ```text
   ORCHERD ask --question "Should store migration v2 backfill legacy host/pane columns?" --body-file context.md
   ```
   - Marks the worker's current Task `blocked`, inserts a `pending` row in `gates`, and rings the supervisor seat with a high-priority `question` inbox item.
   - The worker **ends its turn immediately**.
2. **Supervisor resolves the gate**:
   ```text
   ORCHERD reply <gate_id> --body-file answer.md
   ```
   - Marks the gate `resolved`, restores the Task to `in_progress`, writes a `gate_resolved` item to the worker's inbox, and rings the worker seat to resume.

## Human approval gates (`pending` and `approve`)

Under Orcherd's same-user trust model, any local process could set `ORCHERD_SEAT`. To prevent accidental roster damage (such as an agent removing a live seat or reassigning the `supervisor` role on its own word):

- `ORCHERD seat remove <seat>` and `ORCHERD seat handoff --to <seat>` never execute immediately when called by a team seat. Instead, they record a pending approval gate (`approval_remove` or `approval_role`) and log an `audit` row.
- The human owner lists and approves them from a non-member shell:
  ```text
  ORCHERD pending
  ORCHERD approve <gate_id>
  ```
  `approve` refuses any caller whose environment or `$HC_TERMINAL_ID` resolves to an agent seat.
