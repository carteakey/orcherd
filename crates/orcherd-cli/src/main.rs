//! `orcherd` standalone binary entry point.
// Tests start real processes and use unwrap/expect; production code uses `new_command`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// `println!` that exits quietly when stdout is closed (`orcherd log | head`) instead of
/// panicking with "failed printing to stdout: Broken pipe".
macro_rules! outln {
    ($($arg:tt)*) => { $crate::output::emit(&format!($($arg)*), true) };
}
/// `print!` with the same broken-pipe handling as `outln!`.
macro_rules! out {
    ($($arg:tt)*) => { $crate::output::emit(&format!($($arg)*), false) };
}

pub mod board;
pub mod cli;
pub mod daemon;
pub mod freeze;
pub mod herdr_space;
pub mod identity;
pub mod mod_install;
pub mod output;
pub mod remote;
pub mod self_update;
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
