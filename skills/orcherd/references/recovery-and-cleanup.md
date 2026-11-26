# Recovery, blocked dialogs, circuit breaker, and teardown

Act only on observed proof from `herdr-core` and the Orcherd store. Silence or a slow turn never proves a worker exited.

## Three worker endings: settled, ended, and destroyed

Keep **Task outcome**, **Dispatch state**, and **terminal process liveness** separate:

1. **Settled (Task & Dispatch outcome)**:
   - `ORCHERD done` settles the Dispatch (`settled_succeeded` or `settled_failed`) and moves the Task to `completed`, `review`, or `failed`. A worker's reported failure (`--outcome failed`) is a real conclusion—never mark it `completed`.
2. **Ended (warm idle terminal)**:
   - By default (`--keep-open`, the default for named seats), the seat's terminal stays open and idle after `done` so the next task or follow-up `send` reuses the warm signed-in agent session immediately.
3. **Destroyed (terminal process proven gone)**:
   - Confirmed by `herdr-core` reporting `TerminalState::Finished` (`stop_reason = exited`) or a confirmed `ORCHERD seat stop`.

## Handling `escalation` wakes

When `orcherd daemon` wakes the supervisor with an `escalation` item in `inbox`, inspect `stop_reason`:

### 1. `stop_reason: blocked` (Interactive Auth or Permission Dialog)
`herdr-core` reported `StatusState::Blocked` (or refused a doorbell send with `blocked`).
- Run `ORCHERD seat show <seat>` and `ORCHERD seat read <seat> --lines 30`.
- **Auth screen (`kind: auth` or sign-in prompt)**: Tell the user which terminal needs sign-in (`<seat>` at terminal `<address>`). Never spawn a second worker of the same agent CLI while auth is expired—it will block at the exact same screen. Once the user signs in, the terminal returns to `idle` and `orcherd daemon` automatically delivers the waiting doorbell.
- **Permission prompt (`kind: permission`)**: Either ask the user to approve in the terminal or relaunch the seat with its harness's non-interactive flag if authorized by the project rules.

### 2. `stop_reason: exited` (Process Exit Confirmed)
The terminal exited before the worker called `ORCHERD done`.
- Inspect the last output with `ORCHERD seat read <seat> --lines 50`.
- Retry the same Task on the seat with `--retry-of <dispatch_id>` (and `--resume` to keep its conversation history):
  ```text
  ORCHERD spawn --seat <seat> --task <task_id> --retry-of <dispatch_id> --resume --json
  ```

### 3. `stop_reason: lost` or `unverifiable` (Remote Disconnect or Core Gap)
- Inspect `ORCHERD seat show <seat>`. The remote agent process may still be running even if the SSH connection dropped.
- Do not launch a duplicate worker on the same worktree while liveness is `unverifiable`; wait for `herdr-core` to reconnect or report the uncertainty to the user.

## The three-strike circuit breaker

After **3 consecutive dispatch failures** in one `--retry-of` chain (excluding a worker's own explicit `done --outcome failed`), the Dispatch enters `circuit_broken` and the Task is marked `failed`:
- Stop automatic retries and report the failure and last terminal output to the user.
- Never route around a tripped circuit breaker by creating a duplicate Task ID.
- Only when the user explicitly asks to reopen the task, run:
  ```text
  ORCHERD task update <task_id> --status ready
  ORCHERD spawn --seat <seat> --task <task_id> --resume --json
  ```

## Explicit stop, abandon, and release

- `ORCHERD seat stop <seat|dispatch_id>`: Stops the supervised agent terminal via `herdr-core` and settles the active dispatch. If exit cannot be confirmed, it exits 1 with `stop_unknown`.
- `ORCHERD seat abandon <dispatch_id>`: Fences an uncertain dispatch so the Task can be reassigned on another worktree while retaining the possibly-live terminal for inspection.
- `ORCHERD seat release <dispatch_id>`: Closes a settled worker's terminal when `--keep-open` is no longer needed.
