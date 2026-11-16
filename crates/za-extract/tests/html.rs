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
    bounded_within(label, html, Duration::from_secs(30) * time_scale())
}

/// Предел «около секунды в release» (I1 повторного ревью fix-7): в release — 1 с ×
/// `ZA_TEST_TIME_SCALE`, в отладочной сборке (в 10–20 раз медленнее) — общий предел 30 с ×
/// масштаб, только от зависания.
fn release_second() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(30) * time_scale()
    } else {
        Duration::from_secs(1) * time_scale()
    }
}

/// [`bounded`] со своим пределом времени.
fn bounded_within(label: &str, html: String, limit: Duration) -> Markdown {
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

/// Слияние соседних одинаковых строчных элементов. У `htmd` оно квадратично: каждое слияние
/// сдвигает список детей и копирует весь накопленный текст. Серия из r элементов с текстом
/// длины L стоит ≈ L·r²/2 байт копирования (I1 повторного ревью fix-7: L = 40 — 13,3 с в
/// release, больше тайм-аута подпроцесса). Серии сливаются до `htmd`, одним проходом за
/// линейное время, поэтому страница идёт полным путём и быстро, при любой длине текста.
#[test]
fn adjacent_merge_series_are_linear() {
    for len in [1usize, 40, 400] {
        let unit = format!("<b>{}</b>", "y".repeat(len));
        let html = format!("<p>{}</p>", unit.repeat((2 * MIB - 16) / unit.len()));
        let m = bounded_within(&format!("merge L={len}"), html, release_second());
        assert!(!m.simplified, "L={len}: серия должна идти полным путём");
        // Одна серия — один жирный отрезок, текст целиком.
        assert_eq!(m.text.matches("**").count(), 2, "L={len}");
        assert!(m.text.starts_with("**yyy") && m.text.ends_with("yyy**"));
    }
    // Много серий по 5000 `<b>x</b>` через пробел (прежний вид класса).
    let unit = format!("{} ", "<b>x</b>".repeat(5000));
    let m = bounded_within("merge runs", rep(&unit, 2 * MIB), release_second());
    assert!(!m.simplified);
    assert!(m.text.starts_with(&format!("**{}**", "x".repeat(5000))));
}

// --- Страж: глубина вставки и там, где построитель переносит элементы (m1 ревью fix-7) ---

/// Глубина узла, перенесённого из таблицы (foster parenting, `append_before_sibling`), и
/// узла в содержимом `template` считается от настоящего места в дереве, а не от нуля:
/// поддерево, начатое внутри глубокой таблицы, превышает порог глубины онлайн.
#[test]
fn guard_counts_depth_of_fostered_and_template_nodes() {
    use za_extract::guard::{MAX_DEPTH, guarded_parse};
    let outer = MAX_DEPTH - 60;
    let inner = 100;
    // Перенос из таблицы: `<div>` внутри `<table>` вставляется перед таблицей.
    let fostered = format!(
        "<html><body>{}<table>{}x</table></body></html>",
        "<div>".repeat(outer),
        "<div>".repeat(inner)
    );
    assert_eq!(
        guarded_parse(&fostered).err(),
        Some("depth"),
        "foster parenting"
    );
    // Содержимое `template` — фрагмент вне дерева, но открыто внутри шаблона.
    let template = format!(
        "<html><body>{}<template>{}x</template></body></html>",
        "<div>".repeat(outer),
        "<div>".repeat(inner)
    );
    assert_eq!(guarded_parse(&template).err(), Some("depth"), "template");
    // Под порогом — без отказа.
    let shallow = format!(
        "<html><body>{}<table>{}x</table></body></html>",
        "<div>".repeat(20),
        "<div>".repeat(20)
    );
    assert!(guarded_parse(&shallow).is_ok());
}

/// Перенос из таблицы потоком элементов: `RcDom` на каждый перенос ищет таблицу среди детей
/// её родителя линейным проходом (`get_parent_and_index`), и 2 МиБ `<table><i></i>…` до
/// исправления разбирались ≈ 11 с в release. Страж считает длину этих поисков → `simplified`.
#[test]
fn foster_parenting_flood_is_simplified() {
    for unit in ["<i></i>", "<b>x</b>", "<div></div>"] {
        let html = format!(
            "<html><body><div><table>{}</table></div></body></html>",
            rep(unit, 2 * MIB - 64)
        );
        let m = bounded_within(&format!("foster {unit}"), html, release_second());
        assert!(m.simplified, "{unit}: перенос из таблицы не замечен");
    }
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

/// Ложное упрощение (Ruling прохода fix-8, сомнение 3): в скрипте `i<n` и дальше больше 1024
/// «слов» без `>`. Для токенизатора это текст скрипта, а не тег. Подсчёт атрибутов пропускает
/// содержимое raw-text элементов так же, как токенизатор, поэтому страница идёт полным путём.
#[test]
fn tag_like_text_in_raw_text_is_not_simplified() {
    let mut code = String::from("if (i<n) {");
    for k in 0..1500 {
        code.push_str(&format!(" x{k} = {k};"));
    }
    code.push_str(" }");
    // Встроенный массив данных графика после цикла `i<n` (замечание ревью fix-8).
    let numbers: Vec<String> = (0..5000).map(|k| (k * 7 % 1000).to_string()).collect();
    let chart = format!(
        "for (var i = 0; i<n; i++) {{ draw(i); }} var series = [{}];",
        numbers.join(", ")
    );
    let html =
        format!("<html><head><script>{chart}</script></head><body><p>chart page</p></body></html>");
    let m = bounded("chart data", html);
    assert!(!m.simplified, "массив данных графика: ложное упрощение");
    assert!(m.text.contains("chart page"));
    for (open, close) in [
        ("<script>", "</script>"),
        ("<SCRIPT type=module>", "</Script >"),
        ("<style>", "</style>"),
        ("<textarea>", "</textarea>"),
        ("<title>", "</title>"),
        ("<noscript>", "</noscript>"),
    ] {
        let html = format!(
            "<html><head>{open}{code}{close}</head><body><p>after raw text</p></body></html>"
        );
        let m = bounded(open, html);
        assert!(!m.simplified, "{open}: ложное упрощение");
        assert!(m.text.contains("after raw text"), "{open}: {:?}", m.text);
    }
}

/// Пропуск raw text не прячет настоящий тег: подсчёт пропускает только там, где токенизатор
/// точно в raw text. Каждый вход несёт тег с 2000 атрибутами там, где его разберёт токенизатор.
#[test]
fn raw_text_skip_does_not_hide_attribute_flood() {
    let attrs: String = (0..2000).map(|i| format!(" a{i}")).collect();
    let flood = format!("<div{attrs}>tail</div>");
    let cases = [
        // `<script>` внутри `<noscript>` — текст noscript, после `</noscript>` — настоящий тег.
        ("noscript", format!("<noscript><script></noscript>{flood}")),
        // Конец скрипта, затем тег.
        (
            "after script",
            format!("<script>if (i<n) x</script>{flood}"),
        ),
        ("upper end tag", format!("<SCRIPT>a</SCRIPT\t>{flood}")),
        // `</scriptx` — не конец скрипта; тег — после настоящего конца.
        (
            "not an end tag",
            format!("<script>a</scriptx b</script/>{flood}"),
        ),
        // Атрибуты у закрывающего тега токенизатор тоже собирает.
        ("end tag attrs", format!("<script>x</script{attrs}>")),
        ("rcdata end", format!("<title>a</title>{flood}")),
        // В SVG `<script>` — обычный элемент: его содержимое разбирается как теги.
        ("svg script", format!("<svg><script>{flood}</script></svg>")),
        ("math", format!("<math><style>{flood}</style></math>")),
        // `<!--<script>` внутри скрипта: двойное экранирование, первый `</script>` скрипт не
        // закрывает, тег — после второго.
        (
            "double escape",
            format!("<script><!--<script></script><style></script>{flood}</style>"),
        ),
        // Комментарий `<!--!>` не закрыт: `<script>` в нём — текст, тег — после `-->`.
        ("comment", format!("<!--!><script>-->{flood}")),
        // В шаблоне после `<col>` построитель пропускает `<script>`: raw text не начинается.
        (
            "template col",
            format!("<template><col><script>{flood}</template>"),
        ),
        ("frameset", format!("<frameset><script>{flood}")),
    ];
    for (name, html) in cases {
        let m = bounded(name, html);
        assert!(m.simplified, "{name}: тег с 2000 атрибутами не замечен");
    }
}

// --- Линейное извлечение: стек открытых пропускаемых элементов (I2 повторного ревью fix-7) ---

/// Пустые элементы, `option` и `head` без закрывающего тега не обрывают текст: счётчик
/// пропуска застревал на них, и весь текст после пропадал.
#[test]
fn linear_extract_closes_skipped_elements() {
    let cases = [
        ("<p>before</p><input name=q><p>after</p>", "before\nafter"),
        ("<p>before</p><input name=q/><p>after</p>", "before\nafter"),
        ("<p>before</p><embed src=/e><p>after</p>", "before\nafter"),
        (
            "<p>a</p><select><option>one<option>two</select><p>after select</p>",
            "a\nafter select",
        ),
        (
            "<p>a</p><datalist><option value=x>one<option>two</datalist><p>after list</p>",
            "a\nafter list",
        ),
        (
            "<html><head><title>T</title><body><p>body text</p>",
            "body text",
        ),
        // `<body>` закрывает `head`, поэтому лишний `</head>` в теле не снимает `<object>`.
        (
            "<head><title>T</title><body><object>fallback</head>inside</object><p>after</p>",
            "after",
        ),
        // Первый элемент тела закрывает `head` и без `<body>`; содержимое `head` не видно.
        (
            "<head><meta charset=utf-8><style>p{}</style><div>body text</div>",
            "body text",
        ),
        // Текст в `head` тоже переходит в тело (как у построителя дерева).
        (
            "<head><title>T</title>Loose text<p>more</p>",
            "Loose text\nmore",
        ),
        // Самозакрытый `<button/>` в стек не кладётся.
        ("<p>a</p><button/><p>b</p>", "a\nb"),
        // Закрывающий без пары не трогает стек.
        (
            "<p>a</p><select></button><option>x</option></select><p>b</p>",
            "a\nb",
        ),
        // Ячейки таблицы разделены (m3).
        ("<table><tr><td>a</td><td>b</td></tr></table>", "a\nb"),
    ];
    for (html, want) in cases {
        let m = za_extract::linear::linear_extract(html);
        assert_eq!(m.text, want, "{html}");
        assert!(m.simplified);
    }
    let m = za_extract::linear::linear_extract("<html><head><title>T</title><body><p>x</p>");
    assert_eq!(m.title.as_deref(), Some("T"));
    // `<title>` внутри SVG заголовком страницы не считается (как на полном пути, m3).
    let m = za_extract::linear::linear_extract("<svg><title>svg t</title></svg><p>x</p>");
    assert_eq!((m.title, m.text.as_str()), (None, "x"));
}

/// Страницы вида Википедии и MDN (шапка с `<input>`, `<button>`, `<select>` без `</option>`,
/// значки SVG, шаблоны теневого DOM) и обычные страницы корпуса: линейное извлечение
/// сохраняет весь текст статьи и не меньше половины слов полного пути.
#[test]
fn linear_extract_keeps_page_text() {
    for (name, html, markers) in [
        ("wiki-like", wiki_like(), 200usize),
        ("mdn-like", mdn_like(), 150),
    ] {
        let m = za_extract::linear::linear_extract(&html);
        for i in 0..markers {
            assert!(
                m.text.contains(&format!("marker{i} ")),
                "{name}: нет абзаца {i} ({} байт текста)",
                m.text.len()
            );
        }
        for gone in [
            "move to sidebar",
            "Search Wikipedia",
            "Español",
            "Toggle menu",
            "RLCONF",
        ] {
            assert!(!m.text.contains(gone), "{name}: {gone} в тексте");
        }
        assert!(m.title.is_some(), "{name}: нет заголовка");
    }
    let mut pages = realistic_pages();
    pages.push(("wiki-like", wiki_like()));
    pages.push(("mdn-like", mdn_like()));
    for (name, html) in pages {
        let full = words(&without_link_targets(&html_to_markdown(&html).text));
        let linear = words(&za_extract::linear::linear_extract(&html).text);
        println!("words {name}: linear {linear} / full {full}");
        assert!(2 * linear >= full, "{name}: {linear} слов из {full}");
    }
}

/// Число слов (буквенно-цифровых последовательностей) в тексте.
fn words(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .count()
}

/// Markdown без адресов ссылок и картинок (`](…)`): линейное извлечение ссылок не даёт,
/// сравнивать нужно только текст.
fn without_link_targets(md: &str) -> String {
    let mut prose = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(at) = rest.find("](") {
        prose.push_str(&rest[..=at]);
        rest = rest[at..].find(')').map_or("", |end| &rest[at + end + 1..]);
    }
    prose.push_str(rest);
    prose
}

/// Страница вида статьи Википедии (Vector 2022): шапка с флажками-переключателями меню,
/// кнопками и формой поиска, сноски, таблицы, `<noscript>` с картинкой, скрипты в конце.
fn wiki_like() -> String {
    let mut html = String::from(
        "<!DOCTYPE html>\n<html class=\"client-nojs\" lang=\"en\" dir=\"ltr\">\n<head>\n\
         <meta charset=\"UTF-8\">\n<title>Rust (programming language) - Wikipedia</title>\n\
         <script>document.documentElement.className=\"client-js\";RLCONF={\"wgBreakFrames\":false,\"wgPageName\":\"Rust\"};</script>\n\
         <link rel=\"stylesheet\" href=\"/w/load.php?modules=site.styles&amp;only=styles\">\n\
         <meta name=\"viewport\" content=\"width=1120\">\n</head>\n\
         <body class=\"skin-vector-2022 mediawiki ltr\">\n\
         <a class=\"mw-jump-link\" href=\"#bodyContent\">Jump to content</a>\n\
         <div class=\"vector-header-container\"><header class=\"vector-header mw-header\">\
         <div class=\"vector-dropdown\"><input type=\"checkbox\" id=\"vector-main-menu-dropdown-checkbox\" role=\"button\" aria-haspopup=\"true\" class=\"vector-dropdown-checkbox\" aria-label=\"Main menu\">\
         <label for=\"vector-main-menu-dropdown-checkbox\" class=\"vector-dropdown-label\"><span class=\"vector-icon\"></span><span class=\"vector-dropdown-label-text\">Main menu</span></label>\
         <div class=\"vector-dropdown-content\"><div class=\"vector-pinnable-header\"><div class=\"vector-pinnable-header-label\">Main menu</div>\
         <button class=\"vector-pinnable-header-toggle-button vector-pinnable-header-pin-button\">move to sidebar</button>\
         <button class=\"vector-pinnable-header-toggle-button vector-pinnable-header-unpin-button\">hide</button></div>\
         <ul><li><a href=\"/wiki/Main_Page\">Main page</a></li><li><a href=\"/wiki/Contents\">Contents</a></li></ul></div></div>\
         <form action=\"/w/index.php\" id=\"searchform\"><div class=\"cdx-search-input\">\
         <input class=\"cdx-text-input__input\" type=\"search\" name=\"search\" placeholder=\"Search Wikipedia\" title=\"Search Wikipedia [f]\" accesskey=\"f\" id=\"searchInput\">\
         <input type=\"hidden\" name=\"title\" value=\"Special:Search\"><button class=\"cdx-button\">Search</button></div></form>\
         <input type=\"checkbox\" id=\"vector-appearance-dropdown-checkbox\" role=\"button\" aria-label=\"Appearance\">\
         <input type=\"checkbox\" id=\"vector-user-links-dropdown-checkbox\" role=\"button\" aria-label=\"Personal tools\">\
         </header></div>\n<div class=\"mw-page-container\"><main id=\"content\" class=\"mw-body\">\
         <h1 id=\"firstHeading\"><span class=\"mw-page-title-main\">Rust (programming language)</span></h1>\
         <input type=\"checkbox\" id=\"vector-page-titlebar-toc-checkbox\" class=\"vector-menu-checkbox\" role=\"button\">\
         <div id=\"bodyContent\"><div id=\"mw-content-text\" class=\"mw-body-content\"><div class=\"mw-parser-output\" lang=\"en\" dir=\"ltr\">\n",
    );
    for i in 0..200 {
        html.push_str(&format!(
            "<h2 id=\"S{i}\">Section {i}</h2><p><b>Rust</b> article paragraph marker{i} about \
             <a href=\"/wiki/Memory_safety\" title=\"Memory safety\">memory safety</a> and \
             <a href=\"/wiki/Concurrency\">concurrency</a> in practice.<sup id=\"cite_ref-{i}\" class=\"reference\">\
             <a href=\"#cite_note-{i}\"><span class=\"cite-bracket\">[</span>{i}<span class=\"cite-bracket\">]</span></a></sup></p>\n\
             <table class=\"wikitable\"><tbody><tr><th>Key {i}</th><td>value {i}</td></tr></tbody></table>\n"
        ));
    }
    html.push_str(
        "</div></div></div><noscript><img src=\"https://login.wikimedia.org/wiki/Special:CentralAutoLogin/start?type=1x1\" alt=\"\" width=\"1\" height=\"1\"></noscript>\
         </main></div>\n<script>(RLQ=window.RLQ||[]).push(function(){mw.config.set({\"wgHostname\":\"mw-web\"});});</script>\n</body>\n</html>\n",
    );
    html
}

/// Страница вида справочника MDN: значки SVG в кнопках, шаблоны теневого DOM в шапке, поиск,
/// выбор языка `<select>` с `<option>` без закрывающих тегов, статья с таблицами.
fn mdn_like() -> String {
    let mut html = String::from(
        "<!doctype html><html lang=\"en-US\" data-theme=\"light dark\"><head><meta charset=\"utf-8\">\
         <title>HTML elements reference - HTML | MDN</title>\
         <script>window.__mdn = {\"lang\": \"en-US\"}; if (a < b) { start(); }</script>\
         <style>:root { --text: #1b1b1b; } .skip-link { position: absolute; }</style></head>\
         <body class=\"page-layout\"><a class=\"skip-link\" href=\"#content\">Skip to main content</a>\
         <header class=\"top-navigation\"><nav>\
         <button type=\"button\" class=\"button action has-icon\" aria-label=\"Toggle menu\"><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\"><path d=\"M3 12h18M3 6h18M3 18h18\"/></svg><span class=\"visually-hidden\">Toggle menu</span></button>",
    );
    for k in 0..12 {
        html.push_str(&format!(
            "<mdn-dropdown><template shadowroot=\"open\" shadowrootmode=\"open\"><button part=\"button\">Toggle menu {k}</button>\
             <svg width=\"16\" height=\"16\"><path d=\"M1 1l7 7 7-7\"/></svg></template><a href=\"/en-US/docs/Web/{k}\">Docs {k}</a></mdn-dropdown>"
        ));
    }
    html.push_str(
        "<form role=\"search\" action=\"/en-US/search\"><input type=\"search\" name=\"q\" placeholder=\"Search MDN\" autocomplete=\"off\">\
         <button type=\"submit\">Search</button></form>\
         <select id=\"language-selector\" name=\"language\"><option value=\"en-US\" selected>English (US)\
         <option value=\"es\">Español<option value=\"fr\">Français<option value=\"ja\">日本語</select>\
         </nav></header><main id=\"content\" class=\"main-content\"><article class=\"main-page-content\" lang=\"en-US\">\
         <header><h1>HTML elements reference</h1></header>",
    );
    for i in 0..150 {
        html.push_str(&format!(
            "<section aria-labelledby=\"s{i}\"><h2 id=\"s{i}\"><a href=\"#s{i}\">Section {i}</a></h2>\
             <div class=\"section-content\"><p>Reference paragraph marker{i} explains the \
             <a href=\"/en-US/docs/Web/HTML/Element/div\"><code>&lt;div&gt;</code></a> element and its use.</p>\
             <table class=\"standard-table\"><thead><tr><th scope=\"col\">Element</th><th scope=\"col\">Description</th></tr></thead>\
             <tbody><tr><td><a href=\"/en-US/docs/Web/HTML/Element/p\"><code>&lt;p&gt;</code></a></td>\
             <td>Represents a paragraph number {i}.</td></tr></tbody></table></div></section>"
        ));
    }
    html.push_str(
        "</article></main><footer class=\"page-footer\"><p>Your blueprint for a better internet.</p></footer>\
         <script>document.querySelectorAll(\"mdn-dropdown\").forEach(function (d) { d.upgrade(); });</script></body></html>",
    );
    html
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

/// Плоская страница (m1 ревью fix-9): 100 000 строк `line<br>` прямо в `body` (200 тыс.
/// детей), в конце таблица на 2000 строк, после каждой строки лишний `&nbsp;`. Каждый перенос
/// текста из таблицы ищет её среди детей `body` (счётчик поиска ≈ 4·10^8), а полный разбор
/// без стража идёт ≈ 130 мс. Такая страница — обычная кривая разметка, не атака: полный путь.
#[test]
fn flat_page_with_fostered_text_is_not_simplified() {
    let mut html = String::from("<html><body>");
    html.push_str(&"line<br>".repeat(100_000));
    html.push_str("<table>");
    for i in 0..2000 {
        html.push_str(&format!("<tr><td>cell {i}</td></tr>&nbsp;"));
    }
    html.push_str("</table><p>after table</p></body></html>");
    let m = bounded_within("flat fostered", html, release_second());
    assert!(!m.simplified, "ложное упрощение плоской страницы");
    assert!(m.text.contains("cell 1999") && m.text.contains("after table"));
}

/// Худший случай полного пути у класса «поток переноса»: счётчик поиска чуть ниже порога
/// `MAX_SCAN` (200 тыс. детей `body` × 4900 переносов ≈ 9,8·10^8). Страница идёт полным
/// путём и укладывается в цель ≈ 1 с в release.
#[test]
fn scan_just_below_threshold_is_fast() {
    use za_extract::guard::MAX_SCAN;
    let children = 200_000u64;
    let moves = MAX_SCAN / children * 98 / 100;
    let mut html = String::from("<html><body>");
    html.push_str(&"line<br>".repeat(children as usize / 2));
    html.push_str("<table>");
    for i in 0..moves {
        html.push_str(&format!("<tr><td>c{i}</td></tr>x"));
    }
    html.push_str("</table></body></html>");
    let m = bounded_within("scan below threshold", html, release_second());
    assert!(!m.simplified, "ниже порога — полный путь");
}

/// Строки «ключ — значение» (`<tr><th>…</th><td>…</td></tr>`, карточки Википедии): `htmd`
/// 0.5.6 берёт первую строку с `th` заголовком таблицы и теряет её `td`, а в следующих строках
/// читает только `td` и теряет `th`. После подготовки DOM ни одна ячейка не теряется.
#[test]
fn key_value_rows_keep_th_and_td() {
    let html = "<table class=\"infobox\"><tbody>\
        <tr><th colspan=\"2\">Rust</th></tr>\
        <tr><th scope=\"row\">Paradigms</th><td>Concurrent, functional</td></tr>\
        <tr><th scope=\"row\">Designed by</th><td>Graydon Hoare</td></tr>\
        <tr><th colspan=\"2\">Implementations</th></tr>\
        <tr><th scope=\"row\">License</th><td>MIT and Apache 2.0</td></tr>\
        </tbody></table><p>after</p>";
    let m = html_to_markdown(html);
    assert!(!m.simplified);
    for want in [
        "Rust",
        "Paradigms",
        "Concurrent, functional",
        "Designed by",
        "Graydon Hoare",
        "Implementations",
        "License",
        "MIT and Apache 2.0",
        "after",
    ] {
        assert!(m.text.contains(want), "нет {want:?}: {:?}", m.text);
    }
    // Ключ и значение — в одной строке таблицы, порядок строк сохранён.
    let line = |s: &str| m.text.lines().position(|l| l.contains(s));
    assert_eq!(
        line("Paradigms"),
        line("Concurrent, functional"),
        "{}",
        m.text
    );
    assert_eq!(line("License"), line("MIT and Apache 2.0"), "{}", m.text);
    assert!(line("Designed by") < line("Implementations"), "{}", m.text);
    assert!(line("Implementations") < line("License"), "{}", m.text);
}

/// Матрица: пустая угловая ячейка `td` в строке заголовка и `th` строки в теле. Все ячейки
/// на месте, столбцы не сдвинуты: строка `r1` — в одной строке со своими значениями.
#[test]
fn matrix_table_keeps_corner_and_row_headers() {
    let html = "<table><thead><tr><td></td><th>A</th><th>B</th></tr></thead>\
        <tbody><tr><th>r1</th><td>one</td><td>two</td></tr>\
        <tr><th>r2</th><td>three</td><td>four</td></tr></tbody></table>";
    let m = html_to_markdown(html);
    let line = |s: &str| m.text.lines().position(|l| l.contains(s));
    for want in ["A", "B", "r1", "one", "two", "r2", "three", "four"] {
        assert!(m.text.contains(want), "нет {want:?}: {:?}", m.text);
    }
    assert_eq!(line("r1"), line("two"), "{}", m.text);
    assert_eq!(line("r2"), line("four"), "{}", m.text);
    // Заголовок — три столбца (пустой угол сохранён), как у строк тела.
    let header = m.text.lines().find(|l| l.contains('A')).unwrap_or_default();
    let row = m
        .text
        .lines()
        .find(|l| l.contains("r1"))
        .unwrap_or_default();
    assert_eq!(
        header.matches('|').count(),
        row.matches('|').count(),
        "{}",
        m.text
    );
}

/// Обычная таблица с заголовком (`thead` из `th`, тело из `td`) остаётся таблицей Markdown с
/// строкой заголовка и разделителем: подготовка DOM её не трогает.
#[test]
fn plain_header_table_is_unchanged() {
    let html = "<table><thead><tr><th>Name</th><th>Kind</th></tr></thead>\
        <tbody><tr><td>alpha</td><td>first</td></tr><tr><td>beta</td><td>second</td></tr></tbody></table>";
    let m = html_to_markdown(html);
    let lines: Vec<&str> = m.text.lines().collect();
    assert_eq!(lines.len(), 4, "{}", m.text);
    assert!(lines[0].contains("Name") && lines[0].contains("Kind"));
    assert!(lines[1].contains("---"));
    assert!(lines[2].contains("alpha") && lines[3].contains("second"));
    // Первая строка тела из одних `th` без `thead` — тоже заголовок, как у `htmd`.
    let html =
        "<table><tr><th>Name</th><th>Kind</th></tr><tr><td>alpha</td><td>first</td></tr></table>";
    let m = html_to_markdown(html);
    let lines: Vec<&str> = m.text.lines().collect();
    assert_eq!(lines.len(), 3, "{}", m.text);
    assert!(lines[1].contains("---") && lines[2].contains("alpha"));
}

/// Позиция первой строки текста, содержащей `s`.
fn line_of(text: &str, s: &str) -> Option<usize> {
    text.lines().position(|l| l.contains(s))
}

/// m6 ревью задачи 10. Пустой `thead`: `htmd` считает, что заголовок уже есть, и строку из
/// одних `th` в теле не берёт ни заголовком, ни данными — она пропадала целиком. m-2
/// повторного ревью fix-10: пустой `thead` до заголовка становится телом, и строка `th` —
/// заголовком таблицы GFM, с разделителем.
#[test]
fn empty_thead_keeps_header_row_of_body() {
    let html = "<table><thead></thead><tbody><tr><th>Alpha</th><th>Beta</th></tr>\
        <tr><td>one</td><td>two</td></tr></tbody></table>";
    let m = html_to_markdown(html);
    for want in ["Alpha", "Beta", "one", "two"] {
        assert!(m.text.contains(want), "нет {want:?}: {:?}", m.text);
    }
    assert!(
        line_of(&m.text, "Alpha") < line_of(&m.text, "one"),
        "{}",
        m.text
    );
    let lines = table_lines(&m.text);
    assert_eq!(lines.len(), 3, "{}", m.text);
    assert!(lines[0].contains("Alpha") && lines[0].contains("Beta"));
    assert!(lines[1].contains("---"), "нет разделителя: {}", m.text);
    let html = "<table><thead><tr></tr></thead><tbody><tr><th>Alpha</th></tr>\
        <tr><td>one</td></tr></tbody></table>";
    let lines = table_lines(&html_to_markdown(html).text).len();
    assert_eq!(lines, 3, "пустая строка thead");
}

/// m-4 повторного ревью fix-10b (побочный эффект m-2): пустой `thead` перед телом без строки
/// из одних `th` остаётся заголовком таблицы, и `htmd` печатает строки таблицы `| 1 | 2 |`, а
/// не ячейки отдельными абзацами. То же со смешанной первой строкой.
#[test]
fn empty_thead_before_data_rows_keeps_table_rows() {
    let html = "<table><thead></thead><tbody><tr><td>1</td><td>2</td></tr>\
        <tr><td>3</td><td>4</td></tr></tbody></table>";
    let m = html_to_markdown(html);
    assert!(!m.text.contains("1\n\n2"), "ячейки абзацами: {:?}", m.text);
    let lines = table_lines(&m.text);
    assert!(
        lines.iter().any(|l| l.contains('1') && l.contains('2')),
        "{:?}",
        m.text
    );
    assert!(
        lines.iter().any(|l| l.contains('3') && l.contains('4')),
        "{:?}",
        m.text
    );
    let html = "<table><thead><tr></tr></thead><tbody><tr><th>Key</th><td>value</td></tr>\
        <tr><td>a</td><td>b</td></tr></tbody></table>";
    let m = html_to_markdown(html);
    let lines = table_lines(&m.text);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Key") && l.contains("value")),
        "{:?}",
        m.text
    );
}

/// m6 ревью задачи 10. Многострочный `thead`: `htmd` читает только первую строку, остальные
/// пропадали. Первая строка — заголовок, следующие — строки данных перед телом.
#[test]
fn multi_row_thead_keeps_every_row() {
    let html = "<table><thead><tr><th>Name</th><th>Size</th></tr>\
        <tr><th>first</th><th>bytes</th></tr></thead>\
        <tbody><tr><td>alpha</td><td>1</td></tr></tbody></table>";
    let m = html_to_markdown(html);
    let lines = table_lines(&m.text);
    assert_eq!(lines.len(), 4, "{}", m.text);
    assert!(lines[0].contains("Name") && lines[0].contains("Size"));
    assert!(lines[2].contains("first") && lines[2].contains("bytes"));
    assert!(lines[3].contains("alpha"));
}

/// m6 ревью задачи 10. Второй `thead` (после тела) или пустой `thead` после заголовка
/// заменял заголовок у `htmd`: прежний пропадал. Заголовок — первый, остальные строки —
/// данные в порядке документа.
#[test]
fn later_thead_does_not_replace_header() {
    let html = "<table><thead><tr><th>Head</th></tr></thead><tbody><tr><td>value</td></tr></tbody>\
        <thead><tr><th>Later</th></tr></thead></table>";
    let m = html_to_markdown(html);
    let lines = table_lines(&m.text);
    assert_eq!(lines.len(), 4, "{}", m.text);
    assert!(lines[0].contains("Head"), "{}", m.text);
    assert!(lines[2].contains("value") && lines[3].contains("Later"));
    let html = "<table><thead><tr><th>Head</th></tr></thead><tbody><tr><td>value</td></tr></tbody>\
        <thead></thead></table>";
    let m = html_to_markdown(html);
    assert!(table_lines(&m.text)[0].contains("Head"), "{}", m.text);
}

/// Страница вида Википедии: значения таблиц «ключ — значение» не теряются.
#[test]
fn wiki_like_keeps_table_values() {
    let m = bounded("wiki-like", wiki_like());
    assert!(!m.simplified);
    for i in [0, 7, 199] {
        assert!(m.text.contains(&format!("Key {i}")), "нет Key {i}");
        assert!(m.text.contains(&format!("value {i}")), "нет value {i}");
    }
}

// --- Выравнивание таблиц (I1 ревью задачи 10) ---

/// Предел «около 0,5 с в release» для таблиц (I1 ревью задачи 10): в release — 500 мс ×
/// `ZA_TEST_TIME_SCALE`, в отладочной сборке — общий предел 30 с × масштаб, только от
/// зависания.
fn release_half_second() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(30) * time_scale()
    } else {
        Duration::from_millis(500) * time_scale()
    }
}

