//! Сканер секретов и маскировка (R1-C1 D90, D100): известные значения, шаблоны с
//! границей слова слева, PEM без `END`, корпус настоящих форматов и корпус ложных
//! срабатываний. Все значения — фиктивные, но правильного формата.

use std::borrow::Cow;

use za_secrets::{MASK_PREFIX, SecretMasker};

/// Ключ PEM с `END` (тело фиктивное).
const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEFAKEexampleNOTaREALkeyAAAA\nBBBBexample0000\n-----END RSA PRIVATE KEY-----";

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
    assert_eq!(
        m.mask("ghp_0123456789abcdefghijklmnopqrstuvwxyz"),
        "[secret masked: github_token]"
    );
    assert_eq!(
        m.mask("https://u:p4ss@host/x"),
        "https://u:[secret masked: url_password]@host/x"
    );
    assert!(m.mask(PEM).starts_with("[secret masked: private_key]"));
    assert_eq!(m.mask(PEM), "[secret masked: private_key]");
    assert_eq!(
        m.mask("x\n-----BEGIN RSA PRIVATE KEY-----\nAAAA"),
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
    let out = k.mask("id AKIAIOSFODNN7EXAMPLE-tailsecret12 end");
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
    let google = format!("AIza{}", &"Sy0FAKE_example-".repeat(3)[..35]);
    let npm = format!("npm_{}", &"FAKE0example0".repeat(3)[..36]);
    let pypi = format!("pypi-AgEIcHlwaS5vcmc{}", "FAKE_example-0".repeat(5));
    let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let v = |s: &str| s.to_string();
    vec![
        // Ключи провайдеров.
        (
            format!(
                "ANTHROPIC_API_KEY=sk-ant-api03-{}",
                "FAKE0example0AAAA".repeat(5)
            ),
            "api_key",
            format!("sk-ant-api03-{}", "FAKE0example0AAAA".repeat(5)),
        ),
        (
            v("OPENAI_API_KEY=\"sk-proj-FAKEexample1234567890abcdefFAKEexample12\""),
            "api_key",
            v("sk-proj-FAKEexample1234567890abcdefFAKEexample12"),
        ),
        (
            format!("Authorization: Bearer sk-or-v1-{hex64}"),
            "api_key",
            format!("sk-or-v1-{hex64}"),
        ),
        (
            v("key: sk-0123456789abcdef0123456789abcdef"),
            "api_key",
            v("sk-0123456789abcdef0123456789abcdef"),
        ),
        (
            v("stripe sk_live_FAKE0000example0000test"),
            "stripe_key",
            v("sk_live_FAKE0000example0000test"),
        ),
        (
            v("rk_test_FAKE0000example0000test"),
            "stripe_key",
            v("rk_test_FAKE0000example0000test"),
        ),
        (
            format!("GOOGLE_KEY={google}"),
            "google_api_key",
            google.clone(),
        ),
        (
            v("aws_access_key_id = AKIAIOSFODNN7EXAMPLE"),
            "aws_access_key",
            v("AKIAIOSFODNN7EXAMPLE"),
        ),
        (
            v("ASIAIOSFODNN7EXAMPLE"),
            "aws_access_key",
            v("ASIAIOSFODNN7EXAMPLE"),
        ),
        // Токены.
        (
            v("token ghp_0123456789abcdefghijklmnopqrstuvwxyz"),
            "github_token",
            v("ghp_0123456789abcdefghijklmnopqrstuvwxyz"),
        ),
        (
            v("gho_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            "github_token",
            v("gho_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        ),
        (
            v("ghs_0123456789abcdefghijklmnopqrstuvwxyz"),
            "github_token",
            v("ghs_0123456789abcdefghijklmnopqrstuvwxyz"),
        ),
        (
            v("github_pat_11FAKE0000_example0000example0000"),
            "github_token",
            v("github_pat_11FAKE0000_example0000example0000"),
        ),
        (
            // Префикс и тело склеиваются во время выполнения: цельная строка формата
            // токена в исходнике блокируется защитой push от секретов (значение фиктивное).
            ["glpat", "-FAKE0example0000000x"].concat(),
            "gitlab_token",
            ["glpat", "-FAKE0example0000000x"].concat(),
        ),
        (
            v("SLACK=xoxb-0000000000-0000000000000-FAKEexample"),
            "slack_token",
            v("xoxb-0000000000-0000000000000-FAKEexample"),
        ),
        (
            v("hf_FAKEexample0000000000000000000000"),
            "huggingface_token",
            v("hf_FAKEexample0000000000000000000000"),
        ),
        (
            format!("//registry.npmjs.org/:_authToken={npm}"),
            "npm_token",
            npm.clone(),
        ),
        (format!("password = {pypi}"), "pypi_token", pypi.clone()),
        (format!("Authorization: Bearer {jwt}"), "jwt", v(jwt)),
        // Приватные ключи.
        (
            format!("cat id_rsa\n{PEM}\n"),
            "private_key",
            v("MIIEFAKEexampleNOTaREALkeyAAAA"),
        ),
        (
            v(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAFAKE\n-----END OPENSSH PRIVATE KEY-----",
            ),
            "private_key",
            v("b3BlbnNzaC1rZXktdjEAAAAFAKE"),
        ),
        (
            v("-----BEGIN ENCRYPTED PRIVATE KEY-----\\nMIIFAKEencrypted0000\\n"),
            "private_key",
            v("MIIFAKEencrypted0000"),
        ),
        (
            v("-----BEGIN EC PRIVATE KEY-----\nMHcFAKEecKEY000\n-----END EC PRIVATE KEY-----"),
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
    assert_eq!(
        m.mask("git remote set-url origin https://x-access-token:ghp_0123456789abcdefghijklmnopqrstuvwxyz@github.com/o/r.git"),
        "git remote set-url origin https://x-access-token:[secret masked: url_password]@github.com/o/r.git"
    );
    assert_eq!(
        m.mask("Server=db;Password=FakePass123;Timeout=5"),
        "Server=db;Password=[secret masked: connection_password];Timeout=5"
    );
    // Несколько секретов в одном тексте, перевод строки и не-ASCII рядом.
    assert_eq!(
        m.mask("ключ:ghp_0123456789abcdefghijklmnopqrstuvwxyz\nAKIAIOSFODNN7EXAMPLE."),
        "ключ:[secret masked: github_token]\n[secret masked: aws_access_key]."
    );
    // `ghp_` в URL как имя пользователя.
    assert_eq!(
        m.mask("https://ghp_0123456789abcdefghijklmnopqrstuvwxyz@github.com/o/r"),
        "https://[secret masked: github_token]@github.com/o/r"
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
    let corpus = [
        // Хэши git и lock-файлов.
        "commit 84bfdb2c0e5a6f1d9b3e4a7c8d2f0b1e6a9c3d5f",
        "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"",
        "\"integrity\": \"sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUR4jEzE5uKVnnlA3Q==\"",
        "integrity sha1-DdTJ/6K14ZlUqHnFa9Mt5yUVtdk=",
        "\"hash\": \"sha256:5f2c6a8b1e9d4f7a3c0b6e2d8f1a4c7e9b3d5f0a2c4e6b8d0f1a3c5e7b9d1f3a\"",
        // Похожие слова и короткие префиксы.
        "scikit-learn sk-learn pip install sk-video",
        "sk-video-processing-toolkit-extension-pack",
        "disk-0123456789abcdefghijklmnop",
        "AKIAIOSFODNN7EXAMPLEXTRA",
        "AKIAiosfodnn7example",
        "ghp_short",
        "the ghp_ prefix",
        "hf_hub_download(repo_id)",
        "npm_config_registry_url_override_value_x",
        "xoxb-short",
        "eyJhbGciOiJIUzI1NiJ9",
        "use AIza keys",
        "glpat-",
        // URL и адреса без пароля.
        "git@github.com:org/repo.git",
        "ssh://git@host.example/x.git",
        "https://host:8080/path?q=1",
        "mailto:user@example.com",
        "see http://a.b/c:d@e",
        // Пароль-заглушка и ключи без значения.
        "Password=;",
        "Password=$DB_PASS;",
        "Password={password};",
        "--password=xyz",
        "password: hunter2",
        "-----BEGIN PUBLIC KEY-----\nMIIBIjAN\n-----END PUBLIC KEY-----",
        "-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----",
        "",
    ];
    for s in corpus {
        assert_eq!(m.mask(s), s, "{s}");
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
