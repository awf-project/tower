//! Spec 24 fault fixture: crash-on-init.
//!
//! Reads the `initialize` request from stdin, then exits with code 1 without
//! responding.  The host sees EOF on stdout and must return
//! `ExtensionFault::Crashed { code: Some(1) }` (AC2).

use std::io::{self, BufRead};

fn main() {
    // Consume the initialize line so the host's write does not get SIGPIPE.
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    // Read one line — the initialize request — then crash.
    let _ = lines.next();
    std::process::exit(1);
}
