# Окружение разработки: тулчейн Rust живёт внутри проекта, в .toolchain/
# Использование: . scripts/dev-env.sh
_za_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export RUSTUP_HOME="$_za_root/.toolchain/rustup"
export CARGO_HOME="$_za_root/.toolchain/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
export TMPDIR="$_za_root/.tmp/os-tmp"
export TMP="$TMPDIR" TEMP="$TMPDIR"
mkdir -p "$TMPDIR"
