# ADR 0002. Метрики GPU через процесс `nvidia-smi`, а не NVML

- Статус: принято (R1-A, 2026-11-09). Источник: решение плана D6, `docs/plans/2026-11-09-r1a-models.md`.
- Требования: RES-010, MDL-012, Review Focus 4 плана R1-A; ограничение workspace `unsafe_code = "forbid"`.

## Контекст

Допуск заявок к локальным моделям учитывает VRAM (D10, D30), а `resources` и `doctor` показывают GPU. Основная машина — RTX 3090 под Windows (WDDM), CI — машины без GPU. Источник метрик должен работать без `unsafe` в нашем коде, без тяжёлых зависимостей и деградировать без NVIDIA: GPU другого вендора, нет драйвера, `nvidia-smi` отвечает `[N/A]`.

## Решение

- Каждый замер — отдельный процесс `nvidia-smi --query-gpu=index,name,uuid,utilization.gpu,memory.total,memory.used,memory.free,temperature.gpu --format=csv,noheader,nounits`, таймаут 5 с, `kill_on_drop`; на Windows — `CREATE_NO_WINDOW`.
- Процесс запускается с очищенным окружением (ключи `ZA_SECRET_*` не наследуются) и временным каталогом `ZA_HOME/work/tmp/monitor` (D33).
- Период — 2 с, пока GPU доступен; нет `nvidia-smi`, ошибка или таймаут — `GpuStatus::Unavailable { reason }`, повтор через 60 с. Поля `[N/A]` — `None`.
- Путь к исполняемому файлу настраивается: `[resources] nvidia_smi`.
- Недоступный GPU не блокирует допуск навсегда; `doctor` показывает его как `warn`.

## Альтернативы

- **NVML через `libloading`.** `Library::new` — `unsafe fn`, что запрещено `unsafe_code = "forbid"`.
- **`nvml-wrapper`.** Безопасный API, но тянет `libloading` и proc-macro цепочку `wrapcenum-derive → darling → syn`; против минимума зависимостей (N-13).
- **Только `sysinfo`.** Не даёт VRAM NVIDIA.
- **WMI / DXGI на Windows.** Потребовали бы `unsafe` или COM-обёртки, и отдельный путь для Linux.

## Последствия

- Запуск процесса раз в 2 с — пренебрежимая нагрузка; замер GPU идёт отдельной задачей монитора и не задерживает CPU, RAM и тома.
- AMD и Intel пока «недоступны» (P2, позже — отдельный источник за тем же `GpuStatus`).
- Живая проверка на RTX 3090 (2026-11-09): `zero-agent resources` и `doctor` показывают `NVIDIA GeForce RTX 3090: всего 24576 МиБ`; при несуществующем `nvidia_smi` — `warn`, код 0.
