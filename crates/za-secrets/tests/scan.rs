//! Сканер секретов и маскировка (R1-C1 D90, D100): известные значения, шаблоны с
//! границей слова слева, PEM без `END`, корпус настоящих форматов и корпус ложных
//! срабатываний. Все значения — фиктивные, но правильного формата.
//!
//! Значения в формате настоящих токенов и ключей собираются из частей во время
//! выполнения ([`j`]): цельная строка такого формата в исходнике блокируется защитой
//! push от секретов (GitHub push protection), даже если значение фиктивное.

use std::borrow::Cow;

use za_secrets::{MASK_PREFIX, SecretMasker};

/// Склейка частей фиктивного значения.
fn j(parts: &[&str]) -> String {
    parts.concat()
}

/// Фиктивный токен GitHub (`ghp_` и 36 символов).
fn ghp() -> String {
    j(&["gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"])
}

/// Фиктивный ключ Anthropic.
fn ant() -> String {
    j(&["sk", "-ant-api03-", &"FAKE0example0AAAA".repeat(5)])
}

/// Документационный ключ доступа AWS.
fn aws() -> String {
    j(&["AKIA", "IOSFODNN7EXAMPLE"])
}

/// Первая строка блока PEM приватного ключа вида `kind`.
fn pem_begin(kind: &str) -> String {
    j(&["-----BEGIN ", kind, " PRIVATE", " KEY-----"])
}

/// Последняя строка блока PEM.
fn pem_end(kind: &str) -> String {
    j(&["-----END ", kind, " PRIVATE", " KEY-----"])
}

/// Ключ PEM с `END` (тело фиктивное).
fn pem() -> String {
    format!(
        "{}\nMIIEFAKEexampleNOTaREALkeyAAAA\nBBBBexample0000\n{}",
        pem_begin("RSA"),
        pem_end("RSA")
    )
}

/// Картинка PNG 1×1 в base64.
const BASE64_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn none() -> SecretMasker {
    SecretMasker::new(vec![])
}

fn label(kind: &str) -> String {
    format!("{MASK_PREFIX}{kind}]")
}

/// В маскированном тексте (без самих меток) нет ни одного куска секрета длиной от 6
/// символов.
fn assert_no_part(out: &str, secret: &str) {
    let mut rest = out;
    let mut text = String::new();
    while let Some(p) = rest.find(MASK_PREFIX) {
        text.push_str(&rest[..p]);
        text.push('\u{1}');
        let tail = &rest[p..];
        rest = tail.find(']').map_or("", |e| &tail[e + 1..]);
    }
    text.push_str(rest);
    let out = text.as_str();
    let chars: Vec<char> = secret.chars().collect();
    let w = chars.len().min(6);
    for win in chars.windows(w) {
        let part: String = win.iter().collect();
        assert!(
            !out.contains(&part),
            "кусок {part:?} секрета остался: {out}"
        );
    }
}

#[test]
fn masks_known_and_patterns() {
    let m = SecretMasker::new(vec![("anthropic".into(), "verylongsecretvalue".into())]);
    assert_eq!(
        m.mask("k=verylongsecretvalue"),
        "k=[secret masked: anthropic]"
    );
    assert_eq!(m.mask(&ghp()), "[secret masked: github_token]");
    assert_eq!(
        m.mask("https://u:p4ss@host/x"),
        "https://u:[secret masked: url_password]@host/x"
    );
    assert!(m.mask(&pem()).starts_with("[secret masked: private_key]"));
    assert_eq!(m.mask(&pem()), "[secret masked: private_key]");
    assert_eq!(
        m.mask(&format!("x\n{}\nAAAA", pem_begin("RSA"))),
        "x\n[secret masked: private_key]"
    );
}

#[test]
fn unchanged_text_is_borrowed() {
    let m = SecretMasker::new(vec![("k".into(), "verylongsecretvalue".into())]);
    assert!(matches!(m.mask("ничего секретного"), Cow::Borrowed(_)));
    assert!(matches!(m.mask("x verylongsecretvalue"), Cow::Owned(_)));
}

#[test]
fn known_values_longest_first_short_ignored() {
    let m = SecretMasker::new(vec![
        ("a".into(), "abcdefgh".into()),
        ("long".into(), "abcdefgh-ijkl".into()),
        ("short".into(), "1234567".into()),
    ]);
    assert_eq!(
        m.mask("x abcdefgh-ijkl y abcdefgh 1234567"),
        "x [secret masked: long] y [secret masked: a] 1234567"
    );
}

