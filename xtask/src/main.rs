//! Служебные команды workspace: `cargo xtask <команда>`.

mod policy;

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("policy") => policy::run(),
        _ => {
            eprintln!("использование: cargo xtask policy");
            ExitCode::from(2)
        }
    }
}
