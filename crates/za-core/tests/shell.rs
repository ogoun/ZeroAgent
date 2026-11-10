//! Лексер команд оболочки и классификаторы (R1-C1 D77, D80, D81, D99; Ruling 7.16, 7.17).
//!
//! Таблицы брифа задачи 3 и два обязательных корпуса: обходов (команда выглядит
//! безобидно, но должна дать `Ask` — метку необратимости, подстановку или неразобранный
//! сегмент) и ложных срабатываний (обычные команды разработки, на которых детекторы
//! молчат). Каждый случай — строка таблицы с текстом команды в сообщении о падении.

use za_core::shell::{
    Dialect, Segment, argv0, irreversible, is_network, is_simple, path_words, segments,
};

const B: Dialect = Dialect::Bash;
const P: Dialect = Dialect::PowerShell;

/// Лексическая граница проекта для тестов: абсолютный путь, `~` и `..` — снаружи.
fn outside(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with('\\')
        || p.starts_with('~')
        || p.split(['/', '\\']).any(|c| c == "..")
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Метки необратимости всех сегментов команды (как их собирает политика D80).
fn labels(c: &str, d: Dialect) -> Vec<&'static str> {
    segments(c, d)
        .iter()
        .filter_map(|s| irreversible(s, d, &outside))
        .collect()
}

/// Команда даёт `Ask`: хотя бы один сегмент необратим (D81) либо `allow` к ней
/// неприменимо из-за подстановки или неразобранной части (D80, Ruling 7.16).
fn asks(c: &str, d: Dialect) -> bool {
    let segs = segments(c, d);
    !labels(c, d).is_empty() || segs.iter().any(|s| s.has_subst || s.unparsed)
}

fn seg0(c: &str, d: Dialect) -> Segment {
    segments(c, d)
        .into_iter()
        .next()
        .expect("хотя бы один сегмент")
}

// --- Таблицы брифа -------------------------------------------------------------------

#[test]
fn segments_split_and_mark() {
    let s = segments("cargo test & curl x", Dialect::Bash);
    assert_eq!(s.len(), 2);
    assert!(segments("cargo test $(curl x)", Dialect::Bash)[0].has_subst);
    assert!(segments("echo `id`", Dialect::Bash)[0].has_subst);
    assert!(segments("cat <(curl x)", Dialect::Bash)[0].has_subst);
    assert!(segments("echo 'a", Dialect::Bash)[0].unparsed);
    assert_eq!(
        segments(r"Write-Host C:\a\b", Dialect::PowerShell)[0].argv[1],
        r"C:\a\b"
    ); // \ не экранирует
}

#[test]
fn irreversible_hits() {
    for (c, d, k) in [
        ("rm -rf build", B, "delete"),
        ("sudo rm -r x", B, "delete"),
        ("xargs rm -rf", B, "delete"),
        ("find . -delete", B, "delete"),
        ("/bin/rm -f a", B, "delete"),
        (r"\rm -rf a", B, "delete"),
        ("bash -c 'rm -rf x'", B, "delete"),
        ("ri -r x", P, "delete"),
        ("Remove-Item -Recurse:$true x", P, "delete"),
        ("git -C x reset --hard", B, "git_reset"),
        ("git checkout .", B, "git_reset"),
        ("git clean -xdf", B, "git_reset"),
        ("git stash drop", B, "git_reset"),
        ("git branch -D f", B, "git_reset"),
        ("git -c a=b push -f", B, "git_force_push"),
        ("git push origin +main", B, "git_force_push"),
        ("cargo publish", B, "publish"),
        ("terraform destroy", B, "infra"),
        ("psql -c 'DROP TABLE t'", B, "sql_drop"),
        ("echo x > /etc/hosts", B, "overwrite"),
        ("cat <<EOF", B, "unparsed"),
    ] {
        assert_eq!(
            irreversible(&segments(c, d)[0], d, &outside),
            Some(k),
            "{c}"
        );
    }
}

