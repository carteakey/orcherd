# Supervisor loop, DAGs, and tiered review

## Task DAGs

Break multi-step work into a DAG in the task board so dependent steps unblock automatically as upstream tasks complete:

```text
ORCHERD task create --title "1.1 Scaffold types" --spec-file specs/1.1.md --json
ORCHERD task create --title "1.2 Move wire Op/Reply" --spec-file specs/1.2.md --deps T-01 --reviewer vicesuper --json
ORCHERD task create --title "1.3 Move SQLite store" --spec-file specs/1.3.md --deps T-01 --reviewer vicesuper --json
ORCHERD task list --ready --json
```

On every supervisor wake:
1. Run `ORCHERD inbox` to process completions, verdicts, questions, and escalations.
2. Run `ORCHERD task list --ready` and assign or `spawn` every newly unblocked task onto an idle worker seat.
3. Run the **pre-idle check**: before ending your turn, confirm every idle worker seat either holds an active dispatch or there are no `ready` tasks remaining. Then end your turn.

## Tiered review (`super` + `vicesuper` / `gauntlet`)

Pairing one supervisor with cheaper or specialized builders saves tokens only when the supervisor stays thin:

1. **Assign a dedicated `reviewer_seat` (`--reviewer vicesuper` or `--reviewer gauntlet`)** on tasks that touch high-risk surfaces (storage schemas, wire protocols, delivery loops, security, or media cuts requiring blind QA).
   - Orcherd enforces **reviewer blindness**: `reviewer_seat` cannot equal `assignee_seat`.
   - When the worker runs `ORCHERD done --result-file ...`, the task moves to `review` and Orcherd wakes the `reviewer_seat` automatically.
2. **Reviewer action**:
   - **Gate the exact commit hash, not a dirty working tree**: Inspect `git show <sha>` (or check out `<sha>` in a clean worktree) and reproduce the claimed fix rather than trusting report prose.
   - The reviewer posts:
     ```text
     ORCHERD verdict <task_id> --verdict pass|fix|escalate --body-file review.md
     ```
   - `fix` sends numbered fix notes directly back to the worker seat, copies you, and increments `review_round`. Owner-directed rounds after a PASS go through `ORCHERD task reopen <task_id> --spec-file <path>`, which keeps the count.
   - `pass` wakes the supervisor to land or ship.
   - **Negative results count**: When an experiment proves an approach is slower or worse, record the measured numbers in a finding and close the task cleanly.
3. **Low-risk surfaces** (docs, isolated tests, help text): omit `--reviewer` and verify directly from the worker's structured completion report plus green CI checks.

## Commit, push, and landing authority

| Role | Authority |
| --- | --- |
| **Worker** | Works on a task branch or child worktree. Never commits to `main`, pushes to remotes, or opens PRs unless the supervisor posts `--kind authorized` (`AUTHORIZED: push <branch>`). |
| **Reviewer** | Read-only verification in the worker's worktree (`--worktree path:<dir>`) or git ref. Posts `verdict --verdict pass|fix|escalate`; never merges. |
| **Supervisor** | Authors briefs, resolves `ask` gates, verifies git ancestry and CI, posts `AUTHORIZED: push`, and fast-forwards reviewed branches onto `main`. |
| **Human Owner** | Owns release tags, production deploys, external publishing, and `ORCHERD approve` for roster removals or role handoffs. |

## Durable findings

Whenever a task uncovers an architectural invariant, benchmark result, or root-cause proof that outlives the task, record it as a finding:

```text
ORCHERD finding add --title "hc 7501 probe response path" --task T-04 --confidence verified --evidence-file findings/hc-7501.md
```