/// Строки текста, начинающиеся с `|` (строки таблиц Markdown).
fn table_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with('|')).collect()
}

/// Таблица из ревью задачи 10 (I1): заголовок шириной 100 000 символов и 2000 строк по одной
/// короткой ячейке, 138 КБ входа. `htmd` дополняет каждую строку пробелами до ширины столбца:
/// 200 МБ вывода за 7 с в отладочной сборке. Надбавка выравнивания больше бюджета, таблица
/// идёт без дополнения: те же ячейки, вывод порядка входа, ≤ 0,5 с в release.
#[test]
fn wide_column_table_is_not_padded() {
    use za_extract::TABLE_ALIGN_BUDGET;
    let mut html = format!(
        "<table><thead><tr><th>{}</th></tr></thead><tbody>",
        "a".repeat(100_000)
    );
    html.push_str(&"<tr><td>x</td></tr>".repeat(2000));
    html.push_str("</tbody></table><p>after</p>");
    assert!(
        html.len() > 130_000 && html.len() < 140_000,
        "{}",
        html.len()
    );
    let m = bounded_within("wide column", html, release_half_second());
    assert!(!m.simplified);
    assert!(
        (m.text.len() as u64) < TABLE_ALIGN_BUDGET,
        "{} байт",
        m.text.len()
    );
    assert!(m.text.len() < 200_000, "{} байт", m.text.len());
    let lines = table_lines(&m.text);
    // Заголовок, разделитель и 2000 строк данных — таблица Markdown сохранилась.
    assert_eq!(lines.len(), 2002, "{}", &m.text[..200]);
    assert!(lines[0].contains(&"a".repeat(100_000)));
    assert_eq!(lines[1], "| --- |");
    assert!(lines[2..].iter().all(|l| *l == "| x |"), "{}", lines[2]);
    assert!(m.text.ends_with("after"));
}