#[test]
fn irreversible_false_positive_corpus() {
    for (c, d) in [
        ("echo rm -rf", B),
        ("grep \"git push --force\" notes.md", B),
        ("rm file.txt", B),
        ("git push", B),
        ("git checkout -b x", B),
        ("git clean -n", B),
        ("Remove-Item a.txt", P),
        ("cargo publish --dry-run", B),
        ("kubectl get pods", B),
        ("cat 'DROP TABLE.sql'", B),
        ("echo x > out.txt", B),
        ("git log --grep=reset", B),
    ] {
        assert_eq!(irreversible(&segments(c, d)[0], d, &outside), None, "{c}");
    }
}

#[test]
fn path_words_corpus() {
    assert_eq!(
        path_words(&segments("cat /etc/hosts", B)[0], B),
        vec!["/etc/hosts"]
    );
    assert_eq!(path_words(&segments("cd ../x", B)[0], B), vec!["../x"]);
    for c in [
        "curl https://a.b/c",
        "grep -E '/a/' f",
        "echo C: drive",
        "cargo test -- --flag=x",
        "make -C build",
    ] {
        assert!(path_words(&segments(c, B)[0], B).is_empty(), "{c}");
    }
}

#[test]
fn network_commands_and_corpus() {
    for c in ["curl https://x", "wget x", "git clone u", "git -C a fetch"] {
        assert!(is_network(&segments(c, B)[0], B), "{c}");
    }
    assert!(is_network(&segments("iwr https://x", P)[0], P));
    for c in ["git log --grep=curl", "cat curl.txt", "echo wget"] {
        assert!(!is_network(&segments(c, B)[0], B), "{c}");
    }
}

// --- Лексер: разделители, кавычки, подстановки, перенаправления ----------------------

#[test]
fn bash_separators_split_every_part() {
    for (c, n) in [
        ("a; b", 2),
        ("a && b", 2),
        ("a || b", 2),
        ("a | b", 2),
        ("a |& b", 2),
        ("a & b", 2),
        ("a\nb", 2),
        ("a; b && c || d | e & f", 6),
        ("( a; b )", 2),
        ("{ a; b; }", 2),
        ("a;;", 1),
        ("echo 'a; b'", 1),
        ("echo \"a && b\"", 1),
        (r"echo a\;b", 1),
        ("echo a # ; b", 1),
    ] {
        assert_eq!(segments(c, B).len(), n, "{c}");
    }
}

