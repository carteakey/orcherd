//! `orcherd` standalone binary entry point.
// Tests start real processes and use unwrap/expect; production code uses `new_command`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod board;
pub mod cli;
pub mod daemon;
pub mod identity;
pub mod mod_install;
pub mod output;
pub mod remote;
pub mod skills;
pub mod tunnel;
pub mod worktree;

#[cfg(test)]
mod tests;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::run_from_args(args) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("orcherd error: {err}");
            std::process::exit(1);
        }
    }
}
