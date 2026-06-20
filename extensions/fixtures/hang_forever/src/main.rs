//! Spec 24 fault fixture: hang-forever.
//!
//! Reads the `initialize` request from stdin (so the host does not get a broken
//! pipe immediately), then sleeps forever without ever sending a response.  This
//! causes the host's request-timeout to fire, which must kill this process and
//! return `ExtensionFault::Timeout` (AC1).

use std::io::{self, BufRead};

fn main() {
    // Consume the initialize line so the host's write does not get SIGPIPE.
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    // Read one line — the initialize request — then block forever.
    let _ = lines.next();
    // Hang until SIGKILL.
    std::thread::park();
}