/// Худший случай I1 в пределах 2 МиБ: ячейка шириной 1 МиБ и 55 000 строк (около 55 ГБ
/// выровненного вывода). Без дополнения — около 1,2 МБ за время порядка разбора.
#[test]
fn two_mib_wide_table_is_bounded() {
    let mut html = format!(
        "<table><thead><tr><th>{}</th></tr></thead><tbody>",
        "w".repeat(MIB)
    );
    while html.len() < HTML_MAX_BYTES - 64 {
        html.push_str("<tr><td>y</td></tr>");
    }
    html.push_str("</tbody></table>");
    let m = bounded_within("2 MiB wide", html, release_second());
    assert!(!m.simplified);
    assert!(m.text.len() < 2 * MIB, "{} байт", m.text.len());
    assert!(table_lines(&m.text).len() > 50_000);
}

/// Бюджет выравнивания общий на страницу: 200 таблиц, у каждой надбавка около 1 МБ
/// (ширина 3000 × 350 строк, 10 КБ входа), без общего бюджета дали бы 200 МБ. Выровнены
/// только первые, пока надбавка укладывается в бюджет; остальные — без дополнения.
#[test]
fn align_budget_is_per_page() {
    use za_extract::TABLE_ALIGN_BUDGET;
    let mut html = String::from("<html><body>");
    for i in 0..200 {
        html.push_str(&format!(
            "<table><tr><th>t{i} {}</th></tr>{}</table>",
            "h".repeat(3000),
            "<tr><td>z</td></tr>".repeat(350)
        ));
    }
    html.push_str("</body></html>");
    let input = html.len();
    let m = bounded_within("200 tables", html, release_second());
    assert!(!m.simplified);
    assert!(
        (m.text.len() as u64) < TABLE_ALIGN_BUDGET + input as u64,
        "{} байт при входе {input}",
        m.text.len()
    );
    // Первая таблица выровнена, последняя — нет.
    let padded = table_lines(&m.text)
        .iter()
        .filter(|l| l.starts_with("| z ") && l.len() > 3000)
        .count();
    assert_eq!(padded, 350, "первая таблица не выровнена");
    assert!(m.text.contains("t199"), "нет последней таблицы");
    let tail = &m.text[m.text.find("t199").unwrap_or(0)..];
    assert!(tail.contains("\n| z |\n"), "последняя таблица выровнена");
}

