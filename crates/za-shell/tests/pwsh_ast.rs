//! Дифференциальная проверка лексера PowerShell (D81, раунд 2 ревью задачи 8): разбиение
//! на команды сверяется с AST-парсером самого PowerShell
//! (`System.Management.Automation.Language.Parser::ParseInput`) на сгенерированном
//! корпусе. Команды только разбираются, не исполняются; имена в корпусе безобидные
//! (`Write-Output`, `Get-Date`).
//!
//! Правило сверки: если PowerShell разбирает команду без ошибок, а лексер не пометил ни
//! одного сегмента неразобранным, то мультимножества имён команд (простые имена вида
//! `get-date`) должны совпасть. Неразобранная команда идёт на подтверждение — это
//! безопасный ответ, она не сверяется. Ошибка разбора PowerShell — скрипт не исполнится
//! вовсе. Только Windows (станция и CI).
#![cfg(windows)]

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use za_shell::{Dialect, argv0, segments};

#[path = "../../../test-support/unique_dir.rs"]
mod shared_dir;

/// Скрипт: строка входа — команда в base64 от UTF-16LE; строка выхода — число ошибок
/// разбора, табуляция, имена команд через запятую.
const SCRIPT: &str = r#"param([string]$In, [string]$Out)
$res = New-Object System.Collections.Generic.List[string]
foreach ($b in [IO.File]::ReadAllLines($In)) {
    $c = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($b))
    $t = $null; $e = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseInput($c, [ref]$t, [ref]$e)
    $names = @($ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.CommandAst] }, $true) |
        ForEach-Object { $n = $_.GetCommandName(); if ($n) { $n.ToLowerInvariant() } })
    $res.Add(('{0}' -f $e.Count) + "`t" + ($names -join ','))
}
[IO.File]::WriteAllLines($Out, $res)
"#;

fn base64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(c.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(c.get(2).copied().unwrap_or(0));
        for (i, sh) in [18u32, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= c.len() {
                char::from(A[((n >> sh) & 63) as usize])
            } else {
                '='
            });
        }
    }
    out
}

