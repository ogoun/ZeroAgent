//! Служебные команды workspace: `cargo xtask <команда>`.

mod policy;
mod prompt_budget;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("policy") => policy::run(),
        Some("prompt-budget") => prompt_budget::run(&args[1..]),
        _ => {
            eprintln!(
                "использование: cargo xtask policy | cargo xtask prompt-budget [--llama http://host:port]"
            );
            ExitCode::from(2)
        }
    }
}