/// Недостающие ячейки `htmd` тоже дополняет: одна строка из 40 000 `th` и 40 000 строк по
/// одной `td` дали бы 40 000 × 40 000 пустых ячеек (6 ГБ). Без дополнения строки короче —
/// вывод порядка входа, а подсчёт надбавки идёт по ячейкам, а не по строкам × столбцы.
#[test]
fn missing_cells_are_not_padded() {
    let mut html = String::from("<table><thead><tr>");
    html.push_str(&"<th>h</th>".repeat(40_000));
    html.push_str("</tr></thead><tbody>");
    html.push_str(&"<tr><td>q</td></tr>".repeat(40_000));
    html.push_str("</tbody></table>");
    let input = html.len();
    let m = bounded_within("missing cells", html, release_second());
    assert!(!m.simplified);
    assert!(
        m.text.len() < input,
        "{} байт при входе {input}",
        m.text.len()
    );
    let lines = table_lines(&m.text);
    assert_eq!(lines.len(), 40_002);
    assert_eq!(lines[2], "| q |");
}

/// Ширину ячейки задаёт её Markdown, а не текст HTML: ссылка с адресом в 100 000 символов и
/// коротким текстом занимает в выводе весь адрес. Оценка по тексту DOM её бы пропустила.
#[test]
fn long_link_target_counts_in_column_width() {
    let mut html = format!(
        "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td><a href=\"/{}\">x</a></td></tr>",
        "p".repeat(100_000)
    );
    html.push_str(&"<tr><td>y</td></tr>".repeat(2000));
    html.push_str("</tbody></table>");
    let m = bounded_within("long link", html, release_half_second());
    assert!(m.text.len() < 200_000, "{} байт", m.text.len());
    assert!(m.text.contains(&format!("[x](/{})", "p".repeat(100_000))));
    assert_eq!(table_lines(&m.text).len(), 2003);
}

