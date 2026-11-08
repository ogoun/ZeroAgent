//! Версия компилятора на момент сборки для `zero-agent doctor`.
//!
//! `CARGO_PKG_RUST_VERSION` — лишь минимальная поддерживаемая версия (MSRV),
//! поэтому фактическая берётся у компилятора, которым собирается крейт
//! (`$RUSTC --version`), и передаётся как `ZA_RUSTC_VERSION`.

use std::process::Command;

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rustc (версия неизвестна)".to_owned());
    println!("cargo:rustc-env=ZA_RUSTC_VERSION={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");
}
