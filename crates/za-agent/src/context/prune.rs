//! Удаление старых результатов инструментов (CTX-008, шаг 2; R1-B2 D45).
//!
//! Результаты старше последних `keep_turns` ходов и длиннее `min_bytes` заменяются
//! строкой `[result removed to save context; full text: <путь spill>]`. Выгруженный
//! результат (D44) ссылается на свой файл; невыгруженный сначала пишется в
//! `{run}-{call_id}.txt` текстом из истории (он уже экранирован). Запись на границе —
//! вне эффекта, исключение D30: имя и содержимое детерминированы историей, запись
//! идемпотентна, поэтому реплей границы даёт ту же историю.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use za_llm::Role;
use za_protocol::RunId;

use super::spill::{prune_name, spill_path, spilled_path, write_spill};
use crate::conversation::Conversation;

/// Сколько последних ходов не трогается (D45).
pub(crate) const PRUNE_KEEP_TURNS: usize = 4;
/// Результат длиннее стольких байт удаляется (D45).
pub(crate) const PRUNE_MIN_BYTES: usize = 1024;

/// Текст на месте удалённого результата (D45, D23: уходит модели — по-английски).
fn removed(path: &str) -> String {
    format!("[result removed to save context; full text: {path}]")
}

/// D45. Возвращает новую историю и число заменённых; spill для невыгруженных —
/// `{run}-{call_id}.txt` с текстом из истории. Исходная история не меняется; результаты
/// последнего хода не трогаются при любом `keep_turns`. Ошибка — ввод-вывод файла
/// выгрузки или нарушенный инвариант истории (`InvalidData`).
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
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut count = 0;
    let mut failure: Option<io::Error> = None;
    let out = conv
        .rewrite_results(&mut |turn, call_id, content| {
            // Номер появления id считается по всем результатам: имя не зависит от того,
            // какие из них удаляются.
            let occurrence = {
                let n = seen.entry(call_id.to_owned()).or_insert(0);
                *n += 1;
                *n
            };
            if failure.is_some() || turn >= old_before || content.len() <= min_bytes {
                return None;
            }
            let path = match spilled_path(project_dir, run, content) {
                Some(p) => p,
                None => {
                    let name = prune_name(run, call_id, occurrence);
                    if let Err(e) = write_spill(project_dir, &name, content.as_bytes()) {
                        failure = Some(e);
                        return None;
                    }
                    spill_path(&name)
                }
            };
            count += 1;
            Some(removed(&path))
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(e) = failure {
        return Err(e);
    }
    Ok((out, count))
}