/// Простое имя команды: буквы, цифры и `-`, с буквы.
fn plain(n: &str) -> bool {
    n.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Корпус: шаблоны с местом `{X}` и символы/последовательности для него.
fn corpus() -> Vec<String> {
    let mut xs: Vec<String> = (0u32..=0xFF)
        .filter_map(char::from_u32)
        .map(String::from)
        .collect();
    for r in [
        0x1680..=0x1680u32,
        0x180E..=0x180E,
        0x2000..=0x206F,
        0x3000..=0x3000,
        0xFEFF..=0xFEFF,
        0xFF02..=0xFF02,
        0xFF07..=0xFF07,
        0xFF1B..=0xFF1B,
        0x037E..=0x037E,
    ] {
        xs.extend(r.filter_map(char::from_u32).map(String::from));
    }
    for s in [
        "\r\n",
        "\r\r",
        "\n\r",
        "\r\n\r\n",
        " \r\n",
        "\r \n",
        "`\r\n",
        "` ",
        "`\t",
        "''",
        "\"\"",
        "\u{2019}\u{2019}",
        "\u{201D}\u{201D}",
    ] {
        xs.push(s.to_owned());
    }
    let templates = [
        "Write-Output a{X}Get-Date b",
        "Write-Output '{X}; Get-Date b; Write-Output {X}'",
        "Write-Output \"{X}; Get-Date b; Write-Output {X}\"",
        "Write-Output {X}a; Get-Date b; Write-Output {X}",
        "# c{X}Get-Date b",
        "Write-Output a `{X}Get-Date b",
        "Write-Output a`{X}Get-Date b",
        "Write-Output $(Write-Output a{X}Get-Date b)",
        "Write-Output $(Write-Output a`{X}Get-Date b)",
        "Write-Output \"$(Write-Output '{X}; Get-Date b; Write-Output {X}')\"",
        "<# c{X}#> Get-Date b",
        "Write-Output a{X}{X}Get-Date b",
        "& { Write-Output a{X}Get-Date b }",
        "$x = 1{X}Get-Date b",
        "Write-Output a {X}; Get-Date b",
        "Write-Output a{X}| Get-Date",
        "Write-Output a;{X}Get-Date b",
        "{X}Get-Date b",
        "Write-Output 'a'{X}Get-Date b",
        "Write-Output @(Write-Output '{X}'); Get-Date",
    ];
    let mut out = Vec::new();
    for t in templates {
        for x in &xs {
            out.push(t.replace("{X}", x));
        }
    }
    out
}

/// PowerShell станции: `pwsh`, иначе `powershell`.
fn powershell(script: &Path, input: &Path, output: &Path) -> std::process::Output {
    let run = |exe: &str| {
        Command::new(exe)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive"])
            .args(["-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .arg(input)
            .arg(output)
            .output()
    };
    run("pwsh")
        .or_else(|_| run("powershell"))
        .expect("PowerShell не запустился")
}

#[test]
fn lexer_segments_match_powershell_ast() {
    let started = Instant::now();
    let dir = shared_dir::unique_dir("za-shell-pwsh", "ast");
    let cases = corpus();
    let script = dir.join("parse.ps1");
    let input = dir.join("cases.txt");
    let output = dir.join("ast.txt");
    std::fs::write(&script, SCRIPT).unwrap();
    let lines: Vec<String> = cases
        .iter()
        .map(|c| {
            let b: Vec<u8> = c.encode_utf16().flat_map(u16::to_le_bytes).collect();
            base64(&b)
        })
        .collect();
    std::fs::write(&input, lines.join("\n") + "\n").unwrap();
    let o = powershell(&script, &input, &output);
    assert!(
        o.status.success(),
        "парсер PowerShell: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let ast = std::fs::read_to_string(&output).unwrap();
    let ast: Vec<&str> = ast.lines().collect();
    assert_eq!(ast.len(), cases.len());

    let (mut compared, mut unparsed, mut ps_errors) = (0, 0, 0);
    let mut diff = Vec::new();
    for (c, line) in cases.iter().zip(&ast) {
        let segs = segments(c, Dialect::PowerShell);
        let (errs, names) = line.split_once('\t').unwrap_or((line, ""));
        if errs != "0" {
            ps_errors += 1;
            continue;
        }
        if segs.iter().any(|s| s.unparsed) {
            unparsed += 1;
            continue;
        }
        compared += 1;
        // Имя PowerShell — к виду `argv0`: без каталога и расширения программы.
        let mut ps: Vec<String> = names
            .split(',')
            .map(|n| {
                let base = n.rsplit(['/', '\\']).next().unwrap_or(n);
                [".exe", ".cmd", ".bat", ".com"]
                    .iter()
                    .find_map(|e| base.strip_suffix(e).filter(|b| !b.is_empty()))
                    .unwrap_or(base)
                    .to_owned()
            })
            .filter(|n| plain(n))
            .collect();
        let mut lex: Vec<String> = segs.iter().filter_map(argv0).filter(|n| plain(n)).collect();
        ps.sort();
        lex.sort();
        if ps != lex {
            diff.push(format!("{c:?}: PowerShell {ps:?}, лексер {lex:?}"));
        }
    }
    eprintln!(
        "корпус {}: сверено {compared}, неразобрано {unparsed}, ошибки разбора PowerShell {ps_errors}; {:.1} с",
        cases.len(),
        started.elapsed().as_secs_f64()
    );
    assert!(compared > 1000, "сверено слишком мало: {compared}");
    assert!(
        diff.is_empty(),
        "{} расхождений, первые:\n{}",
        diff.len(),
        diff.iter().take(30).cloned().collect::<Vec<_>>().join("\n")
    );
}
