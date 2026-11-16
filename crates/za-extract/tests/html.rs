//! HTML в Markdown (R1-C2 D109, D127): пропуск служебных элементов, `<title>` отдельным
//! полем, обрезка входа до разбора, картинки `data:`, пустые строки; глубокая вложенность
//! роняет только отдельный процесс.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use za_extract::{HTML_MAX_BYTES, html_to_markdown};

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn time_scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 6))
}

#[test]
fn html_to_markdown_drops_scripts_and_head() {
    let html = "<html><head><title>T</title><script>evil()</script></head><body><h1>Head</h1><p>Hi <a href=\"/x\">link</a></p><img src=\"data:image/png;base64,AAAA\"></body></html>";
    let m = html_to_markdown(html);
    assert_eq!(m.title.as_deref(), Some("T"));
    assert!(
        m.text.contains("# Head")
            && m.text.contains("[link](/x)")
            && !m.text.contains("evil")
            && !m.text.contains("data:"),
        "{:?}",
        m.text
    );
    assert!(!m.text.starts_with('T'), "{:?}", m.text); // текст <title> не попал в тело
}

/// Вход больше 2 МиБ режется до разбора: метка после предела в текст не попадает, разбор
/// 3 МиБ занимает время разбора 2 МиБ (предел времени — от квадратичного разрастания).
#[test]
fn html_over_2_mib_is_cut_before_parsing() {
    let para = "<p>alpha beta gamma delta</p>\n";
    let mut html = String::from("<html><body><h1>Start</h1>\n");
    while html.len() < HTML_MAX_BYTES + 1000 {
        html.push_str(para);
    }
    html.push_str("<p>TAILMARKER</p>\n");
    while html.len() < 3 * 1024 * 1024 {
        html.push_str(para);
    }
    html.push_str("</body></html>");

    let started = Instant::now();
    let m = html_to_markdown(&html);
    let took = started.elapsed();

    assert!(
        m.text.starts_with("# Start"),
        "{:?}",
        &m.text[..40.min(m.text.len())]
    );
    assert!(m.text.contains("alpha beta gamma delta"));
    assert!(!m.text.contains("TAILMARKER"), "вход не обрезан до разбора");
    assert!(m.text.len() <= HTML_MAX_BYTES, "{}", m.text.len());
    let limit = Duration::from_secs(60) * time_scale();
    assert!(
        took < limit,
        "разбор 3 МиБ занял {took:?} (предел {limit:?})"
    );
}

/// Предел приходится на середину многобайтового символа: обрезка по границе символа.
#[test]
fn cut_respects_char_boundary() {
    let mut html = String::from("<p>xy");
    while html.len() < HTML_MAX_BYTES + 10 {
        html.push('я');
    }
    assert!(
        !html.is_char_boundary(HTML_MAX_BYTES),
        "граница должна быть внутри символа"
    );
    let m = html_to_markdown(&html);
    assert!(m.text.starts_with("xyя"));
    assert!(m.text.len() <= HTML_MAX_BYTES);
}

#[test]
fn page_without_title_has_none() {
    let m = html_to_markdown("<p>only body</p>");
    assert_eq!(m.title, None);
    assert_eq!(m.text, "only body");
    // Пустой и пробельный <title> — тоже нет заголовка.
    assert_eq!(html_to_markdown("<title> \n </title><p>x</p>").title, None);
}

/// Заголовок — текст первого `<title>` (пробелы схлопнуты, без экранирования Markdown);
/// `<title>` в теле и `<title>` внутри SVG в текст не попадают.
#[test]
fn title_is_plain_text_of_first_title() {
    let html = "<head><title>  A *b*\n  &amp; [c] </title></head>\
                <body><svg><title>svg title</title></svg><p>body</p><title>second</title></body>";
    let m = html_to_markdown(html);
    assert_eq!(m.title.as_deref(), Some("A *b* & [c]"));
    assert_eq!(m.text, "body");
    // SVG-заголовок не считается заголовком страницы.
    let m = html_to_markdown("<body><svg><title>svg only</title></svg><p>x</p></body>");
    assert_eq!(m.title, None);
}

/// Список пропуска D109: содержимое служебных и встраиваемых элементов не попадает в текст.
#[test]
fn skipped_elements_are_dropped() {
    let html = "<html><head><style>.s{color:red}</style><meta charset=utf-8>\
                <link rel=stylesheet href=/s.css><script>head_js()</script></head><body>\
                <p>visible one</p>\
                <script>body_js()</script><style>p{}</style>\
                <noscript>noscript_text</noscript>\
                <template><p>template_text</p></template>\
                <svg><text>svg_text</text></svg>\
                <canvas>canvas_text</canvas>\
                <iframe src=\"/f\">iframe_text</iframe>\
                <object data=\"/o\">object_text</object>\
                <embed src=\"/e\">\
                <form action=\"/login\"><input name=password>form_text</form>\
                <p>visible two</p></body></html>";
    let m = html_to_markdown(html);
    for gone in [
        "color",
        "head_js",
        "body_js",
        "noscript_text",
        "template_text",
        "svg_text",
        "canvas_text",
        "iframe_text",
        "object_text",
        "form_text",
        "/e",
        "/f",
        "/o",
        "/login",
    ] {
        assert!(!m.text.contains(gone), "{gone} в тексте: {:?}", m.text);
    }
    assert!(
        m.text.contains("visible one") && m.text.contains("visible two"),
        "{:?}",
        m.text
    );
}

