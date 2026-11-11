//! Роли и статусы run.

use serde::{Deserialize, Serialize};

/// Роль run в дереве.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    Top,
    Orchestrator,
    Worker,
}

/// Статус run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Pausing,
    Paused,
    Cancelling,
    BlockedOnBudget,
    Done,
    Partial,
    Blocked,
    Failed,
    Cancelled,
}

/// Причины паузы run: поле `reason` события `run.status` при переходе в `paused`.
///
/// Значения — часть протокола: их читают UI, статус и автоподъём после старта,
/// поэтому менять строки нельзя.
pub mod pause_reason {
    /// Нужно решение человека: исход `NonIdempotent`-эффекта неизвестен.
    pub const NEEDS_DECISION: &str = "needs_decision";
    /// Записанный в журнале эффект расходится с тем, что run делает сейчас.
    pub const REPLAY_MISMATCH: &str = "replay_mismatch";
    /// Заявка в очереди к модели ждала дольше `max_wait` (RES-007).
    pub const QUEUE: &str = "queue";
    /// Не хватает ресурса узла: RAM, VRAM, места на диске (RES-012).
    pub const RESOURCE: &str = "resource";
    /// Провайдер модели недоступен или отказал, повторы исчерпаны (MDL-016).
    pub const PROVIDER: &str = "provider";
    /// Дневной бюджет облачной группы исчерпан (RES-004, `QueueError::BudgetExhausted`).
    pub const BUDGET: &str = "budget";
    /// Пауза по команде пользователя (`RunControl::pause`, Ctrl+C).
    pub const COMMAND: &str = "command";
    /// Модель спросила пользователя (`ask_user`), ждём ответа.
    pub const NEEDS_INPUT: &str = "needs_input";
    /// Вызов инструмента ждёт одобрения человека.
    pub const NEEDS_APPROVAL: &str = "needs_approval";
    /// Ждём внешнего результата или таймера.
    pub const WAITING: &str = "waiting";
    /// Внутренняя ошибка рантайма или модели, не относящаяся к провайдеру.
    pub const INTERNAL: &str = "internal";
    /// Контекст модели переполнен.
    pub const CONTEXT: &str = "context";
    /// Модель отказалась отвечать (`finish = Refusal` или фильтр содержимого).
    pub const REFUSAL: &str = "refusal";
    /// Подряд ошибки инструментов одного класса (R1-B2 D57).
    pub const TOOL_ERRORS: &str = "tool_errors";
    /// Модель зациклилась: повтор текста или бесконечное рассуждение (R1-B2 D58, D59, D61).
    pub const MODEL_LOOP: &str = "model_loop";
    /// Разовый сбой журнала посреди шага (R1-C1 D72): пауза только статусом, без
    /// снимка; resume реплеит шаг с последнего снимка.
    pub const STORE: &str = "store";
    /// Первый вызов `Exec` в недоверенном проекте ждёт решения о доверии (R1-C1 D83).
    pub const NEEDS_TRUST: &str = "needs_trust";
}

/// Причины `Ask` политики (R1-C1 D80): поле `reasons` payload прерывания одобрения.
/// Порядок констант — порядок причин в payload (Ruling 1.5).
///
/// Значения — часть протокола (их читают CLI и UI), менять строки нельзя.
pub mod approval_reason {
    /// Режим run требует одобрения.
    pub const MODE: &str = "mode";
    /// Сработало правило `ask`.
    pub const RULE: &str = "rule";
    /// Путь вне проекта без гранта (D77).
    pub const OUTSIDE_PROJECT: &str = "outside_project";
    /// Контекст run помечен недоверенным содержимым (D82).
    pub const TAINTED: &str = "tainted";
    /// Необратимая команда (D81).
    pub const IRREVERSIBLE: &str = "irreversible";
    /// Запрос гранта (`request_grant`, D78).
    pub const GRANT: &str = "grant";
}

/// Причины отказа политики (R1-C1 D79, D80, D83): текст результата `denied: <причина>`.
///
/// Значения — часть протокола, менять строки нельзя.
pub mod deny_reason {
    /// Нативная запись в защищённый путь (D79).
    pub const PROTECTED_PATH: &str = "protected path";
    /// Сработало правило `deny` (D80).
    pub const POLICY_RULE: &str = "policy rule";
    /// Пользователь отказал проекту в доверии (D83).
    pub const UNTRUSTED_PROJECT: &str = "untrusted project";
}