/// `htmd` решает, выравнивать ли таблицу, по любому потомку с именем `th` или `thead` в любом
/// пространстве имён (`has_explicit_headers`). `th` внутри `<math>` в ячейке делает таблицу
/// «с заголовками», хотя ячеек `th` в ней нет. Решение о дополнении такой таблицы — то же:
/// без дополнения.
#[test]
fn foreign_th_does_not_bring_back_padding() {
    let mut html = format!(
        "<table><tr><td>{}<math><th>m</th></math></td></tr>",
        "a".repeat(100_000)
    );
    html.push_str(&"<tr><td>x</td></tr>".repeat(2000));
    html.push_str("</table>");
    let m = bounded_within("math th", html, release_half_second());
    assert!(m.text.len() < 200_000, "{} байт", m.text.len());
    assert!(
        m.text.contains("\n| x |\n"),
        "{}",
        &m.text[m.text.len() - 100..]
    );
}

// --- Префиксы строк цитат и списков (I-1 повторного ревью fix-10) ---

/// `depth` уровней вложенности: открывающие теги `open`, затем `inner`, затем закрывающие.
fn nested(open: &str, close: &str, depth: usize, inner: &str) -> String {
    let mut html = String::with_capacity(inner.len() + depth * (open.len() + close.len()));
    html.push_str(&open.repeat(depth));
    html.push_str(inner);
    html.push_str(&close.repeat(depth));
    html
}