/// Картинки `data:` (в любом регистре и с пробелами) удаляются, обычные остаются.
#[test]
fn data_images_dropped_other_images_kept() {
    let html = "<p>a <img src=\" DATA:image/png;base64,QUJD\" alt=\"inline\"> b</p>\
                <p><img src=\"/pic.png\" alt=\"pic\"></p>";
    let m = html_to_markdown(html);
    assert!(
        !m.text.contains("base64") && !m.text.contains("inline"),
        "{:?}",
        m.text
    );
    assert!(m.text.contains("![pic](/pic.png)"), "{:?}", m.text);
}

/// Три и более переводов строки подряд (с пробельными строками между ними) сжимаются в одну
/// пустую строку; в начале и в конце пустых строк нет.
#[test]
fn blank_line_runs_are_collapsed() {
    let html = "<div>\n\n</div><p>one</p><div> </div><div>\n \n</div><script>x</script>\
                <style>y</style><p>two</p><pre>a\n\n\n\nb</pre><div>\n</div>";
    let m = html_to_markdown(html);
    let lines: Vec<&str> = m.text.split('\n').collect();
    for w in lines.windows(2) {
        assert!(
            !(w[0].trim().is_empty() && w[1].trim().is_empty()),
            "две пустые строки подряд: {:?}",
            m.text
        );
    }
    assert!(
        m.text.starts_with("one") && m.text.contains("two"),
        "{:?}",
        m.text
    );
    assert!(
        !m.text.ends_with('\n') && !m.text.starts_with('\n'),
        "{:?}",
        m.text
    );
}

/// Глубина вложенности тестовой страницы: около 2 МиБ открывающих тегов `<span>` —
/// в пределах входа D109.
fn deep_html() -> String {
    let depth = (HTML_MAX_BYTES - 64) / "<span>".len();
    let mut html = String::with_capacity(HTML_MAX_BYTES);
    html.push_str("<p>");
    for _ in 0..depth {
        html.push_str("<span>");
    }
    html.push('x');
    html
}

/// Подпроцесс теста [`deep_nesting_kills_only_child`]: в обычном прогоне пропущен, а при
/// `--include-ignored` без переменной `ZA_EXTRACT_DEEP_CHILD` ничего не делает.
#[test]
#[ignore = "подпроцесс deep_nesting_kills_only_child"]
fn deep_nesting_child() {
    if std::env::var_os("ZA_EXTRACT_DEEP_CHILD").is_none() {
        return;
    }
    let m = html_to_markdown(&deep_html());
    println!("child-done {}", m.text.len());
}

/// D127: рекурсивный обход DOM у `htmd` на глубокой вложенности переполняет стек. Такое
/// падение `catch_unwind` не ловит, поэтому разбор идёт только в отдельном процессе: умирает
/// он, а запустивший процесс получает код завершения и продолжает работу. Подпроцесс здесь —
/// сам тестовый бинарник (`CARGO_BIN_EXE_*` есть только у пакета-владельца бинарника,
/// `zero-agent`; сквозной путь через `za_jail::Runner` — его тест, задача 10).
///
/// Если подпроцесс завершился успешно, обход `htmd` больше не рекурсивный — пересмотреть D127.
#[test]
fn deep_nesting_kills_only_child() {
    let exe = std::env::current_exe().expect("путь тестового бинарника");
    let mut child = Command::new(exe)
        .args(["deep_nesting_child", "--exact", "--ignored", "--nocapture"])
        .args(["--test-threads", "1"])
        .env("ZA_EXTRACT_DEEP_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("запуск подпроцесса");
    let deadline = Instant::now() + Duration::from_secs(60) * time_scale();
    let status = loop {
        if let Some(s) = child.try_wait().expect("ожидание подпроцесса") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("подпроцесс не завершился до предела");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    assert!(
        !status.success() && !out.contains("child-done"),
        "подпроцесс разобрал вложенность глубиной {} без падения ({status:?}): \
         обход htmd больше не рекурсивный — пересмотреть D127\n{out}",
        deep_html().len() / 6
    );
    assert!(
        err.contains("overflowed its stack"),
        "подпроцесс упал не от переполнения стека ({status:?}):\n{err}"
    );
}
