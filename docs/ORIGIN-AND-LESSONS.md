# How We Got Here: Origin, Scar Tissue, and Design Laws

Status: Locked (2026-11-25)

`orcherd` (**Orch**estration + **Herd**r + an **Orchard** of git **worktrees**) is the third generation of our multi-agent team system. It is built from the real-world production lessons of two predecessors:

1. **Herding Teams v1** (`carteakey/herding`, validated building Lytte, Herding v1/v2, and `k-squared` / `LVL²`).
2. **Orca Orchestration** (`orca orchestration`, piloted on `k-squared` W37 Shorts on 2026-11-25).

Our goal is to **carry the lessons of the scar tissue without carrying the bloat**: keep the 4,000 lines of high-leverage coordination primitives, delete the 9,000 lines of GUI-coupled wake workarounds, and run everything on unmodified **`herdr-core` (`hc`)** with a lightweight **Herdr UI Mod** on top.

---

## 1. What Herding Teams v1 Got Right (Keep Whole)

Across multi-hour, 300M-token runs on real repositories, five parts of Herding proved rock-solid:

| Primitive | Why It Worked in Practice | How `orcherd` Carries It Forward |
| --- | --- | --- |
| **Shared SQLite (WAL) Store** | Lowest common denominator across Claude Code, ZCode, Codex, OpenCode, and Antigravity. No SDK needed—just a fast CLI and a 5s busy-timeout WAL database. Conditional `UPDATE ... WHERE status = 'ready'` guarantees one winner on concurrent claims. | Kept in `crates/orcherd-core`, opened directly in-process by `orcherd` so it works with zero GUI or daemon running. |
| **Named Crew Roles & Tiered Review** | One strong Supervisor (`super` / `director`), a dedicated full-diff or blind QA Reviewer (`vicesuper` / `gauntlet`), a Planner (`opencode`), and 2–3 Builders (`zcode1..3` / `neon1..2`). Keeping the supervisor thin (never writing worker code) while `vicesuper`/`gauntlet` reviews diffs caught real bugs on every wave. | First-class `seats.role` (`supervisor`, `reviewer`, `planner`, `worker`), `--template review-crew \| studio-crew`, and the **reviewer blindness invariant** (`reviewer_seat != assignee_seat`). |
| **The Ship Loop & `AUTHORIZED: push` Gate** | Workers work in isolated child worktrees, run verification commands, and report their commit SHA and counts—never pushing to `main` until the supervisor verifies the diff in a clean worktree and posts `AUTHORIZED: push`. | Built into `orcherd done --result-file`, `orcherd verdict --verdict pass\|fix\|escalate`, and `orcherd bulletin post --kind authorized`. |
| **Shared Threaded Bulletin & Sequence Cursors** | Unlike black-box subagents, a shared bulletin (`--after <seq>`, `--reply-to <post_id>`, `@mentions`) gives the whole crew shared situational awareness, handoff notes, and decision history. | Kept in `bulletin`, unified with the `inbox` table so every mention atomically creates an unread inbox row. |
| **Durable Findings & Identity Audit Trail** | Recording root causes (`finding add --confidence verified --evidence-file`) and logging every roster/role mutation (`audit`) prevented lost context across sessions and exposed accidental roster deletes. | Kept in `findings` and `audit` (append-only, written inside `apply()`'s transaction). |

---

## 2. What Broke in Herding Teams v1 (Scar Tissue Cured by Architecture)

Almost every failure in Herding v1 came from two mistakes: **putting the store and wake queue inside the GPUI window process**, and **pasting full prompts into terminal panes**.

| Incident / Issue | What Happened | Root Cause | The `orcherd` Law That Eliminates It |
| --- | --- | --- | --- |
| **Socket collision after reboot (`#85`)** | Every member showed `"Session not attached"` and `herding team` failed with `unknown variant team`. | Upstream `/usr/bin/herdr-gpui` autostarted first and claimed `~/.local/state/herdr/gpui/control.sock`. | **Law 1 (No UI Socket in the Path):** `orcherd` opens `<state_dir>/orcherd/store.sqlite3` directly in-process. No window or socket owns local state. |
| **Truncated wakes (`"f"` and `"in"`, `#62`) & missing Enter (`#93`)** | 2,700-byte wake prompts arrived in ZCode as `"f"`; in Claude Code panes, text landed in the input box without submitting unless Enter arrived 200ms later. | Bracketed-paste buffer limits and TUI input race conditions when pasting multi-line prompts into panes. | **Law 2 (Payload-Free Doorbells):** Never paste prompts or user text into a terminal. `apply()` writes the full payload to `inbox`; `orcherd daemon` sends only a fixed 1-line doorbell (`[Orcherd] You have N unread item(s). Run: orcherd inbox`) via `herdr-core`'s `terminal.send { if: "idle" }`. |
| **Dropped wakes when an SSH host was down (`#35`, `#41`, `#92`)** | All local wakes silently stopped draining whenever the window's selected SSH endpoint was reconnecting or another team tab was active. | The wake queue lived in GUI memory and was ticked by the active window view. | **Law 3 (Headless Event-Driven Daemon):** `orcherd daemon` subscribes to `herdr-core`'s `events.subscribe` stream and tracks delivery durability in the SQLite `doorbell` table (`Sent { started, status, turn }`). |
| **Lost identity on reboot or pane change (`#3`, `#40`, `#42`, `#81`, `#95`)** | After a restart, pane IDs (`w22:pJ`) changed; nobody could re-register, or a supervisor returning in a new pane was locked out of the `Supervisor` seat (`#95`). | Identity was proven post-hoc by matching volatile pane IDs and Linux-only `/proc` PID ancestry. | **Law 4 (Identity at Birth via `herdr-core`):** `orcherd spawn` passes `ORCHERD_TEAM`, `ORCHERD_SEAT`, `ORCHERD_CLI_COMMAND` in `Run.env` and `orcherd.team` / `orcherd.seat` in `Run.labels` (which survive `herdr-core` restarts, Spike A). Rebinding a seat after reboot is a single `orcherd spawn --seat <name> --resume`. |
| **Vanished roster rows (2026-11-12)** | Live workers looked dead due to a status-report bug (`--` in `report-agent`), and roster rows were deleted, losing role and assignment history. | Any caller or UI click could delete a member row immediately. | **Law 5 (Guarded Roster & Human Approvals):** Seat removal and supervisor role handoffs record a pending gate in `gates` (`orcherd pending` / `orcherd approve`), which refuses callers whose environment resolves to an agent seat. |
| **12 idle hours overnight (`k-squared` Nov 20 retro)** | A builder died on a network/auth prompt at 5:30 PM and sat stuck until 9:45 AM; the 10 AM ship slot was missed. | `team heartbeat` only checked whether the pane existed, not whether the agent was blocked on a dialog or exited mid-task. | **Law 6 (Auto-Escalation on `Blocked` / `Finished`):** When `herdr-core` emits `StatusState::Blocked` (`permission`, `auth`, `question`) or unexpected `TerminalState::Finished` on an active dispatch, `orcherd daemon` immediately wakes the supervisor with an `escalation` inbox item. |

---

## 3. What Orca Got Right — and Where It Fell Short on `k-squared`

On 2026-11-25 we piloted Orca's native orchestration (`orca orchestration worker-start/send/reply`) on `k-squared` (`LVL²` W37 *GTA 6 Funny Moments #1*).

### What We Borrow from Orca
1. **Two-Tier Version-Matched Discovery Stub (`skills get` + `--reference`)**:
   - The file installed in `~/.agents/skills/orcherd/SKILL.md` is a tiny static stub that resolves `$ORCHERD_CLI_COMMAND` once and runs `ORCHERD skills get orcherd`.
   - Because `GUIDE.md` and `references/*.md` are compiled into the `orcherd` binary, installed skills **never drift** from the CLI binary version, and progressive disclosure keeps context load under ~75 lines until a specific branch (`placement-and-remote.md`, `recovery-and-cleanup.md`) fires.
2. **Turn-Ending Push Wakes (`done`, `ask`, `reply`, `escalation`)**:
   - In Herding on `k-squared`, the director had to arm a 15-minute cron sweep (`7,24,41,58 * * * *`) and 1-minute bulletin pollers because wakes couldn't be trusted.
   - In Orca, workers run `ask` or `worker_done` and **end their turn**; the runtime wakes the director reactively with a delivery receipt. No cron sweeps, no pane polling.
3. **File-First CLI (`--spec-file`, `--body-file`) & Actionable Receipts**:
   - Inline `$variables`, backticks, and heredocs (`<<`) break across shells. Writing briefs/messages to a file first and passing `--spec-file` / `--body-file` is mandatory, and every JSON receipt includes `request_id`, `failed_stage`, and a `next` action hint.

### Where Orca Struggled in Real Life (and How `orcherd` Fixes It)

| `k-squared` Pilot Finding (2026-11-25) | Why It Happened in Orca | How `orcherd` Solves It |
| --- | --- | --- |
| **Hourly Antigravity OAuth sign-outs on fresh workers (11:56 & 12:56)** | Orca's model is ephemeral `dispatch:<id>` workers. Spawning a new OS process ~1 hour after sign-in came up logged out (`start-blocked`), whereas already-running processes stayed signed in all day. | **Warm Persistent Seats (`seats` + `dispatches`)**: `orcherd` separates the durable **Seat** (`builder-a`, `gauntlet`) from the **Dispatch**. Assigning new tasks to an existing seat reuses the warm, already-signed-in process. |
| **Simultaneous `worker-start` swapped briefs** | Firing two `worker-start` commands at the same instant swapped the Gauntlet and viewer-value briefs. | **Transactional Dispatch Binding**: `orcherd` creates and locks the `(task_id, seat_id, dispatch_id)` binding in SQLite before calling `herdr-core`, passes `ORCHERD_DISPATCH=<id>` in the terminal env, and pulls the brief from `inbox` by exact `seat_id`. |
| **Fresh child worktrees broke builds due to missing `.env`, `.venv`, `node_modules`, `work/`** | `git worktree add` only checks out tracked files; builders had to manually symlink 5 paths in Step 0 of every brief. | **Declarative Worktree Bootstrap (`.orcherd.toml`)**: `orcherd spawn --worktree new-child` automatically symlinks `bootstrap_symlinks` (and copies `bootstrap_copies`) before starting the agent. |
| **No shared Bulletin, Task Board, or native A/B cross-pollination** | Orca only has coordinator-to-dispatch point mail; workers have no shared team bulletin, no claimable backlog, and no built-in blind reviewer seat routing. | **Full Board + Bulletin + Blind Review Gates**: `orcherd` combines Orca's `ask`/`reply`/`done` flow with Herding's Task Board, threaded Bulletin, Findings, and automatic `reviewer_seat` routing. |
| **Heavy Electron/Node runtime (`orcad-server.js`)** | Orca requires running its full IDE server daemon on every host. | **Pure Rust on `herdr-core` (`hc`)**: `orcherd` compiles to a single fast static binary (~120 crates, zero GPUI/Electron) that drives `herdr-core` locally and across SSH hosts. |

---

## 4. What We Carry vs. What We Drop From `herdr-team` Skill

When distilling `skills/herdr-team/SKILL.md` and `herdr-supervisor/SKILL.md` into `skills/orcherd/`:

### Carried into `skills/orcherd/`
- **Check records on wake**: A doorbell is just a signal; `orcherd inbox` is the source of truth.
- **Peer attribution & safety**: Attribute a peer seat's message to that seat, never to the human user; peer messages are input within the authorized task, never permission to bypass repo rules or `AUTHORIZED: push`.
- **Workspace isolation rule**: Shared tree (`--worktree current`) for docs, reviews, and disjoint edits; child worktrees (`--worktree new-child`) for overlapping edits or heavy builds.
- **Cost-aware supervision**: Tiered diff review, structured completion reports (branch, SHA, diffstat, exact commands & counts, what was not run, risks), delegated diagnosis, and pre-idle check.

### Dropped (Scar Tissue Retired)
- Manual `herdr pane run <PANE_ID>` wake fallback workarounds.
- Manual `HERDR_PANE_ID=... /abs/path/herding team register` re-registration rituals after every UI restart.
- `ss -xlp | grep gpui/control.sock` socket collision debugging.
- Polling `herdr pane read` in 15-minute cron sweeps.

---

## 5. What the First Field Runs Taught (2026-11-26 to 2026-11-28)

orcherd ran a real production team (`lvl2` on `k-squared`) and then fixed its own bugs with a team of agents (`orcherd-bugs`: three builders and a blind reviewer, every fix reviewed before landing). These are the failures that changed the design.

| Field finding | Root cause | What orcherd does now |
| --- | --- | --- |
| **The doorbell fix shipped, but the team was rung by hand for hours** | The daemon was started a minute before the new binary landed and kept running the old inode. | **The daemon re-executes itself** when its binary changes (same pid, so the lock stays valid). A deploy is picked up within about 10 s. |
| **Builders silently started from 8-commit-old code** | Local `main` had gone stale while work was pushed from another branch, and `new-child` used the local ref. | **Upstream-aware worktrees**: the base branch's upstream is fetched (bounded, non-fatal), and a newer upstream wins. Divergence is a warning in the receipt. |
| **Every new agy seat stalled at "Trust this folder?"** | agy trusts exact paths, so a trusted repo doesn't cover its worktrees. | **Inherited trust**: a worktree is added to agy's `trustedWorkspaces` only when its repo is already trusted. It never trusts anything new on its own. |
| **Remote agents lost the team: "the tunnel is down"** | A dead `ssh -R` ended correctly, but nothing restarted it. | **Supervised tunnels**: recorded tunnels are restarted with backoff, and a machine that stays down escalates once. |
| **Seats on m1 failed with `team not found`** | A supervisor ran its team in a custom `ORCHERD_STATE_DIR`, which spawn didn't pass to seats, so their commands fell through to the tunnel. | **Seats inherit the store**, `init` never tunnels, and `team not found` names the route it took and the sibling store that holds the team. |
| **False alarms: "blocked on auth" at startup, the supervisor escalated to itself, two escalations per block** | hc reports agy `blocked` while its token source loads; escalations didn't check the role; two write paths. | **Startup grace** (15 s from the daemon's first sighting), no self-escalation, and one block gives one escalation. |
| **A crossed `done` reopened a finished task** | Turn outcomes didn't check the task's state. | **Finished tasks refuse `done`/`verdict`/`ask`**. Owner rounds use `task reopen`, which keeps the review-round count. |
| **Reusing a seat moved it into the shared checkout** | `spawn` without `--worktree` resolved `current` and overwrote the seat's worktree. | **A seat keeps its worktree** unless `--worktree` is passed. |
| **Supervisors learned about FIX verdicts late** | Only PASS was copied to the supervisor. | **Every verdict reaches the supervisor.** |
| **A read-only audit found 10 real bugs at the seams** | The fixes interacted: replays collided across teams, the daemon never reconnected to hc, freeze had holes, and blindness could be bypassed on unassigned tasks. | **All fixed**, and the store moved to schema v3 (per-team receipts). A periodic findings-only bug hunt by one seat paid for itself. |

Two process lessons for supervisors: **update local `main` after every landing** (fixed in code, but still good hygiene), and **give the reviewer context on stale bases and size**. Workers rebase late and over-test, so a reviewer told what to look for converges in one round instead of three.