/// Случай повторного ревью fix-10 (I-1): 20 000 строк `pre` под 120 уровнями цитат, 43 КБ
/// входа. `htmd` ставит `> ` перед каждой строкой на каждом уровне: 4,8 МБ текста, больше
/// предела вывода подпроцесса, и страница целиком падала с `extractor output too large`.
/// Оценка вывода больше бюджета — линейное извлечение, текст на месте.
#[test]
fn quote_prefixes_over_budget_are_simplified() {
    let html = nested(
        "<blockquote>",
        "</blockquote>",
        120,
        &format!("<pre>{}</pre>", "x\n".repeat(20_000)),
    );
    assert!(html.len() < 45_000, "{}", html.len());
    let m = bounded_within("quote prefixes", html, release_second());
    assert!(m.simplified);
    assert!(m.text.len() < 100_000, "{} байт", m.text.len());
    assert!(m.text.contains('x'));
}

/// I-1 повторного ревью fix-10b: текст формулы `<span class="math math-display">` `htmd`
/// выводит как есть, с переводами строк, и цитата ставит префикс на каждую из них — тот же
/// класс, что у `pre`. Общее правило оценки: перевод строки в любом текстовом узле — строка,
/// которой достаётся префикс. 20 000 строк формулы под 120 цитатами (43 КБ; было 4,8 МБ
/// текста) и 2 МиБ под 240 цитатами (было больше 15 с) — `simplified` за ≤ ~1 с в release.
#[test]
fn math_text_lines_under_quotes_are_simplified() {
    let math = |body: String| format!("<span class=\"math math-display\">{body}</span>");
    let small = nested(
        "<blockquote>",
        "</blockquote>",
        120,
        &math("x\n".repeat(20_000)),
    );
    assert!(small.len() < 45_000, "{}", small.len());
    let m = bounded_within("math 120", small, release_second());
    assert!(m.simplified);
    assert!(m.text.len() < 100_000, "{} байт", m.text.len());
    assert!(m.text.contains('x'));
    let big = nested(
        "<blockquote>",
        "</blockquote>",
        240,
        &math(format!("a\n{}a", rep("q\n", 2 * MIB - 8 * 1024))),
    );
    assert!(big.len() <= HTML_MAX_BYTES, "{}", big.len());
    let m = bounded_within("math 240", big, release_second());
    assert!(m.simplified);
    assert!(m.text.len() < 4 * MIB, "{} байт", m.text.len());
    assert!(m.text.contains('q'));
}

