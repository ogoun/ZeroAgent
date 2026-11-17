//! Маленькие PDF из байтов теста (R1-C2 D121, задача 10): без файлов-фикстур с чужим
//! содержимым. Один помощник на workspace, подключается модулем:
//!
//! ```ignore
//! #[path = "../../../test-support/pdf.rs"]
//! mod pdf;
//! ```
//!
//! Документ — PDF 1.4 с одной страницей на строку `pages`: шрифт Helvetica (стандартный, без
//! встраивания, кодировка WinAnsi), текст — оператор `Tj`, таблица `xref` с точными
//! смещениями. Строки страниц — ASCII без `(`, `)` и `\`.
#![allow(dead_code)]

/// PDF из двух страниц с текстом `alpha` и `beta`.
pub fn two_page_pdf() -> Vec<u8> {
    pdf_with_pages(&["alpha", "beta"])
}

/// PDF со страницами `pages` (пустая строка — страница без текстового слоя).
pub fn pdf_with_pages(pages: &[&str]) -> Vec<u8> {
    build(pages, false)
}

/// Зашифрованный PDF (обработчик `Standard`, RC4 40 бит), пароль пользователя которого не
/// пуст: без пароля документ не прочитать. Значения `/O` и `/U` произвольны — пустой пароль
/// их не подтверждает.
pub fn encrypted_pdf() -> Vec<u8> {
    build(&["secret"], true)
}

fn build(pages: &[&str], encrypt: bool) -> Vec<u8> {
    let n = pages.len();
    // Объекты: 1 — каталог, 2 — дерево страниц, 3 — шрифт, затем по паре «страница,
    // содержимое» на каждую страницу.
    let mut objects: Vec<String> = Vec::new();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_owned());
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {n} >>",
        kids.join(" ")
    ));
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_owned(),
    );
    for (i, text) in pages.iter().enumerate() {
        let contents = 5 + 2 * i;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {contents} 0 R >>"
        ));
        let stream = if text.is_empty() {
            String::new()
        } else {
            format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET")
        };
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let encrypt_ref = if encrypt {
        let hex = "A1".repeat(32);
        objects.push(format!(
            "<< /Filter /Standard /V 1 /R 2 /Length 40 /O <{hex}> /U <{hex}> /P -4 >>"
        ));
        format!(
            " /Encrypt {} 0 R /ID [<{id}> <{id}>]",
            objects.len(),
            id = "0F".repeat(16)
        )
    } else {
        String::new()
    };
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R{encrypt_ref} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Смещение таблицы `xref` в документе [`pdf_with_pages`]: всё до него — тело без хвоста.
/// Первое вхождение: второе — в `startxref`.
pub fn xref_offset(pdf: &[u8]) -> usize {
    pdf.windows(5)
        .position(|w| w == b"xref\n")
        .unwrap_or(pdf.len())
}
