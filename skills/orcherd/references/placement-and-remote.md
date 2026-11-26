# Worktree placement, bootstrap symlinks, and remote machines

## Local worktree placement (`--worktree`)

Choose `--worktree` based on file ownership and isolation needs:

- **`--worktree current`**: Runs in the current checkout. Use for the supervisor, planner, read-only reviewers (`vicesuper`), documentation tasks, or non-overlapping edits that benefit from a shared build cache.
- **`--worktree new-child`**: Creates a new git worktree branched from `HEAD` (or `--base-branch <ref>`) under `<workspace_root>-wt/<seat_or_task>` and binds it to the seat. Use whenever parallel workers edit overlapping crates/files or run concurrent builds.
- **`--worktree new-top-level`**: Creates a sibling worktree outside the parent workspace hierarchy.
- **`--worktree path:<dir>`**: Runs inside an existing worktree path (for example, pointing a blind `gauntlet` reviewer at `builder-a`'s child worktree so it can inspect built artifacts read-only).

### Bootstrap symlinks for fresh worktrees

Git worktrees do not carry git-ignored files (`.env`, `.venv`, `node_modules`, local media caches). When `orcherd spawn` creates a `new-child` or `new-top-level` worktree, it reads `bootstrap_symlinks` from `.orcherd.toml` (or the team record) and symlinks each listed relative path from the root checkout into the new worktree before starting the agent terminal.

Never run `git init` in an existing user folder, and let `orcherd spawn` create and bootstrap worktrees rather than running raw `git worktree add` by hand.

## Remote execution across machines (`--on <machine>`)

`herdr-core` (`hc`) natively spans machines over SSH (`hc machine add <name> <host>`), addressing remote terminals as `<machine>/t<N>` (e.g., `m1/t7`).

1. **Discover available machines**:
   ```text
   ORCHERD seat list --include-remote --json
   ```
   Use LAN hostnames/IPs configured in `hc` when Tailscale MagicDNS is unavailable on a target host.
2. **Spawn a seat on a remote machine**:
   ```text
   ORCHERD spawn --seat m1-worker --on m1 --agent zcode --worktree current --spec-file briefs/m1-task.md --json
   ```
   - `orcherd` calls `herdr-core`'s `terminal.run { on: Some("m1"), env, labels, cwd }` and ensures the per-machine reverse SSH socket tunnel (`ORCHERD tunnel m1`, forwarding remote `control.sock` to local `remote/m1.sock`) is active.
   - Because `herdr-core` sets bare `HC_TERMINAL_ID=t7` inside remote terminals, `orcherd spawn --on m1` exports `ORCHERD_MACHINE=m1` and `ORCHERD_REMOTE_SOCKET=<forwarded_sock>` in the worker's environment, and the local daemon qualifies requests arriving on `remote/m1.sock` as `m1/t7`.
   - Outbound doorbells go directly from the supervisor host's `orcherd daemon` through local `herdr-core` to `m1/t7`.
   - Inbound CLI calls (`orcherd inbox`, `orcherd ask`, `orcherd done`) from `m1-worker` travel over the forwarded unix socket back to the supervisor host's SQLite store.
3. **Record placement in the spec**:
   When a task runs on a specific machine, end `--spec-file` with:
   ```text
   Placement: <machine> (<reason>).
   ```
   Remember that a remote machine has its own git checkout and credentials: push the branch or commit SHA it needs before dispatching, and never pass local-only `/tmp` paths in `--spec-file`.