/// 2 МиБ строк `pre` под предельной вложенностью цитат, списков и их смеси. До исправления
/// `htmd` на 60 уровнях цитат давал 128 МБ за 6,6 с, на 120 уровнях списков не укладывался
/// в 12 с при 0,75 ГБ памяти. Пустые строки под пунктом списка отступа не получают, и вывод
/// не растёт, но `htmd` проходит их на каждом уровне. Оценка ловит все формы: `simplified` за
/// ≤ ~1 с в release, вывод меньше предела.
#[test]
fn nested_quote_and_list_prefixes_are_bounded() {
    let pre = |unit: &str| format!("<pre>a\n{}a</pre>", rep(unit, 2 * MIB - 8 * 1024));
    let cases = [
        (
            "quotes 60",
            nested("<blockquote>", "</blockquote>", 60, &pre("x\n")),
        ),
        (
            "quotes 240",
            nested("<blockquote>", "</blockquote>", 240, &pre("q\n")),
        ),
        (
            "lists 120",
            nested("<ul><li>", "</li></ul>", 120, &pre("item\n")),
        ),
        (
            "ordered 120",
            nested("<ol start=1000000><li>", "</li></ol>", 120, &pre("n\n")),
        ),
        (
            "mixed 80",
            nested(
                "<blockquote><ul><li>",
                "</li></ul></blockquote>",
                80,
                &pre("m\n"),
            ),
        ),
        (
            "blank lines 125",
            nested("<ul><li>", "</li></ul>", 125, &pre("\n")),
        ),
    ];
    for (label, html) in cases {
        assert!(html.len() <= HTML_MAX_BYTES, "{label}: {}", html.len());
        let m = bounded_within(label, html, release_second());
        assert!(m.simplified, "{label}");
        assert!(m.text.len() < 4 * MIB, "{label}: {} байт", m.text.len());
        assert!(m.text.starts_with('a'), "{label}");
    }
}

/// Обычная вложенность цитат и списков с `pre` идёт полным путём: оценка вывода в бюджете,
/// префиксы строк на месте.
#[test]
fn moderate_quote_and_list_nesting_is_full_path() {
    let html = nested(
        "<blockquote><ul><li>",
        "</li></ul></blockquote>",
        3,
        &format!("<pre>{}</pre>", "code line\n".repeat(2000)),
    );
    let m = bounded("moderate nesting", html);
    assert!(!m.simplified);
    let lines: Vec<&str> = m
        .text
        .lines()
        .filter(|l| l.ends_with("code line"))
        .collect();
    assert_eq!(lines.len(), 2000);
    assert!(lines.iter().all(|l| l.starts_with("> ")), "{}", lines[1]);
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