#[test]
fn bash_quotes_and_escapes_are_removed_from_argv() {
    assert_eq!(seg0("\"r\"\"m\" -rf x", B).argv, ["rm", "-rf", "x"]);
    assert_eq!(seg0(r"r\m -rf x", B).argv, ["rm", "-rf", "x"]);
    assert_eq!(seg0("echo 'a b' \"c d\"", B).argv, ["echo", "a b", "c d"]);
    assert_eq!(seg0(r#"echo "a\"b""#, B).argv, ["echo", "a\"b"]);
    assert_eq!(seg0("echo $'a\\'b'", B).argv, ["echo", "a'b"]);
    assert_eq!(seg0("echo a\\\nb", B).argv, ["echo", "ab"]);
    // Переменные не раскрываются, текст сохраняется.
    assert_eq!(seg0("echo $HOME ${X}", B).argv, ["echo", "$HOME", "${X}"]);
    assert_eq!(seg0("ls -la", B).text, "ls -la");
}

#[test]
fn powershell_quotes_and_escapes() {
    assert_eq!(seg0("Write-Host 'it''s'", P).argv, ["Write-Host", "it's"]);
    assert_eq!(seg0("Write-Host \"a`\"b\"", P).argv, ["Write-Host", "a\"b"]);
    assert_eq!(seg0(r"Get-Item C:\x\y", P).argv, ["Get-Item", r"C:\x\y"]);
    assert_eq!(segments("a; b | c && d || e", P).len(), 5);
    assert_eq!(segments("Write-Host 'a; b'", P).len(), 1);
    assert_eq!(segments("ls # comment; rm -r x", P).len(), 1);
    assert_eq!(segments("ls <# block; rm -r x #> ", P).len(), 1);
}

#[test]
fn substitutions_are_marked_and_their_bodies_are_segments() {
    for (c, d, inner) in [
        ("echo $(rm -rf x)", B, "rm -rf x"),
        ("echo `rm -rf x`", B, "rm -rf x"),
        ("cat <(curl x)", B, "curl x"),
        ("tee >(rm -rf x)", B, "rm -rf x"),
        ("echo \"$(id)\"", B, "id"),
        ("echo ${X:-$(id)}", B, "id"),
        ("echo $(echo $(id))", B, "id"),
        ("Write-Host $(Get-Date)", P, "Get-Date"),
        ("Write-Host \"$(Get-Date)\"", P, "Get-Date"),
        ("Write-Host @(Get-Date)", P, "Get-Date"),
        ("Write-Host (Get-Date)", P, "Get-Date"),
        ("gci | % { Remove-Item $_ }", P, "Remove-Item $_"),
    ] {
        let segs = segments(c, d);
        assert!(segs.iter().any(|s| s.has_subst), "{c}");
        assert!(segs.iter().any(|s| s.text == inner), "{c}: {segs:?}");
    }
}

#[test]
fn redirects_are_recorded_with_targets() {
    assert_eq!(seg0("echo x > out.txt", B).redirects, [">out.txt"]);
    assert_eq!(seg0("echo x >> log", B).redirects, [">>log"]);
    assert_eq!(seg0("cmd < in.txt", B).redirects, ["<in.txt"]);
    assert_eq!(seg0("cargo test 2>&1", B).redirects, ["2>&1"]);
    assert_eq!(seg0("cargo test 2> err.log", B).redirects, ["2>err.log"]);
    assert_eq!(seg0("cargo test &> all.log", B).redirects, ["&>all.log"]);
    assert_eq!(seg0("echo x>out.txt", B).argv, ["echo", "x"]);
    assert_eq!(seg0("Get-Date > $null", P).redirects, [">$null"]);
    assert_eq!(seg0("Get-Date *> all.log", P).redirects, ["*>all.log"]);
}

#[test]
fn ambiguous_input_is_unparsed() {
    for (c, d) in [
        ("echo 'a", B),
        ("echo \"a", B),
        ("echo $(a", B),
        ("echo `a", B),
        ("echo a\\", B),
        ("cat <<EOF\nrm -rf x\nEOF", B),
        ("cat <<< 'x'", B),
        ("Write-Host 'a", P),
        ("Write-Host @'\nx\n'@", P),
        ("Write-Host $(a", P),
        ("cmd --% /c del x", P),
        ("pwsh -EncodedCommand ZQBjAGgAbwA=", P),
        ("pwsh -enc ZQBjAGgAbwA=", P),
        ("Get-Date < in.txt", P),
    ] {
        assert!(segments(c, d).iter().any(|s| s.unparsed), "{c}");
        assert!(labels(c, d).contains(&"unparsed"), "{c}");
    }
}

#[test]
fn argv0_strips_wrappers_path_case_and_exe() {
    for (c, d, n) in [
        ("sudo -u root rm -rf x", B, Some("rm")),
        ("/usr/bin/GIT status", B, Some("git")),
        (r"C:\Tools\Cargo.exe test", P, Some("cargo")),
        ("env A=1 B=2 cargo test", B, Some("cargo")),
        ("A=1 cargo test", B, Some("cargo")),
        ("timeout 10 cargo test", B, Some("cargo")),
        ("nice -n 5 make", B, Some("make")),
        ("nohup cargo run", B, Some("cargo")),
        ("xargs -0 -n1 rm", B, Some("rm")),
        ("command rm x", B, Some("rm")),
        ("& git status", P, Some("git")),
        ("$out = git status", P, Some("git")),
        ("then rm x", B, Some("rm")),
        ("A=1", B, None),
        ("command -v rm", B, None),
    ] {
        assert_eq!(argv0(&seg0(c, d)).as_deref(), n, "{c}");
    }
}

#[test]
fn is_simple_allows_only_one_plain_segment() {
    assert!(is_simple(&segments("cargo test", B)));
    assert!(is_simple(&segments("cargo test 2>&1", B)));
    assert!(is_simple(&segments("cargo test -p za-core", P)));
    for c in [
        "cargo test | tail",
        "cargo test > out.txt",
        "cargo test; ls",
        "cargo test $(id)",
        "echo 'a",
        "bash -c 'cargo test'",
        "cargo test 2> err.log",
    ] {
        assert!(!is_simple(&segments(c, B)), "{c}");
    }
    assert!(!is_simple(&segments("& cargo test", P)));
}

#[test]
fn deep_nesting_is_unparsed_without_stack_overflow() {
    let deep_param = format!("echo {}x{}", "${".repeat(20_000), "}".repeat(20_000));
    assert!(labels(&deep_param, B).contains(&"unparsed"));
    let deep_subst = format!("echo {}x{}", "$(".repeat(20_000), ")".repeat(20_000));
    assert!(labels(&deep_subst, B).contains(&"unparsed"));
    let deep_find = format!("find {}-delete", "-exec find ".repeat(20_000));
    assert!(!labels(&deep_find, B).is_empty());
    let deep_block = format!("Write-Host {}x{}", "{".repeat(20_000), "}".repeat(20_000));
    assert!(labels(&deep_block, P).contains(&"unparsed"));
    assert!(labels(r"find . -exec find . -name x \;", B).contains(&"unparsed"));
}

// --- Корпус обходов: выглядит безобидно, но должно дать Ask --------------------------

#[test]
fn bypass_corpus_bash_gets_irreversible_label() {
    for (c, k) in [
        ("echo ok; rm -rf build", "delete"),
        ("cargo test && git push --force", "git_force_push"),
        ("cargo test || rm -rf target", "delete"),
        ("ls | xargs rm -rf", "delete"),
        ("true & rm -rf x", "delete"),
        ("ls\nrm -rf x", "delete"),
        ("echo $(rm -rf x)", "delete"),
        ("echo `rm -rf x`", "delete"),
        ("cat <(rm -rf x)", "delete"),
        ("X=$(rm -rf x) ls", "delete"),
        ("\"r\"\"m\" -rf x", "delete"),
        (r"r\m -rf x", "delete"),
        ("'rm' -rf x", "delete"),
        ("RM=rm; $RM -rf x", "unparsed"),
        ("${RM} -rf x", "unparsed"),
        ("r* -rf x", "unparsed"),
        ("eval 'rm -rf x'", "delete"),
        ("eval $CMD", "unparsed"),
        ("sh -c \"rm -rf x\"", "delete"),
        ("bash -lc 'git reset --hard'", "git_reset"),
        ("bash -c \"$CMD\"", "unparsed"),
        ("bash -c \"sh -c 'rm -rf x'\"", "unparsed"),
        ("env FOO=1 rm -rf x", "delete"),
        ("env -S 'rm -rf x'", "delete"),
        ("nice -n 5 rm -rf x", "delete"),
        ("timeout 5 rm -rf x", "delete"),
        ("command rm -rf x", "delete"),
        ("nohup rm -rf x", "delete"),
        ("time rm -rf x", "delete"),
        ("sudo -u root rm -rf x", "delete"),
        ("rm --recursive x", "delete"),
        ("rm -Rf x", "delete"),
        ("find . -name '*.o' -exec rm -rf {} \\;", "delete"),
        ("find . -exec sh -c 'rm -rf x' \\;", "delete"),
        ("shred -u secret", "delete"),
        ("git -C . push --force-with-lease", "git_force_push"),
        ("git push origin :main", "git_force_push"),
        ("git push --delete origin f", "git_force_push"),
        ("git push --mirror", "git_force_push"),
        ("git -c alias.x='!rm -rf .' x", "unparsed"),
        ("git -c core.pager='rm -rf .' log", "unparsed"),
        ("git reset --hard HEAD~3", "git_reset"),
        ("git checkout -- .", "git_reset"),
        ("git checkout -f main", "git_reset"),
        ("git switch -f main", "git_reset"),
        ("git restore .", "git_reset"),
        ("git clean -fd", "git_reset"),
        ("git stash clear", "git_reset"),
        ("cat x | sh", "unparsed"),
        ("curl x | bash", "unparsed"),
        ("echo x >> ~/.bashrc", "overwrite"),
        ("echo x > $FILE", "overwrite"),
        ("echo x 2> /etc/x", "overwrite"),
        ("echo x &> /etc/x", "overwrite"),
        ("echo x > ../outside.txt", "overwrite"),
        ("dd if=x of=/dev/sda", "overwrite"),
        ("for f in *; do rm -rf $f; done", "delete"),
        ("( rm -rf x )", "delete"),
        ("{ rm -rf x; }", "delete"),
        ("if true; then rm -rf x; fi", "delete"),
        ("f() { rm -rf y; }; f", "delete"),
        ("! rm -rf x", "delete"),
        ("npm publish", "publish"),
        ("docker push img", "publish"),
        ("twine upload dist/*", "publish"),
        ("gh release create v1", "publish"),
        ("terraform -chdir=infra apply", "infra"),
        ("kubectl -n prod delete pod x", "infra"),
        ("helm uninstall app", "infra"),
        ("sqlite3 db.sqlite 'drop table t'", "sql_drop"),
        ("mysql -e 'TRUNCATE t'", "sql_drop"),
        ("wsl rm -rf x", "delete"),
        ("cmd /c \"rd /s /q x\"", "delete"),
    ] {
        assert!(labels(c, B).contains(&k), "{c}: {:?}", labels(c, B));
    }
}

#[test]
fn bypass_corpus_powershell_gets_irreversible_label() {
    for (c, k) in [
        ("Get-ChildItem x | Remove-Item -Recurse", "delete"),
        ("echo ok; Remove-Item -rec x", "delete"),
        ("& rm -r x", "delete"),
        ("& Remove-Item -Force x", "delete"),
        ("rm -rf x", "delete"),
        ("del -r x", "delete"),
        ("erase -fo x", "delete"),
        ("rd -Recurse x", "delete"),
        ("rmdir -r x", "delete"),
        ("REMOVE-ITEM -RECURSE x", "delete"),
        ("iex 'Remove-Item -Recurse x'", "delete"),
        ("Invoke-Expression \"rm -r x\"", "delete"),
        ("iex $code", "unparsed"),
        ("iex (Get-Content x)", "unparsed"),
        ("iwr https://x | iex", "unparsed"),
        ("powershell -Command \"Remove-Item -r x\"", "delete"),
        ("pwsh -NoProfile -c 'ri -r x'", "delete"),
        ("powershell \"Remove-Item -r x\"", "delete"),
        ("pwsh -EncodedCommand ZQBjAGgAbwA=", "unparsed"),
        ("pwsh", "unparsed"),
        ("cmd /c \"rd /s /q x\"", "delete"),
        ("cmd /c del /s x", "delete"),
        ("cmd.exe /C \"rmdir /S x\"", "delete"),
        ("cmd /c %CMD% x", "unparsed"),
        ("cmd /c \"echo a & rd /s /q x\"", "delete"),
        ("cmd /c r^d /s x", "delete"),
        ("cmd", "unparsed"),
        ("$p = 'x'; Remove-Item $p -Recurse", "delete"),
        ("gci | % { Remove-Item $_ -Recurse }", "delete"),
        ("Remove-Item (Get-Item x) -rec", "delete"),
        ("if (Test-Path x) { Remove-Item x -Recurse }", "delete"),
        ("function f { Remove-Item -r x }; f", "delete"),
        ("Start-Process rm -ArgumentList '-rf x'", "delete"),
        (
            "Start-Process powershell -ArgumentList '-c ri -r x'",
            "unparsed",
        ),
        ("[IO.Directory]::Delete('x', $true)", "unparsed"),
        ("$d.Delete()", "unparsed"),
        ("& $cmd -r x", "unparsed"),
        ("& (Get-Command rm) -r x", "unparsed"),
        ("git push -f", "git_force_push"),
        ("git.exe reset --hard", "git_reset"),
        ("echo x > C:\\Windows\\x.txt", "overwrite"),
        ("Write-Output x >> $env:USERPROFILE\\x", "overwrite"),
        ("wsl -e bash -c 'rm -rf x'", "delete"),
        ("Format-Volume -DriveLetter D", "delete"),
        ("robocopy empty x /MIR", "delete"),
    ] {
        assert!(labels(c, P).contains(&k), "{c}: {:?}", labels(c, P));
    }
}

#[test]
fn bypass_corpus_disables_allow_by_substitution_or_unparsed() {
    for (c, d) in [
        ("cargo test $(curl x)", B),
        ("cargo test `id`", B),
        ("cat <(curl x)", B),
        ("bash -c 'cargo test'", B),
        ("eval 'cargo test'", B),
        ("& cargo test", P),
        ("iex 'cargo test'", P),
        ("cmd /c cargo test", P),
        ("Write-Host $(Get-Date)", P),
        ("pwsh -c 'cargo test'", P),
    ] {
        assert!(asks(c, d), "{c}");
        assert!(!is_simple(&segments(c, d)), "{c}");
    }
}

#[test]
fn nested_interpretation_is_parsed_into_segments() {
    let segs = segments("bash -c 'cd x && rm -rf y'", B);
    assert!(segs[0].has_subst);
    assert!(
        segs.iter().any(|s| s.argv == ["rm", "-rf", "y"]),
        "{segs:?}"
    );
    let segs = segments("cmd /c \"del /s x & echo ok\"", P);
    assert!(
        segs.iter().any(|s| s.argv == ["del", "/s", "x"]),
        "{segs:?}"
    );
    let segs = segments("iex 'git push -f'", P);
    assert!(segs.iter().any(|s| argv0(s).as_deref() == Some("git")));
    // Вложенная интерпретация глубже одного уровня не разбирается — Ask.
    let segs = segments("bash -c \"bash -c 'ls'\"", B);
    assert!(segs.iter().any(|s| s.unparsed), "{segs:?}");
}

// --- Корпус ложных срабатываний: обычные команды разработки --------------------------

#[test]
fn dev_commands_are_not_irreversible_bash() {
    for c in [
        "cargo test",
        "cargo test --workspace --locked",
        "cargo build --release",
        "cargo clippy --workspace --all-targets -- -D warnings",
        "cargo fmt --all -- --check",
        "cargo run -- --help",
        "cargo package",
        "cargo test 2>&1 | tail -20",
        "cargo build > /dev/null 2>&1",
        "cargo test 2> err.log",
        "git status",
        "git diff --stat",
        "git log --oneline -5",
        "git add -A",
        "git commit -m 'rm -rf mention and --force'",
        "git checkout main",
        "git checkout -b feat",
        "git switch -c feat",
        "git restore --staged src/a.rs",
        "git branch -d merged",
        "git stash",
        "git stash pop",
        "git push -u origin main",
        "git pull --rebase",
        "git reset HEAD~1",
        "git reset --soft HEAD~1",
        "git rm --cached x",
        "git clean -n -d",
        "git log --grep=reset --oneline",
        "npm run build",
        "npm install",
        "npm test",
        "npm publish --dry-run",
        "npx tsc --noEmit",
        "pnpm -C web build",
        "yarn test",
        "ls -la",
        "ls -la > files.txt",
        "grep -r \"rm -rf\" src",
        "grep -rn 'DROP TABLE' migrations/",
        "rg TODO",
        "python -m pytest",
        "python -m pytest -x -k 'not slow'",
        "python3 -c 'print(1)'",
        "pytest tests/",
        "go test ./...",
        "make",
        "make clean",
        "mkdir -p build",
        "cp a b",
        "mv a b",
        "touch x",
        "rm file.txt",
        "rm -i file.txt",
        "rmdir empty",
        "cat Cargo.toml | head -20",
        "echo done > status.txt",
        "echo rm -rf /",
        "printf '%s\\n' x",
        "find . -name '*.rs'",
        "find . -type f | wc -l",
        "find . -name '*.tmp' -print",
        "sed -i 's/a/b/' file",
        "awk '{print $1}' f",
        "docker build -t x .",
        "docker run --rm x",
        "kubectl get pods",
        "kubectl apply --dry-run=client -f x.yaml",
        "terraform plan",
        "helm list",
        "psql -c 'SELECT 1'",
        "sqlite3 db.sqlite '.tables'",
        "cat drop.sql",
        "timeout 60 cargo test",
        "env RUST_LOG=debug cargo run",
        "xargs -n1 echo",
        "[ -f x ] && echo yes",
        "[[ -d target ]] || mkdir target",
        "export PATH=$PATH:/x",
        "cd crates/za-core && cargo test",
        "bash -c 'cargo test'",
        "bash scripts/check.sh",
        "bash --version",
        "sh -c 'ls -la'",
        "for f in *.rs; do echo $f; done",
        "while true; do sleep 1; done",
        "cargo test # rm -rf x",
        "echo $(date)",
        "echo ${X:-$(date)} ${#Y}",
        "wc -l $(git ls-files)",
    ] {
        assert!(labels(c, B).is_empty(), "{c}: {:?}", labels(c, B));
    }
}

#[test]
fn dev_commands_are_not_irreversible_powershell() {
    for c in [
        "cargo test",
        "cargo test 2>&1 | Select-Object -Last 20",
        "cargo build *> $null",
        "git status",
        "git push",
        "npm run build",
        "Get-ChildItem -Recurse",
        "Get-ChildItem -Recurse -Filter *.rs",
        "Get-Content x | Select-String foo",
        "Remove-Item a.txt",
        "Remove-Item -Recurse:$false x",
        "Copy-Item a b -Recurse",
        "Set-Content out.txt x",
        "New-Item -ItemType Directory -Force x",
        "dotnet test",
        "python -m pytest",
        "Test-Path x",
        "$x = 5",
        "$items = Get-ChildItem",
        "$items = (Get-ChildItem)",
        "$p = 'x'; $p",
        "$env:RUST_LOG = 'debug'; cargo test",
        "gci | ? { $_.Length -gt 0 }",
        "gci | % { $_.Name }",
        "if (Test-Path x) { Write-Host yes } else { Write-Host no }",
        "foreach ($f in $files) { Write-Host $f }",
        "try { cargo test } catch { Write-Host $_ }",
        "cmd /c dir",
        "cmd /c echo hi",
        "pwsh -NoProfile -Command \"Get-ChildItem\"",
        "powershell -File build.ps1",
        "& cargo test",
        "Write-Host (1 + 2)",
        "echo rm -r x",
    ] {
        assert!(labels(c, P).is_empty(), "{c}: {:?}", labels(c, P));
    }
}

// --- Пути и сеть (D77) ---------------------------------------------------------------

#[test]
fn path_words_extracts_outward_paths() {
    for (c, d, want) in [
        ("cat ~/.ssh/id_rsa", B, vec!["~/.ssh/id_rsa"]),
        ("cat x > /etc/out", B, vec!["/etc/out"]),
        ("cat < /etc/passwd", B, vec!["/etc/passwd"]),
        ("dd if=/dev/sda of=/x", B, vec!["/dev/sda", "/x"]),
        ("curl -o /etc/x https://a.b/c", B, vec!["/etc/x"]),
        ("cp a ../../b", B, vec!["../../b"]),
        ("ls --dir=/opt/x", B, vec!["/opt/x"]),
        ("git -C ../other status", B, vec!["../other"]),
        ("git -C sub status", B, vec!["sub"]),
        ("cd", B, vec!["~"]),
        ("cd build", B, vec!["build"]),
        ("pushd /tmp", B, vec!["/tmp"]),
        ("grep -r foo /etc", B, vec!["/etc"]),
        ("grep -e foo /etc/hosts", B, vec!["/etc/hosts"]),
        ("grep -f /etc/patterns x", B, vec!["/etc/patterns"]),
        ("bash -c 'cat /etc/shadow'", B, vec!["/etc/shadow"]),
        (r"type C:\Windows\win.ini", P, vec![r"C:\Windows\win.ini"]),
        (r"Get-Content -Path:C:\x", P, vec![r"C:\x"]),
        ("Set-Location ..", P, vec![".."]),
        ("sl ../x", P, vec!["../x"]),
        (
            r"Get-Content \\server\share\x",
            P,
            vec![r"\\server\share\x"],
        ),
        (r"Copy-Item a D:/b", P, vec!["D:/b"]),
        ("cmd /c type C:\\x.txt", P, vec!["C:\\x.txt"]),
    ] {
        let segs = segments(c, d);
        assert_eq!(path_words(&segs[0], d), want, "{c}");
    }
}

#[test]
fn path_words_false_positive_corpus() {
    for (c, d) in [
        ("echo /etc/hosts", B),
        ("echo https://a.b/c", B),
        ("grep -e '/a/' f", B),
        ("grep 'http://x/' f", B),
        ("sed 's/a/b/' f", B),
        ("sed -e '/x/d' f", B),
        ("awk '/x/ {print}' f", B),
        ("perl -ne 'print if /x/' f", B),
        ("cargo build 2>/dev/null", B),
        ("cargo build > /dev/null", B),
        ("cargo test -p za-core", B),
        ("cargo test --features x", B),
        ("git log --format=%h:%s", B),
        ("rg -g '!/target' foo", B),
        ("docker run -p 8:80 x", B),
        ("git clone git@github.com:a/b.git", B),
        ("ls src/../src", B),
        ("cmd /c del /s /q x", P),
        ("robocopy a b /MIR", P),
        ("Write-Host C:\\x", P),
        ("Select-String -Pattern '/a/' -Path f", P),
        ("Get-Date > $null", P),
        ("echo C: drive", P),
    ] {
        let segs = segments(c, d);
        let words: Vec<String> = segs.iter().flat_map(|s| path_words(s, d)).collect();
        assert!(
            words.iter().all(|w| !outside(w) || w == "src/../src"),
            "{c}: {words:?}"
        );
    }
}

#[test]
fn network_commands_extended() {
    for (c, d) in [
        ("ssh host ls", B),
        ("scp a host:b", B),
        ("sudo curl x", B),
        ("git pull", B),
        ("git push", B),
        ("git ls-remote origin", B),
        ("gh pr view 1", B),
        ("rsync -a x host:/y", B),
        ("bash -c 'curl x'", B),
        ("Invoke-RestMethod https://x", P),
        ("irm https://x", P),
        ("Invoke-WebRequest https://x -OutFile a", P),
        ("curl.exe https://x", P),
        ("wget https://x", P),
    ] {
        assert!(segments(c, d).iter().any(|s| is_network(s, d)), "{c}");
    }
    for (c, d) in [
        ("cargo test --features curl", B),
        ("grep wget file", B),
        ("git log", B),
        ("git status", B),
        ("ls ssh/", B),
        ("rsync -a src/ dst/", B),
        ("echo curl https://x", B),
        ("Write-Host iwr", P),
        ("Get-Content curl.txt", P),
    ] {
        assert!(!segments(c, d).iter().any(|s| is_network(s, d)), "{c}");
    }
}
