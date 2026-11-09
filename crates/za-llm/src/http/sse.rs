//! Разбор Server-Sent Events по WHATWG HTML, раздел 9.2.6.
//!
//! Строки кончаются на `\n`, `\r\n` или `\r` (в том числе `\r` и `\n` в разных
//! чанках). Байты копятся до границы строки и только потом декодируются, поэтому
//! символ UTF-8, разрезанный между чанками, собирается точно (Review Focus 2).
//! Поля `data` склеиваются через `\n`; строки с `:` в начале — комментарии; событие
//! отправляется по пустой строке, если в нём было хотя бы одно поле `data`.
//! Отклонения от WHATWG: `id` относится к одному событию (последний id потока не
//! наследуется), `retry` игнорируется, [`SseParser::finish`] отправляет событие без
//! завершающей пустой строки.

/// Предел длины одной строки: защита от сервера, который шлёт байты без перевода строки.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// Событие SSE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// Потоковый разборщик SSE.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Байты незавершённой строки.
    buf: Vec<u8>,
    /// Предыдущий чанк кончился на `\r`: ведущий `\n` следующего — часть того же конца строки.
    skip_lf: bool,
    /// Первая строка уже прочитана (BOM снимается только с неё).
    started: bool,
    data: String,
    has_data: bool,
    event: Option<String>,
    id: Option<String>,
    /// Причина отказа: после неё разборщик ничего не отдаёт.
    malformed: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавляет чанк и возвращает события, завершённые в нём.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.malformed.is_some() {
            return out;
        }
        let mut chunk = chunk;
        if self.skip_lf && !chunk.is_empty() {
            if chunk[0] == b'\n' {
                chunk = &chunk[1..];
            }
            self.skip_lf = false;
        }
        let scan_from = self.buf.len();
        self.buf.extend_from_slice(chunk);

        let mut start = 0;
        let mut i = scan_from;
        while i < self.buf.len() {
            let b = self.buf[i];
            if b != b'\n' && b != b'\r' {
                i += 1;
                continue;
            }
            let line = self.buf[start..i].to_vec();
            let mut next = i + 1;
            if b == b'\r' {
                if next < self.buf.len() {
                    if self.buf[next] == b'\n' {
                        next += 1;
                    }
                } else {
                    self.skip_lf = true;
                }
            }
            if let Some(ev) = self.line(&line) {
                out.push(ev);
            }
            if self.malformed.is_some() {
                self.buf.clear();
                return out;
            }
            start = next;
            i = next;
        }
        self.buf.drain(..start);
        if self.buf.len() > MAX_LINE {
            self.buf.clear();
            self.malformed = Some(format!("строка SSE длиннее {MAX_LINE} байт"));
        }
        out
    }

    /// Конец потока: дочитывает незавершённую строку и отправляет накопленное событие.
    /// Повторный вызов даёт `None`.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.malformed.is_some() {
            return None;
        }
        self.skip_lf = false;
        let mut ev = None;
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            ev = self.line(&line);
        }
        if ev.is_none() && self.malformed.is_none() {
            ev = self.dispatch();
        }
        ev
    }

    /// В буфере есть строка без конца строки: если тело на этом кончилось, это обрыв.
    pub fn has_partial_line(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Причина отказа разбора (невалидный UTF-8, слишком длинная строка).
    pub fn malformed(&self) -> Option<&str> {
        self.malformed.as_deref()
    }

    /// Разбирает одну строку без конца строки; пустая строка отправляет событие.
    fn line(&mut self, raw: &[u8]) -> Option<SseEvent> {
        let mut raw = raw;
        if !self.started {
            self.started = true;
            raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
        }
        let Ok(line) = std::str::from_utf8(raw) else {
            self.malformed = Some("невалидный UTF-8 в строке SSE".to_owned());
            return None;
        };
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_owned()),
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        let id = self.id.take();
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        let mut data = std::mem::take(&mut self.data);
        data.pop(); // завершающий `\n` последнего поля `data`
        Some(SseEvent { event, data, id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_data_field_is_dispatched() {
        let mut p = SseParser::new();
        let ev = p.push(b"data\n\nevent: x\n\n");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "");
    }

    #[test]
    fn bom_and_id_are_handled() {
        let mut p = SseParser::new();
        let ev = p.push(b"\xEF\xBB\xBFid: 7\ndata:x\n\n");
        assert_eq!(ev[0].id.as_deref(), Some("7"));
        assert_eq!(ev[0].data, "x");
    }

    #[test]
    fn invalid_utf8_stops_parser() {
        let mut p = SseParser::new();
        let ev = p.push(b"data: ok\n\ndata: \xff\n\ndata: later\n\n");
        assert_eq!(ev.len(), 1);
        assert!(p.malformed().is_some());
        assert!(p.push(b"data: y\n\n").is_empty());
        assert_eq!(p.finish(), None);
    }
}