/// Метки необратимых команд (R1-C1 D81): поле `irreversible` payload прерывания.
///
/// Значения — часть протокола, менять строки нельзя.
pub mod irreversible {
    /// Удаление файлов: `rm -r`/`-f`, `find -delete`, `Remove-Item -Recurse`.
    pub const DELETE: &str = "delete";
    /// Сброс рабочей копии git: `reset --hard`, `checkout .`, `clean -f`.
    pub const GIT_RESET: &str = "git_reset";
    /// Принудительный `git push`.
    pub const GIT_FORCE_PUSH: &str = "git_force_push";
    /// Публикация пакета или образа.
    pub const PUBLISH: &str = "publish";
    /// Изменение инфраструктуры: `terraform apply`, `kubectl delete`.
    pub const INFRA: &str = "infra";
    /// `DROP TABLE`, `TRUNCATE` в клиенте СУБД.
    pub const SQL_DROP: &str = "sql_drop";
    /// Перенаправление `>` в путь вне проекта: там нет чекпоинта.
    pub const OVERWRITE: &str = "overwrite";
    /// Команда не разобрана: неизвестное считается необратимым.
    pub const UNPARSED: &str = "unparsed";
    /// Интерпретатор исполняет код из stdin или конвейера; запуск процесса методом .NET.
    pub const EXEC_UNTRUSTED: &str = "exec_untrusted";
    /// `git config` с ключом, исполняющим код или меняющим поведение git.
    pub const GIT_CONFIG_EXEC: &str = "git_config_exec";
}

/// Происхождения недоверенного содержимого (R1-C1 D82): `LoopState.taint` и первая
/// строка `[untrusted content: <origin>]` результата.
///
/// Значения — часть протокола, менять строки нельзя.
pub mod taint_origin {
    /// Прочитан путь вне проекта, в том числе по гранту.
    pub const OUTSIDE_PROJECT: &str = "outside_project";
    /// Проект не доверен (D83).
    pub const UNTRUSTED_PROJECT: &str = "untrusted_project";
    /// Вывод сетевой команды.
    pub const NETWORK: &str = "network";
    /// Инструмент сам пометил результат недоверенным.
    pub const TOOL: &str = "tool";
}

/// Вид чекпоинта (R1-C1 D89): поле `kind` события `checkpoint.created`.
///
/// Значения — часть протокола, менять строки нельзя.
pub mod checkpoint_kind {
    /// Коммит git под `refs/zero-agent/checkpoints/…`.
    pub const GIT: &str = "git";
    /// Копия затронутых файлов с манифестом.
    pub const FILES: &str = "files";
    /// Чекпоинт не сделан; причина — в `reason`.
    pub const SKIPPED: &str = "skipped";
}

/// Причина чекпоинта вида `skipped` (R1-C1 D89): поле `reason` события
/// `checkpoint.created`.
///
/// Значения — часть протокола, менять строки нельзя.
pub mod checkpoint_reason {
    /// git не уложился в `CHECKPOINT_TIMEOUT`; до конца run чекпоинты git не делаются.
    pub const SLOW: &str = "slow";
    /// Проект не в репозитории git, игнорируется git или недоверен (`Exec`).
    pub const NO_GIT: &str = "no_git";
    /// git нет в `PATH`.
    pub const GIT_NOT_FOUND: &str = "git_not_found";
    /// git отказал по владельцу репозитория (`safe.directory` не трогается).
    pub const GIT_UNSAFE_DIRECTORY: &str = "git_unsafe_directory";
    /// Прочий отказ git (подробности — в логе).
    pub const GIT_ERROR: &str = "git_error";
    /// Сбой ввода-вывода при копии файлов (подробности — в логе).
    pub const IO_ERROR: &str = "io_error";
}

/// Действия журнала аудита (R1-C1 D91): поле `action` записи `audit`.
///
/// Значения — часть протокола, менять строки нельзя.
pub mod audit_action {
    /// Решение о прерывании одобрения.
    pub const APPROVAL_RESOLVED: &str = "approval.resolved";
    /// Выдан грант доступа вне проекта.
    pub const GRANT_ADDED: &str = "grant.added";
    /// Проекту дано доверие.
    pub const TRUST_GRANTED: &str = "trust.granted";
    /// Доверие к проекту отозвано.
    pub const TRUST_REVOKED: &str = "trust.revoked";
    /// Загружена политика с новым хэшем.
    pub const POLICY_LOADED: &str = "policy.loaded";
    /// Рабочая копия восстановлена из чекпоинта.
    pub const CHECKPOINT_RESTORED: &str = "checkpoint.restored";
}

