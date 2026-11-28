# AGENTS.md

Engineering guidance for agents working in the `orcherd` repository. Read this alongside [`README.md`](README.md) and [`SPEC.md`](SPEC.md).

## Priorities

- **Keep `orcherd` zero-GUI and fast to build.** Never add GPUI, `gix`, or heavy UI/HTTP dependencies to workspace crates. `orcherd` talks exclusively to SQLite (`rusqlite`) and `herdr-core` (`hc`) over its framed JSON-RPC socket (`crates/orcherd-hc`).
- **Never modify Herdr or `herdr-core`.** `orcherd` is a client of `herdr-core`'s public socket and CLI contract.
- **Keep skills and CLI in lockstep.** `skills/orcherd/GUIDE.md` and `skills/orcherd/references/*.md` are compiled into `orcherd-cli` and served by `orcherd skills get`. Update the bundled skill whenever a command, flag, or receipt changes.
- **The Herdr board mod is code too.** `mods/orcherd/` is embedded in the binary by `orcherd mod install`. Run `herdr-app check mods/orcherd` after every change, use only Herdr's public mod API and commands, and never clone or edit Herdr's shipped mods or layouts.
- **File size ceiling: 1,000 lines.** Split modules by responsibility before crossing 1,000 lines.
- **Typed errors everywhere.** Each crate defines `pub use error::{Error, Result}` using `thiserror`. Never use `Result<T, String>` or `unwrap()` / `expect()` outside tests.

## Architecture Rules

- **Single transaction per mutation**: `orcherd_core::apply(&mut Store, &Actor, Op) -> Result<Receipt>` validates input, mutates records, writes any resulting `inbox` rows, logs an `audit` row, and records `mutation_receipts` in one SQLite transaction.
- **Doorbells carry no payload**: `orcherd daemon` never pastes prompts or user text into a terminal. It sends only the fixed one-line doorbell, when the seat's turn is over (`idle`, or `done` for harnesses like agy; never mid-`working`), and records only real deliveries in `doorbell`.
- **Reviewer blindness invariant**: A task's `reviewer_seat` must never equal its `assignee_seat`. `apply` rejects self-review assignments and self-verdicts.
- **File-first multiline inputs**: Every command that accepts a brief, message body, or completion report must accept `--spec-file`, `--body-file`, or `--result-file`.
