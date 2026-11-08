# Окружение разработки: тулчейн Rust живёт внутри проекта, в .toolchain/
# Использование: . .\scripts\dev-env.ps1
$ZaRoot = Split-Path -Parent $PSScriptRoot
$env:RUSTUP_HOME = Join-Path $ZaRoot '.toolchain\rustup'
$env:CARGO_HOME = Join-Path $ZaRoot '.toolchain\cargo'
$env:PATH = (Join-Path $env:CARGO_HOME 'bin') + ';' + $env:PATH
$env:TMP = Join-Path $ZaRoot '.tmp\os-tmp'
$env:TEMP = $env:TMP
New-Item -ItemType Directory -Force $env:TMP | Out-Null