/// Детекторы зацикливания: поле `detector` события `loop.detected` (R1-B2 D63) и
/// причина перехода run в `blocked` по детектору (D54–D56); у исхода `degenerate`
/// события `turn.finished` — поле `detector` (D69).
///
/// Значения — часть протокола (их читают UI и инспектор), менять строки нельзя.
pub mod detector {
    /// Один и тот же вызов пятый раз подряд в окне ходов (D54).
    pub const REPEAT: &str = "repeat";
    /// Цикл ходов `X,Y,X,Y` или `X,Y,Z,X,Y,Z` без новых результатов (D55).
    pub const CYCLE: &str = "cycle";
    /// Ходы подряд без прогресса (D56).
    pub const STALL: &str = "stall";
    /// Повтор текста в стриме (D58); тот же хвост у квитанции `interrupted:repetition`.
    pub const REPETITION: &str = "repetition";
    /// Бесконечное рассуждение (D59); тот же хвост у квитанции `interrupted:thinking`.
    pub const THINKING: &str = "thinking";
    /// Обещание без действия: ход без вызовов с оборотом «I will…» (D60).
    pub const PROMISE: &str = "promise";
    /// Ошибки подряд у инструментов одного класса (D57).
    pub const TOOL_ERRORS: &str = "tool_errors";
    /// Ходы подряд с неразобранными вызовами — порча JSON (D61).
    pub const PARSE_ERRORS: &str = "parse_errors";
}

/// Действие детектора: поле `action` события `loop.detected` (R1-B2 D63).
///
/// Значения — часть протокола (их читают UI и инспектор), менять строки нельзя.
pub mod detector_action {
    /// Прошлый результат показан без исполнения (D54).
    pub const REUSE: &str = "reuse";
    /// Вызов отклонён без исполнения (D54).
    pub const REJECT: &str = "reject";
    /// Управляющее сообщение модели (D54, D55, D60).
    pub const NUDGE: &str = "nudge";
    /// Run — `blocked` (D54–D56).
    pub const BLOCK: &str = "block";
    /// Повтор хода с другими параметрами (D58, D59).
    pub const RETRY: &str = "retry";
    /// Run — `paused` (D57–D59, D61).
    pub const PAUSE: &str = "pause";
    /// Пауза перед следующим вызовом модели (D57).
    pub const SLEEP: &str = "sleep";
    /// Аварийная компакция на ближайшей границе (D61).
    pub const COMPACT: &str = "compact";
}

/// Вид события `context.alert`: поле `kind` (R1-B2 D43, D45, D62, D70).
///
/// Значения — часть протокола (их читают UI и инспектор), менять строки нельзя.
pub mod alert_kind {
    /// Эндпоинт не сообщил размер окна, принят по умолчанию (D43).
    pub const N_CTX_UNKNOWN: &str = "n_ctx_unknown";
    /// Окно сменилось на resume (D43).
    pub const N_CTX_CHANGED: &str = "n_ctx_changed";
    /// Расход хода меньше половины оценки: вероятна обрезка сервером (D43).
    pub const TRUNCATION_SUSPECTED: &str = "truncation_suspected";
    /// Файл выгрузки не записан, удаление пропущено (D45).
    pub const PRUNE_FAILED: &str = "prune_failed";
    /// После компакции оценка выше порога (D70).
    pub const COMPACTION_INSUFFICIENT: &str = "compaction_insufficient";
    /// Компакция завершилась ошибкой модели (D46).
    pub const COMPACTION_FAILED: &str = "compaction_failed";
    /// Доля входа из кэша упала без смены эпохи истории (D62).
    pub const CACHE_DROP: &str = "cache_drop";
}