#[test]
fn overlapping_occurrences_leave_nothing() {
    let m = SecretMasker::new(vec![("n".into(), "aaaaaaaa".into())]);
    // Вхождения с 0 и с 1 перекрываются: последняя `a` — тоже часть секрета.
    assert_eq!(m.mask("aaaaaaaaa!"), "[secret masked: n]!");
    // Шаблон кончается внутри известного значения — маска покрывает оба.
    let k = SecretMasker::new(vec![("tail".into(), "EXAMPLE-tailsecret12".into())]);
    let out = k
        .mask(&format!("id {}-tailsecret12 end", aws()))
        .into_owned();
    assert_eq!(out, "id [secret masked: aws_access_key] end");
    assert_no_part(&out, "EXAMPLE-tailsecret12");
}

#[test]
fn known_value_in_json_escaped_form() {
    let m = SecretMasker::new(vec![("q".into(), "pa\"ss\\word-1".into())]);
    // Строка аргументов вызова (`raw_arguments`) — текст JSON с экранированием.
    let raw = r#"{"cmd":"login pa\"ss\\word-1"}"#;
    assert_eq!(m.mask(raw), r#"{"cmd":"login [secret masked: q]"}"#);
    assert_eq!(m.mask("pa\"ss\\word-1"), "[secret masked: q]");
}

/// Настоящие форматы: (текст, вид, секрет, который не должен остаться).
fn real_corpus() -> Vec<(String, &'static str, String)> {
    let hex64 = "0123456789abcdef".repeat(4);
    let body52 = "FAKE0example0".repeat(4);
    let proj = j(&["sk", "-proj-FAKEexample1234567890abcdefFAKEexample12"]);
    let or = j(&["sk", "-or-v1-", &hex64]);
    let ds = j(&["sk", "-0123456789abcdef0123456789abcdef"]);
    let stripe = j(&["sk", "_live_FAKE0000example0000test"]);
    let rk = j(&["rk", "_test_FAKE0000example0000test"]);
    let google = j(&["AI", "za", &"Sy0FAKE_example-".repeat(3)[..35]]);
    let asia = j(&["ASIA", "IOSFODNN7EXAMPLE"]);
    let gho = j(&["gh", "o_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"]);
    let ghs = j(&["gh", "s_0123456789abcdefghijklmnopqrstuvwxyz"]);
    let pat = j(&["github", "_pat_11FAKE0000_example0000example0000"]);
    let glpat = j(&["glpat", "-FAKE0example0000000x"]);
    let slack = j(&["xox", "b-0000000000-0000000000000-FAKEexample"]);
    let hf = j(&["hf", "_FAKEexample0000000000000000000000"]);
    let npm = j(&["npm", "_", &"FAKE0example0".repeat(3)[..36]]);
    let pypi = j(&["pypi", "-AgEIcHlwaS5vcmc", &"FAKE_example-0".repeat(5)]);
    let gsk = j(&["gsk", "_", &body52]);
    let xai = j(&["xai", "-", &body52]);
    let opaque = j(&["ya29", ".FAKE0example0token0value0123"]);
    let jwt = j(&[
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ".",
        "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ",
        ".",
        "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
    ]);
    let v = |s: &str| s.to_string();
    vec![
        // Ключи провайдеров.
        (format!("ANTHROPIC_API_KEY={}", ant()), "api_key", ant()),
        (
            format!("OPENAI_API_KEY=\"{proj}\""),
            "api_key",
            proj.clone(),
        ),
        (format!("Authorization: Bearer {or}"), "api_key", or.clone()),
        (format!("key: {ds}"), "api_key", ds.clone()),
        (format!("stripe {stripe}"), "stripe_key", stripe.clone()),
        (rk.clone(), "stripe_key", rk.clone()),
        (
            format!("GOOGLE_KEY={google}"),
            "google_api_key",
            google.clone(),
        ),
        (
            format!("aws_access_key_id = {}", aws()),
            "aws_access_key",
            aws(),
        ),
        (asia.clone(), "aws_access_key", asia.clone()),
        (format!("GROQ_API_KEY={gsk}"), "groq_key", gsk.clone()),
        (format!("XAI_API_KEY={xai}"), "xai_key", xai.clone()),
        // Токены.
        (format!("token {}", ghp()), "github_token", ghp()),
        (gho.clone(), "github_token", gho.clone()),
        (ghs.clone(), "github_token", ghs.clone()),
        (pat.clone(), "github_token", pat.clone()),
        (glpat.clone(), "gitlab_token", glpat.clone()),
        (format!("SLACK={slack}"), "slack_token", slack.clone()),
        (hf.clone(), "huggingface_token", hf.clone()),
        (
            format!("//registry.npmjs.org/:_authToken={npm}"),
            "npm_token",
            npm.clone(),
        ),
        (format!("password = {pypi}"), "pypi_token", pypi.clone()),
        (format!("Authorization: Bearer {jwt}"), "jwt", jwt.clone()),
        (
            format!("curl -H 'Authorization: bearer {opaque}'"),
            "bearer_token",
            opaque.clone(),
        ),
        // Приватные ключи.
        (
            format!("cat id_rsa\n{}\n", pem()),
            "private_key",
            v("MIIEFAKEexampleNOTaREALkeyAAAA"),
        ),
        (
            format!(
                "{}\nb3BlbnNzaC1rZXktdjEAAAAFAKE\n{}",
                pem_begin("OPENSSH"),
                pem_end("OPENSSH")
            ),
            "private_key",
            v("b3BlbnNzaC1rZXktdjEAAAAFAKE"),
        ),
        (
            format!("{}\\nMIIFAKEencrypted0000\\n", pem_begin("ENCRYPTED")),
            "private_key",
            v("MIIFAKEencrypted0000"),
        ),
        (
            format!("{}\nMHcFAKEecKEY000\n{}", pem_begin("EC"), pem_end("EC")),
            "private_key",
            v("MHcFAKEecKEY000"),
        ),
        // Строки подключения.
        (
            v("DATABASE_URL=postgres://admin:FakePass123@db.local:5432/app"),
            "url_password",
            v("FakePass123"),
        ),
        (
            v("mongodb+srv://u:p%40FakePass@cluster0.example.net/db"),
            "url_password",
            v("p%40FakePass"),
        ),
        (
            v("redis://:fakeredispass@localhost:6379/0"),
            "url_password",
            v("fakeredispass"),
        ),
        (
            v("Server=db;Database=app;User Id=sa;Password=FakePass123;"),
            "connection_password",
            v("FakePass123"),
        ),
        (
            v("\"ConnectionStrings\": {\"Db\": \"Host=h;Username=u;pwd=FakePwd987\"}"),
            "connection_password",
            v("FakePwd987"),
        ),
        // Сырой текст JSON (`raw_arguments`): токен после экранирования `\n`, `\t`,
        // ` ` (ревью задачи 10, I2).
        (
            format!(r#"{{"cmd":"cd x\nexport GH={}"}}"#, ghp()),
            "github_token",
            ghp(),
        ),
        (
            format!(r#"{{"cmd":"a\n{}"}}"#, ghp()),
            "github_token",
            ghp(),
        ),
        (format!(r#"{{"c":"a\t{}"}}"#, ant()), "api_key", ant()),
        (format!(r#"{{"c":"a {}"}}"#, aws()), "aws_access_key", aws()),
        (
            v(r#"{"c":"Server=db\nPassword=FakePass123"}"#),
            "connection_password",
            v("FakePass123"),
        ),
    ]
}

#[test]
fn real_formats_corpus() {
    let m = none();
    for (text, kind, secret) in real_corpus() {
        let out = m.mask(&text);
        assert!(out.contains(&label(kind)), "{kind}: {text} → {out}");
        assert_no_part(&out, &secret);
    }
}

#[test]
fn pattern_masks_whole_token_and_keeps_context() {
    let m = none();
    let g = ghp();
    assert_eq!(
        m.mask(&format!(
            "git remote set-url origin https://x-access-token:{g}@github.com/o/r.git"
        )),
        "git remote set-url origin https://x-access-token:[secret masked: url_password]@github.com/o/r.git"
    );
    assert_eq!(
        m.mask("Server=db;Password=FakePass123;Timeout=5"),
        "Server=db;Password=[secret masked: connection_password];Timeout=5"
    );
    // Несколько секретов в одном тексте, перевод строки и не-ASCII рядом.
    assert_eq!(
        m.mask(&format!("ключ:{g}\n{}.", aws())),
        "ключ:[secret masked: github_token]\n[secret masked: aws_access_key]."
    );
    // `ghp_` в URL как имя пользователя.
    assert_eq!(
        m.mask(&format!("https://{g}@github.com/o/r")),
        "https://[secret masked: github_token]@github.com/o/r"
    );
    // После `Bearer` — только токен.
    assert_eq!(
        m.mask(&format!("Authorization: Bearer {g}")),
        "Authorization: Bearer [secret masked: github_token]"
    );
}

#[test]
fn mask_false_positive_corpus() {
    let m = SecretMasker::new(vec![]);
    for s in [
        "sk-learn",
        "task-runner-integration-suite",
        "risk-assessment-2026",
        "550e8400-e29b-41d4-a716-446655440000",
        "07bbac0f1a2b3c4d5e6f",
        "AKIA is an AWS prefix",
        "myghp_value",
        "https://example.com/user@host",
        BASE64_PNG,
    ] {
        assert_eq!(m.mask(s), s, "{s}");
    }
}

#[test]
fn false_positive_corpus_extended() {
    let m = none();
    let v = |s: &str| s.to_string();
    let corpus = [
        // Хэши git и lock-файлов.
        v("commit 84bfdb2c0e5a6f1d9b3e4a7c8d2f0b1e6a9c3d5f"),
        v("checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\""),
        v(
            "\"integrity\": \"sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUR4jEzE5uKVnnlA3Q==\"",
        ),
        v("integrity sha1-DdTJ/6K14ZlUqHnFa9Mt5yUVtdk="),
        v("\"hash\": \"sha256:5f2c6a8b1e9d4f7a3c0b6e2d8f1a4c7e9b3d5f0a2c4e6b8d0f1a3c5e7b9d1f3a\""),
        // Похожие слова и короткие префиксы.
        v("scikit-learn sk-learn pip install sk-video"),
        v("sk-video-processing-toolkit-extension-pack"),
        // Имя с хэш-суффиксом (ревью задачи 10, m4).
        v("pip install sk-video-0123456789abcdefgh"),
        v("disk-0123456789abcdefghijklmnop"),
        j(&["AKIA", "IOSFODNN7EXAMPLEXTRA"]),
        j(&["AKIA", "iosfodnn7example"]),
        v("ghp_short"),
        v("the ghp_ prefix"),
        v("hf_hub_download(repo_id)"),
        v("npm_config_registry_url_override_value_x"),
        v("gsk_short xai-short"),
        v("xoxb-short"),
        v("eyJhbGciOiJIUzI1NiJ9"),
        v("use AIza keys"),
        v("glpat-"),
        // `Bearer` без токена.
        v("Bearer authentication-scheme-description"),
        v("Authorization: Bearer $TOKEN"),
        v("Bearer ${GITHUB_TOKEN}"),
        // Перевод строки после токена: следующая строка — не хвост.
        // URL и адреса без пароля.
        v("git@github.com:org/repo.git"),
        v("ssh://git@host.example/x.git"),
        v("https://host:8080/path?q=1"),
        v("mailto:user@example.com"),
        v("see http://a.b/c:d@e"),
        // Пароль-заглушка и ключи без значения.
        v("Password=;"),
        v("Password=$DB_PASS;"),
        v("Password={password};"),
        v("--password=xyz"),
        v("password: hunter2"),
        // Экранированная обратная косая черта перед `n` — не перевод строки.
        j(&[r#"{"c":"a\\n"#, &ghp(), "\"}"]),
        v("-----BEGIN PUBLIC KEY-----\nMIIBIjAN\n-----END PUBLIC KEY-----"),
        v("-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----"),
        v(""),
    ];
    for s in &corpus {
        assert_eq!(m.mask(s), s.as_str(), "{s}");
    }
}

/// Хвост токена, перенесённый на следующую строку, маскируется (m1); обычная следующая
/// строка — нет.
#[test]
fn wrapped_token_tail_is_masked() {
    let m = none();
    let key = ant();
    let (a, b) = key.split_at(40);
    for text in [
        format!("{a}\n{b}"),
        format!("{a}\r\n{b}"),
        format!(r#"{{"c":"{a}\n{b}"}}"#),
    ] {
        let out = m.mask(&text);
        assert!(out.contains(&label("api_key")), "{text} → {out}");
        assert_no_part(&out, b);
    }
    let out = m.mask(&format!("{}\nexport FOO=1", ghp())).into_owned();
    assert_eq!(out, "[secret masked: github_token]\nexport FOO=1");
}

/// Известное значение находится и разрезанным переносом строки; регистр значим (m2).
#[test]
fn known_value_split_by_line_break() {
    let m = SecretMasker::new(vec![("k".into(), "verylongsecretvalue".into())]);
    assert_eq!(
        m.mask("x verylongsecret\nvalue y"),
        "x [secret masked: k] y"
    );
    assert_eq!(m.mask("x verylong\r\nsecretvalue"), "x [secret masked: k]");
    assert_eq!(
        m.mask(r#"{"c":"verylongsecret\nvalue"}"#),
        r#"{"c":"[secret masked: k]"}"#
    );
    assert_eq!(m.mask("VERYLONGSECRETVALUE"), "VERYLONGSECRETVALUE");
}

/// Линейность (I1): число шагов сканера на входе 1 МиБ не больше 64 на байт, в том числе
/// на цепочках, где префикс шаблона повторяется внутри тела.
#[test]
fn scanner_is_linear_on_pathological_input() {
    const MIB: usize = 1 << 20;
    const PER_BYTE: u64 = 64;
    let fill = |unit: &str| unit.repeat(MIB / unit.len());
    let plain = none();
    let known = SecretMasker::new(vec![
        ("a".into(), "aaaaaaaa".into()),
        ("ab".into(), "ab".repeat(50)),
        ("n".into(), "x\ny\nz\nw\nv".repeat(2)),
    ]);
    let cases = [
        fill("sk-a-"),
        fill("eyJaa-"),
        fill("eyJaaaaaaaaaaa.eyJ"),
        fill("xoxb-a"),
        fill("AIza-"),
        fill("glpat-"),
        fill("Bearer a1-"),
        fill("gsk_a"),
        fill("-----BEGIN x"),
        fill(" Password=$"),
        fill(" Password=a"),
        fill("u://a:b"),
        fill("\\nghp_"),
        fill("\\\\\\\\n"),
        fill("a"),
        fill("ab"),
        fill("x\n"),
        fill("сек"),
    ];
    for text in &cases {
        for m in [&plain, &known] {
            let (_, steps) = m.mask_with_steps(text);
            let limit = PER_BYTE * text.len() as u64;
            assert!(
                steps <= limit,
                "{steps} шагов на {} байт ({:?}…)",
                text.len(),
                &text[..12.min(text.len())]
            );
        }
    }
}

/// Детерминированные псевдослучайные байты (xorshift) в base64: длинные данные
/// не дают ложных срабатываний.
#[test]
fn random_base64_data_is_untouched() {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = String::new();
    for _ in 0..(256 * 1024) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(char::from(ABC[usize::try_from(x % 64).unwrap()]));
        if out.len() % 77 == 76 {
            out.push('\n');
        }
    }
    let m = none();
    assert_eq!(m.mask(&out), out.as_str());
}

#[test]
fn debug_hides_values() {
    let m = SecretMasker::new(vec![("anthropic".into(), "verylongsecretvalue".into())]);
    let d = format!("{m:?}");
    assert!(!d.contains("verylongsecretvalue"), "{d}");
}

#[test]
fn masked_text_is_stable() {
    let m = SecretMasker::new(vec![("k".into(), "verylongsecretvalue".into())]);
    for (text, _, _) in real_corpus() {
        let once = m.mask(&text).into_owned();
        assert_eq!(m.mask(&once), once.as_str(), "{text}");
    }
}

/// Маска потока (D100, I3 ревью задачи 12): известное значение и токен, разрезанные между
/// фрагментами в любом месте, маскируются целиком; склейка отданных частей равна маске
/// всего текста.
#[test]
fn stream_mask_holds_window_across_chunks() {
    let m = SecretMasker::new(vec![("k".into(), "verylongsecretvalue".into())]);
    let text = format!("{} verylongsecretvalue mid {} tail", "a".repeat(700), ghp());
    for cut in [705, 710, 720, 730, 735, 745, 760] {
        let (a, b) = text.split_at(cut);
        let mut held = String::new();
        let mut out = String::new();
        for (chunk, last) in [(a, false), (b, false), ("", true)] {
            held.push_str(chunk);
            let (part, used) = m.mask_stream(&held, last);
            out.push_str(&part);
            held = held[used..].to_owned();
        }
        assert_eq!(out, m.mask(&text), "разрез на {cut}");
        assert!(!out.contains("verylong") && !out.contains("ghp_"), "{cut}");
    }
    assert!(m.window() >= "verylongsecretvalue".len());
}
