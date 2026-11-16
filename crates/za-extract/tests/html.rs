//! HTML в Markdown (R1-C2 D109, D127): пропуск служебных элементов и элементов управления,
//! `<title>` отдельным полем, обрезка входа до разбора, картинки `data:`, пустые строки;
//! онлайн-страж разбора — слишком сложная страница даёт упрощённое извлечение (`simplified`),
//! а не зависание (ревью задачи 7). Падение подпроцесса — `tests/crash.rs` (под `test-hooks`).

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use za_extract::{HTML_MAX_BYTES, Markdown, html_to_markdown};

const MIB: usize = 1024 * 1024;

/// Множитель времени тестов: `ZA_TEST_TIME_SCALE` (целое 1–6, по умолчанию 1).
fn time_scale() -> u32 {
    std::env::var("ZA_TEST_TIME_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 6))
}

/// Разбор с жёстким пределом времени: при регрессии стража (квадратичный разбор) тест падает
/// быстро, а не висит минутами. Поток с большим стеком — страховка на случай, если страж
/// пропустит глубину и `htmd` уйдёт в рекурсию.
fn bounded(label: &str, html: String) -> Markdown {
    let limit = Duration::from_secs(30) * time_scale();
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    thread::Builder::new()
        .stack_size(64 * MIB)
        .spawn(move || {
            let m = html_to_markdown(&html);
            let _ = tx.send(m);
        })
        .expect("поток разбора");
    match rx.recv_timeout(limit) {
        Ok(m) => {
            println!("{label}: {:?}", started.elapsed());
            m
        }
        Err(_) => panic!("{label}: разбор не уложился в {limit:?} — страж не сработал"),
    }
}

#[test]
fn html_to_markdown_drops_scripts_and_head() {
    let html = "<html><head><title>T</title><script>evil()</script></head><body><h1>Head</h1><p>Hi <a href=\"/x\">link</a></p><img src=\"data:image/png;base64,AAAA\"></body></html>";
    let m = html_to_markdown(html);
    assert_eq!(m.title.as_deref(), Some("T"));
    assert!(!m.simplified);
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

/// I1 ревью задачи 7: страница ASP.NET WebForms целиком обёрнута в один `<form>`. Её текст
/// должен сохраниться — `form` больше не пропускается.
#[test]
fn aspx_whole_page_in_form_keeps_text() {
    let html = "<html><body><form method=post action=./Default.aspx id=form1>\
                <h1>Title</h1><p>Article body</p></form></body></html>";
    let m = html_to_markdown(html);
    assert!(!m.simplified);
    assert!(
        m.text.contains("# Title") && m.text.contains("Article body"),
        "{:?}",
        m.text
    );
}

/// I1: пропускаются только сами элементы управления, а текст формы вокруг них остаётся.
#[test]
fn form_controls_are_dropped_text_around_kept() {
    let html = "<form action=\"/login\"><p>Please sign in</p>\
                <label>Name</label><input name=user value=secret>\
                <select><option>one</option><option>two</option></select>\
                <button>Submit now</button><textarea>draft text</textarea>\
                <p>After the form</p></form>";
    let m = html_to_markdown(html);
    for gone in ["secret", "one", "two", "Submit now", "draft text"] {
        assert!(!m.text.contains(gone), "{gone} в тексте: {:?}", m.text);
    }
    for kept in ["Please sign in", "Name", "After the form"] {
        assert!(m.text.contains(kept), "нет {kept}: {:?}", m.text);
    }
}

/// Вход больше 2 МиБ режется до разбора: метка после предела в текст не попадает.
#[test]
fn html_over_2_mib_is_cut_before_parsing() {
    let para = "<p>alpha beta gamma delta</p>\n";
    let mut html = String::from("<html><body><h1>Start</h1>\n");
    while html.len() < HTML_MAX_BYTES + 1000 {
        html.push_str(para);
    }
    html.push_str("<p>TAILMARKER</p>\n");
    while html.len() < 3 * MIB {
        html.push_str(para);
    }
    html.push_str("</body></html>");

    let m = bounded("over_2mib", html);
    assert!(!m.simplified);
    assert!(
        m.text.starts_with("# Start"),
        "{:?}",
        &m.text[..40.min(m.text.len())]
    );
    assert!(m.text.contains("alpha beta gamma delta"));
    assert!(!m.text.contains("TAILMARKER"), "вход не обрезан до разбора");
    assert!(m.text.len() <= HTML_MAX_BYTES, "{}", m.text.len());
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

/// Список пропуска D109 (I1): служебные, встраиваемые элементы и элементы управления.
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
                <input name=password value=inputval>\
                <select><option>optval</option></select>\
                <button>buttonval</button><textarea>areaval</textarea>\
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
        "inputval",
        "optval",
        "buttonval",
        "areaval",
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

/// Три и более переводов строки подряд сжимаются в одну пустую строку; краёв без пустых строк.
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

// --- Онлайн-страж: классы из ревью задачи 7 дают `simplified`, а не зависание ---

/// Повтор строки `unit` до размера `bytes` (по границе целого `unit`).
fn rep(unit: &str, bytes: usize) -> String {
    unit.repeat(bytes / unit.len().max(1))
}

/// Глубокая вложенность `<span>` (переполняла стек `htmd`): страж ловит глубину → `simplified`.
#[test]
fn deep_nesting_is_simplified() {
    let depth = (2 * MIB - 64) / "<span>".len();
    let mut html = String::with_capacity(2 * MIB);
    html.push_str("<p>");
    html.push_str(&"<span>".repeat(depth));
    html.push_str("deep text x");
    let m = bounded("deep_span", html);
    assert!(
        m.simplified,
        "глубокая вложенность должна давать simplified"
    );
    assert!(m.text.contains("deep text x"), "{:?}", m.text);
}

/// Глубокие `<div>` (квадратичная проверка области видимости `html5ever`) → `simplified`.
#[test]
fn deep_div_is_simplified() {
    let m = bounded("deep_div", rep("<div>", 2 * MIB));
    assert!(m.simplified);
}

/// Реконструкция активных элементов форматирования (`<p><b id=N>x</p>`) разрастает DOM до
/// миллионов узлов: страж ловит глубину/число элементов → `simplified`.
#[test]
fn formatting_reconstruction_is_simplified() {
    let mut html = String::new();
    let mut i = 0usize;
    while html.len() < 2 * MIB {
        html.push_str(&format!("<p><b id={i}>x</p>"));
        i += 1;
    }
    let m = bounded("reconstruct", html);
    assert!(m.simplified);
}

/// «Noah's Ark»: 16 незакрытых `<b>` с разными атрибутами, затем море `<p>x</p>` —
/// реконструкция создаёт миллионы узлов. Страж ловит число элементов → `simplified`.
#[test]
fn noahs_ark_is_simplified() {
    let mut html = String::new();
    for k in 0..16 {
        html.push_str(&format!("<p><b id={k}>x</p>"));
    }
    html.push_str(&rep("<p>x</p>", 2 * MIB - html.len()));
    let m = bounded("noahs_ark", html);
    assert!(m.simplified);
}

/// Слияние соседних одинаковых строчных элементов у `htmd` (квадратично по числу детей):
/// серии по 5000 `<b>x</b>`. Страж ловит цену слияния в `dom_check` → `simplified`.
#[test]
fn adjacent_merge_is_simplified() {
    let unit = format!("{} ", "<b>x</b>".repeat(5000));
    let m = bounded("adjacent_merge", rep(&unit, 2 * MIB));
    assert!(m.simplified);
}

// --- Атрибуты: проверка повторов `html5ever` квадратична по их числу в теге (fix-8) ---

/// Тег `<div a0 a1 …>` на 2 МиБ — сотни тысяч разных атрибутов (прежде ≈ 75 с в release):
/// подсчёт до токенизатора даёт `simplified` быстро, текст до тега сохраняется.
#[test]
fn huge_attribute_tag_is_simplified() {
    let mut html = String::from("<p>before the tag</p><div");
    let mut i = 0usize;
    while html.len() < 2 * MIB - 64 {
        html.push_str(&format!(" a{i}"));
        i += 1;
    }
    html.push_str(">after</div>");
    assert!(i > 200_000, "{i}");
    let m = bounded("huge_attrs", html);
    assert!(
        m.simplified,
        "тег с {i} атрибутами должен давать simplified"
    );
    assert!(m.text.contains("before the tag"), "{:?}", &m.text[..80]);
}

/// Много тегов ровно по пределу (1024 разных атрибута каждый) на 2 МиБ: по отдельности каждый
/// в норме, полный разбор идёт, а квадратичная цена ограничена пределом на тег (замер —
/// около 0,5 с в release, отчёт fix-8). Тег на один атрибут больше — `simplified`.
#[test]
fn attribute_limit_is_per_tag() {
    let tag = |n: usize| {
        let mut t = String::from("<i");
        for k in 0..n {
            t.push_str(&format!(" a{k}"));
        }
        t.push_str(">x</i>\n");
        t
    };
    let m = bounded("attr_tags_at_limit", rep(&tag(1024), 2 * MIB));
    assert!(!m.simplified, "теги по пределу разбираются полностью");
    let m = bounded("attr_tag_over_limit", tag(1025));
    assert!(m.simplified, "1025 атрибутов в теге — simplified");
}

/// Тысяча разных атрибутов и поток повторов в одном теге: повтор стоит прохода по уже
/// собранным, поэтому повторы тоже считаются.
#[test]
fn duplicate_attribute_flood_is_simplified() {
    let mut html = String::from("<b");
    for k in 0..1000 {
        html.push_str(&format!(" a{k}"));
    }
    while html.len() < 2 * MIB - 16 {
        html.push_str(" x");
    }
    html.push_str(">text</b>");
    let m = bounded("dup_attrs", html);
    assert!(m.simplified);
}

/// Кавычка внутри `<script>` не прячет тег после него: подсчёт ведёт и гипотезу «вне тега»,
/// а `>` в значениях в кавычках счёт не обнуляет.
#[test]
fn quoted_values_in_script_do_not_hide_attribute_flood() {
    let mut html = String::from("<script>var s = x<a b='</script><div");
    let mut i = 0usize;
    while html.len() < 2 * MIB - 64 {
        html.push_str(&format!(" a{i}=\">\""));
        i += 1;
    }
    html.push_str(">tail</div>");
    let m = bounded("quoted_attrs", html);
    assert!(m.simplified);
}

// --- Обычные страницы не деградируют ---

/// Семь синтетических страниц, по строению близких к реальным (статья, документация,
/// форум с вложенными ответами, навигация, большая таблица, список ссылок, смесь). Ни одна
/// не должна попасть под страж: полный разбор, `simplified == false`, текст содержательный.
#[test]
fn realistic_pages_are_not_simplified() {
    for (name, html) in realistic_pages() {
        let m = bounded(&format!("page {name}"), html);
        assert!(!m.simplified, "{name}: ложное упрощение");
        assert!(
            m.text.len() > 200,
            "{name}: пустой результат ({} байт)",
            m.text.len()
        );
    }
}

/// Строит семь страниц с разнообразным, но обычным строением.
fn realistic_pages() -> Vec<(&'static str, String)> {
    // 1. Длинная статья: разделы, заголовки, абзацы со ссылками и форматированием, списки.
    let mut article =
        String::from("<!doctype html><html><head><title>Article</title></head><body><main>");
    let mut i = 0;
    while article.len() < MIB {
        article.push_str(&format!(
            "<section><h2>Section {i}</h2>\
             <p>Some prose with a <a href=\"/p/{i}\">link</a>, <b>bold</b>, <em>emphasis</em> and <code>code</code> inside it.</p>\
             <ul><li>first point</li><li>second <a href=\"/x/{i}\">point</a></li><li>third point</li></ul>\
             <blockquote>A quoted remark about item {i}.</blockquote></section>\n"
        ));
        i += 1;
    }
    article.push_str("</main></body></html>");

    // 2. Документация: умеренно глубокая вложенность (nav + article + секции).
    let mut docs = String::from("<html><body><nav><ul>");
    for i in 0..200 {
        docs.push_str(&format!("<li><a href=\"#s{i}\">Item {i}</a></li>"));
    }
    docs.push_str("</ul></nav><article>");
    let mut i = 0;
    while docs.len() < 400 * 1024 {
        docs.push_str(&format!(
            "<div class=block><div class=inner><h3 id=s{i}>Topic {i}</h3>\
             <p>Documentation paragraph number {i} with a <code>fn example()</code> call.</p>\
             <pre><code>let x = {i};\nprintln!(\"{{}}\", x);</code></pre></div></div>"
        ));
        i += 1;
    }
    docs.push_str("</article></body></html>");

    // 3. Форум: вложенные ответы (глубина растёт, но ограничена ~40).
    let mut forum = String::from("<html><body><div class=thread>");
    let mut depth = 0;
    for i in 0..3000 {
        forum.push_str(&format!(
            "<div class=comment><p>Comment {i} by user{i}: I think this is reasonable.</p>"
        ));
        depth += 1;
        if depth >= 40 {
            forum.push_str(&"</div>".repeat(depth));
            depth = 0;
        }
    }
    forum.push_str(&"</div>".repeat(depth + 1));
    forum.push_str("</body></html>");

    // 4. Навигация и ссылки: много мелких ссылок, короткие абзацы.
    let mut links = String::from("<html><body>");
    let mut i = 0;
    while links.len() < 300 * 1024 {
        links.push_str(&format!(
            "<p>Row {i}: <a href=\"/a/{i}\">alpha</a> | <a href=\"/b/{i}\">beta</a> | <a href=\"/c/{i}\">gamma</a></p>"
        ));
        i += 1;
    }
    links.push_str("</body></html>");

    // 5. Большая таблица: много строк, умеренное число столбцов.
    let mut table = String::from(
        "<html><body><table><thead><tr><th>Name</th><th>Kind</th><th>Note</th></tr></thead><tbody>",
    );
    let mut i = 0;
    while table.len() < 500 * 1024 {
        table.push_str(&format!(
            "<tr><td>item {i}</td><td>type {}</td><td>a short note about row {i}</td></tr>",
            i % 7
        ));
        i += 1;
    }
    table.push_str("</tbody></table></body></html>");

    // 6. Смесь строчного форматирования (короткие серии, не достигающие порога слияния).
    let mut inline = String::from("<html><body><article>");
    let mut i = 0;
    while inline.len() < 400 * 1024 {
        inline.push_str(&format!(
            "<p>Para {i}: <b>b1</b> <i>i1</i> <b>b2</b> <code>c1</code> word <strong>s</strong> <em>e</em> tail.</p>"
        ));
        i += 1;
    }
    inline.push_str("</article></body></html>");

    // 7. Крупная «вики»: заголовки, абзацы, сноски, таблицы — около 1 МиБ.
    let mut wiki = String::from("<html><head><title>Wiki</title></head><body>");
    let mut i = 0;
    while wiki.len() < MIB {
        wiki.push_str(&format!(
            "<h2>Heading {i}</h2><p>Lead paragraph {i} with <a href=\"/wiki/T{i}\">a reference</a> and <sup>[{i}]</sup>.</p>\
             <table><tr><th>K</th><th>V</th></tr><tr><td>key{i}</td><td>value {i}</td></tr></table>\
             <ol><li>step one</li><li>step two</li></ol>"
        ));
        i += 1;
    }
    wiki.push_str("</body></html>");

    // 8. Приложение: большие встроенные скрипты (обычный и минифицированный код со
    // сравнениями `i<n`, стрелочными функциями и строками), JSON-LD, SVG с длинными путями,
    // элементы с десятками атрибутов `data-*` (подсчёт атрибутов не должен давать ложного
    // упрощения). Тег-подобный текст скрипта (`<n0; i++) { … }`) подсчёт считает тегом до
    // ближайшего `>`: в обычном коде он есть через строку-другую (`=>`, `>`, строки HTML).
    let mut app = String::from(
        "<!doctype html><html><head><title>App</title>\
         <script type=\"application/ld+json\">{\"@context\": \"https://schema.org\", \"name\": \"x\"}</script>\
         <script>\n",
    );
    for i in 0..3000 {
        app.push_str(&format!(
            "  for (let i = 0; i<n{i}; i++) {{ total = total + values[i] * {i}; label = 'row ' + i; }}\n\
             \x20 const pick{i} = (row) => row.id === {i} ? row : null;\n"
        ));
    }
    app.push_str("</script><script>");
    for i in 0..2000 {
        app.push_str(&format!(
            "function f{i}(a,b){{if(a<b){{return a}}var s=\"<div class='c{i}'>\"+b+\"</div>\";return s}}"
        ));
    }
    app.push_str("</script></head><body>");
    let mut i = 0;
    while app.len() < 1536 * 1024 {
        let data: String = (0..40).map(|k| format!(" data-k{k}=\"v{i}\"")).collect();
        app.push_str(&format!(
            "<div class=\"card\" id=\"c{i}\"{data}><h3>Card {i}</h3><p>Card body text number {i} for the reader.</p>\
             <svg viewBox=\"0 0 24 24\"><path d=\"M12 2L2 7l10 5 10-5-10-5zm0 13l-8-4v6l8 4 8-4v-6l-8 4z\"/></svg></div>\n"
        ));
        i += 1;
    }
    app.push_str("</body></html>");

    vec![
        ("article", article),
        ("docs", docs),
        ("forum", forum),
        ("links", links),
        ("table", table),
        ("inline", inline),
        ("wiki", wiki),
        ("app", app),
    ]
}
