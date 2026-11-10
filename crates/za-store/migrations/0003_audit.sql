-- Схема журнала проекта `journal.db`, версия 3 (ТЗ, часть 01, раздел 5.2; часть 05,
-- SEC-090, SEC-091; R1-C1 D91). Журнал аудита: одобрения, гранты, доверие, политика,
-- восстановление чекпоинтов.
--
-- Записи связаны hash-цепочкой: `hash` записи — BLAKE3 от `prev_hash`, нулевого байта
-- и канонического JSON полей `seq`, `actor`, `action`, `target`, `payload`, `ts`;
-- `prev_hash` первой записи — 64 символа «0». Цепочку считает поток-писатель в той же
-- транзакции, что и вставку (ARC-030: один писатель на файл, гонки `prev_hash` нет).
-- `seq` назначает писатель (следующий за последним), а не SQLite: он входит в хэш.
-- `ts` — миллисекунды Unix (UTC), `payload` — канонический JSON.
--
-- `IF NOT EXISTS`: повторное применение безвредно; миграция идёт в одной транзакции
-- с `user_version`, поэтому сбой посреди неё не оставляет следов.

CREATE TABLE IF NOT EXISTS audit (
    seq       INTEGER PRIMARY KEY,
    prev_hash TEXT    NOT NULL,
    hash      TEXT    NOT NULL,
    actor     TEXT,
    action    TEXT,
    target    TEXT,
    payload   TEXT,
    ts        INTEGER
) STRICT;

-- Последняя запись действия (`last_audit`: доверие проекта, D83).
CREATE INDEX IF NOT EXISTS audit_action ON audit(action, seq);
