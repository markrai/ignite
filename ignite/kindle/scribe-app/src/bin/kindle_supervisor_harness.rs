//! Isolated entry point for the Review 122 Kindle supervisor harness.
//!
//! Requires `--features kindle_supervisor_harness`. Not an Ignite startup path.

fn main() -> std::process::ExitCode {
    scribe_app::run_kindle_supervisor_harness()
}
