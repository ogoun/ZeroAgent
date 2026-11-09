-- Схема журнала проекта `journal.db`, версия 1 (ТЗ, часть 01, раздел 5.2).
-- Идентификаторы UUID хранятся как BLOB(16), время — TEXT в RFC 3339 (UTC),
-- JSON — TEXT в каноническом виде. Прочие таблицы раздела 5.2 добавляются
-- миграциями своих этапов.

-- Run: узел дерева агентов.
CREATE TABLE runs (
    id            BLOB    PRIMARY KEY NOT NULL,
    project_id    TEXT    NOT NULL,
    parent_id     BLOB    REFERENCES runs(id),
    root_id       BLOB    NOT NULL,
    role          TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    graph_hash    TEXT,
    fork_of       BLOB,
    fork_seq      INTEGER,
    zam_session   TEXT,
    worktree      TEXT,
    budget_json   TEXT,
    model_profile TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    generation    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX runs_parent ON runs(parent_id);
CREATE INDEX runs_root ON runs(root_id);

-- Блобы по BLAKE3 (ARC-032): данные в строке или путь к файлу рядом с БД.
CREATE TABLE blobs (
    hash         TEXT    PRIMARY KEY NOT NULL,
    size         INTEGER NOT NULL,
    path_or_data ANY     NOT NULL,
    mime         TEXT
) STRICT;

-- Журнал событий, источник истины. AUTOINCREMENT: seq никогда не переиспользуется,
-- даже после удаления хвоста, потому что он служит курсором (ARC-036).
CREATE TABLE events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    id         BLOB    NOT NULL UNIQUE,
    run_id     BLOB    NOT NULL,
    parent_seq INTEGER,
    ts         TEXT    NOT NULL,
    actor      TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    schema_v   INTEGER NOT NULL,
    payload    TEXT    NOT NULL,
    blob_ref   TEXT    REFERENCES blobs(hash),
    tainted    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX events_run_seq ON events(run_id, seq);

-- Протокол эффектов: намерение и квитанция (CORE-002).
CREATE TABLE effects (
    run_id      BLOB    NOT NULL,
    effect_key  TEXT    NOT NULL,
    class       TEXT    NOT NULL,
    intent_seq  INTEGER NOT NULL REFERENCES events(seq),
    receipt_seq INTEGER REFERENCES events(seq),
    blob_ref    TEXT    REFERENCES blobs(hash),
    PRIMARY KEY (run_id, effect_key)
) STRICT;

-- Прерывания: approval, ввод, неизвестный исход и т. п.
CREATE TABLE interrupts (
    id         TEXT PRIMARY KEY NOT NULL,
    run_id     BLOB NOT NULL,
    kind       TEXT NOT NULL,
    status     TEXT NOT NULL,
    payload    TEXT,
    response   TEXT,
    expires_at TEXT
) STRICT;

CREATE INDEX interrupts_run ON interrupts(run_id);

-- Снимки состояния run для resume (CORE-006).
CREATE TABLE snapshots (
    run_id BLOB    NOT NULL,
    seq    INTEGER NOT NULL,
    state  TEXT    NOT NULL,
    PRIMARY KEY (run_id, seq)
) STRICT;
