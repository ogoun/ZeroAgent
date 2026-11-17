//! Синтетические страницы корпуса `za-extract` (m4 финального ревью извлечения): десять
//! страниц, по строению близких к реальным. Их вывод закреплён тестом `tests/corpus.rs`
//! (хэши в `tests/corpus/expected.txt`), а страницы проверяют и тесты `tests/html.rs`.
//! Модуль подключается в оба теста через `#[path]`; это не отдельная цель тестов.
//!
//! Генераторы детерминированы: ни случайности, ни времени, ни окружения. Любая правка
//! страницы меняет закреплённый вывод и требует осознанного обновления `expected.txt`.

const MIB: usize = 1024 * 1024;

/// Страница вида статьи Википедии (Vector 2022): шапка с флажками-переключателями меню,
/// кнопками и формой поиска, сноски, таблицы, `<noscript>` с картинкой, скрипты в конце.
pub fn wiki_like() -> String {
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
pub fn mdn_like() -> String {
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

/// Строит семь страниц с разнообразным, но обычным строением.
pub fn realistic_pages() -> Vec<(&'static str, String)> {
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