/// Причины ошибки run: поле `reason` события `run.status` при переходе в `failed`.
///
/// Короткий машинный код класса ошибки цикла; подробность — в логах. Значения —
/// часть протокола (их читают CLI и UI), менять строки нельзя.
pub mod fail_reason {
    /// Ошибка журнала, после которой статус удалось записать.
    pub const STORE: &str = "store";
    /// Контекст run отравлен прежней ошибкой.
    pub const POISONED: &str = "poisoned";
    /// Прочая ошибка протокола эффектов (квитанция-отказ там, где её быть не должно).
    pub const EFFECT: &str = "effect";
    /// Нарушен инвариант истории диалога.
    pub const HISTORY: &str = "history";
    /// Снимок run не пишется или не читается.
    pub const SNAPSHOT: &str = "snapshot";
    /// Нарушен инвариант графа цикла.
    pub const GRAPH: &str = "graph";
    /// Run в статусе, из которого продолжить нельзя.
    pub const NOT_RESUMABLE: &str = "not_resumable";
    /// Эндпоинт модели неизвестен.
    pub const UNKNOWN_ENDPOINT: &str = "unknown_endpoint";
    /// Смена модели посреди шага.
    pub const CHANGE_MID_STEP: &str = "change_mid_step";
    /// Префикс промпта не собирается (конфигурация run).
    pub const PREFIX: &str = "prefix";
    /// Запрос хода не сериализуется.
    pub const REQUEST: &str = "request";
}

impl RunStatus {
    /// Допустим ли переход `self -> to` (схема ТЗ, часть 01, раздел 4.3).
    pub fn can_transition(self, to: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            (self, to),
            (Queued, Running)
                | (Queued, Cancelling)
                | (Queued, Cancelled)
                | (Running, Pausing)
                // Немедленная пауза в безопасной точке, выбранной самим рантаймом:
                // ожидание решения человека, отказ провайдера.
                | (Running, Paused)
                | (Pausing, Paused)
                | (Pausing, Running)
                | (Pausing, Failed)
                | (Pausing, Cancelling)
                | (Paused, Running)
                | (Running, Cancelling)
                | (Paused, Cancelling)
                | (BlockedOnBudget, Cancelling)
                | (Cancelling, Cancelled)
                | (Running, BlockedOnBudget)
                | (BlockedOnBudget, Running)
                | (Running, Done | Partial | Blocked | Failed)
        )
    }

    /// Терминальный ли статус (выходов нет).
    pub fn is_terminal(self) -> bool {
        use RunStatus::*;
        matches!(self, Done | Partial | Blocked | Failed | Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_has_no_exits() {
        assert!(!RunStatus::Cancelled.can_transition(RunStatus::Running));
        assert!(RunStatus::Running.can_transition(RunStatus::Pausing));
        assert!(RunStatus::Paused.can_transition(RunStatus::Running));
    }

    #[test]
    fn terminal_statuses_never_transition() {
        use RunStatus::*;
        let all = [
            Queued,
            Running,
            Pausing,
            Paused,
            Cancelling,
            BlockedOnBudget,
            Done,
            Partial,
            Blocked,
            Failed,
            Cancelled,
        ];
        for from in all.into_iter().filter(|s| s.is_terminal()) {
            assert!(all.into_iter().all(|to| !from.can_transition(to)));
        }
    }

    #[test]
    fn new_transitions_allowed() {
        use RunStatus::*;
        for (a, b) in [
            (Queued, Cancelling),
            (Queued, Cancelled),
            (Pausing, Cancelling),
            (BlockedOnBudget, Cancelling),
            (Pausing, Running),
            (Pausing, Paused),
            (Pausing, Failed),
        ] {
            assert!(a.can_transition(b), "{a:?} -> {b:?}");
        }
    }

    #[test]
    fn running_can_pause_immediately() {
        // Немедленная пауза в безопасной точке, выбранной рантаймом
        // (ожидание решения, отказ провайдера), минуя `pausing`.
        assert!(RunStatus::Running.can_transition(RunStatus::Paused));
        assert!(!RunStatus::Paused.can_transition(RunStatus::Paused));
    }

    #[test]
    fn every_non_terminal_can_reach_cancelling() {
        use RunStatus::*;
        for s in [Queued, Running, Pausing, Paused, BlockedOnBudget] {
            assert!(s.can_transition(Cancelling), "{s:?}");
        }
        assert!(!Cancelling.can_transition(Cancelling));
    }

    #[test]
    fn status_serde_round_trip_snake_case() {
        let s = serde_json::to_string(&RunStatus::BlockedOnBudget).unwrap();
        assert_eq!(s, r#""blocked_on_budget""#);
        assert_eq!(
            serde_json::from_str::<RunStatus>(&s).unwrap(),
            RunStatus::BlockedOnBudget
        );
    }
}
