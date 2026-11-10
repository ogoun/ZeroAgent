//! Удаление старых результатов инструментов (CTX-008, шаг 2; R1-B2 D45).
//!
//! Результаты старше последних `keep_turns` ходов и длиннее `min_bytes` заменяются
//! строкой `[result removed to save context; full text: <путь spill>]`. Текст результата
//! всегда пишется в файл из истории (он уже экранирован): у выгруженного результата
//! (D44) это голова, хвост и настоящий указатель на полный файл, поэтому ничего не
//! теряется. Указатель внутри текста не разбирается — текст недоверенный (Ruling I-1
//! ревью задачи 2). Имя файла — по содержимому, `{run}-{blake3[..16]}.txt` (Ruling
//! I-2). Запись на границе — вне эффекта, исключение D30: имя и содержимое
//! детерминированы историей, запись идемпотентна, поэтому реплей границы даёт ту же
//! историю; файловая система на текст новой истории не влияет.

use std::io;
use std::path::Path;

use za_llm::Role;
use za_protocol::RunId;

use super::spill::{content_name, spill_path, write_spill};
use crate::conversation::Conversation;

/// Сколько последних ходов не трогается (D45).
pub(crate) const PRUNE_KEEP_TURNS: usize = 4;
/// Результат длиннее стольких байт удаляется (D45).
pub(crate) const PRUNE_MIN_BYTES: usize = 1024;

/// Текст на месте удалённого результата (D45, D23: уходит модели — по-английски).
/// Вызов, которому отвечает результат, модель видит по `call_id` самого результата.
fn removed(path: &str) -> String {
    format!("[result removed to save context; full text: {path}]")
}

/// D45. Возвращает новую историю и число заменённых; текст каждого заменённого
/// результата — в `{run}-{blake3(content)[..16]}.txt`. Исходная история не меняется;
/// результаты последнего хода не трогаются при любом `keep_turns`. Ошибка — ввод-вывод
/// файла выгрузки (первая; тогда история не меняется) или нарушенный инвариант
/// истории (`InvalidData`).
pub fn prune(
    conv: &Conversation,
    run: RunId,
    project_dir: &Path,
    keep_turns: usize,
    min_bytes: usize,
) -> io::Result<(Conversation, usize)> {
    let turns = conv
        .messages()
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    let old_before = turns.saturating_sub(keep_turns.max(1));
    let mut count = 0;
    let mut failure: Option<io::Error> = None;
    let out = conv
        .rewrite_results(&mut |turn, _call_id, content| {
            if failure.is_some() || turn >= old_before || content.len() <= min_bytes {
                return None;
            }
            let name = content_name(run, content);
            if let Err(e) = write_spill(project_dir, &name, content.as_bytes()) {
                failure = Some(e);
                return None;
            }
            count += 1;
            Some(removed(&spill_path(&name)))
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(e) = failure {
        return Err(e);
    }
    Ok((out, count))
}
