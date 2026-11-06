#![forbid(unsafe_code)]

fn main() {
    if agent_memory_extension::runtime::run().is_err() {
        eprintln!("agent memory extension failed to start");
        std::process::exit(1);
    }
}
