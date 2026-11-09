-- Схема журнала проекта `journal.db`, версия 2 (ТЗ, часть 01, раздел 5.2; R1-B1 D19).
-- Расход модели по вызовам: строка на каждое событие `model.usage` (поле `seq`),
-- суммы по run служат для бюджетов (CORE-113). `cost_eq` — стоимость в долларах США
-- (провайдер или расчёт по ценам), `NULL`, если неизвестна; `gpu_ms` — время GPU.

CREATE TABLE usage (
    run_id    BLOB    NOT NULL,
    seq       INTEGER NOT NULL REFERENCES events(seq),
    model     TEXT    NOT NULL,
    input     INTEGER NOT NULL,
    cached    INTEGER NOT NULL,
    output    INTEGER NOT NULL,
    reasoning INTEGER NOT NULL,
    gpu_ms    INTEGER,
    cost_eq   REAL,
    ts        TEXT    NOT NULL
) STRICT;

CREATE INDEX usage_run ON usage(run_id);
