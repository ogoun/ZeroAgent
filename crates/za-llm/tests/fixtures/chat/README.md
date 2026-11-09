# Фикстуры Chat Completions

Составлены по документации и исходникам серверов 2026-11-09, не записаны с живых
серверов. При доступе к серверам заменить записями живых ответов (`LLAMA_TEST_URL`,
`VLLM_TEST_URL`, `OPENROUTER_TEST_KEY`, `OPENAI_TEST_KEY`). Байты не нормализуются
git (`-text` в `.gitattributes`). Значения `id`, `created`, токенов — условные.

| Файл | Источник формы |
|---|---|
| `llama_text.sse` | llama.cpp `tools/server/server-task.cpp` (master, коммит `d05706b4`, 2026-10-27): `to_json_oaicompat_chat` (первый чанк `{role, content:null}`), `to_json_oaicompat_chat_stream` (чанк с `finish_reason` и пустым `delta`, затем чанк `choices: []` с `usage` и `timings`), `result_timings` (`cache_n`, `prompt_n`, `prompt_ms`, `predicted_n`, `predicted_ms`); README `tools/server` — пример `timings`. В `usage` намеренно нет `prompt_tokens_details`, как у сборок до его появления: проверяется подстановка `timings.cache_n`. |
| `llama_tool_calls.sse` | То же; фрагменты `delta.tool_calls[{index, id, type, function:{name, arguments}}]` — `server_chat_msg_diff_to_json_oaicompat` (`common/chat.cpp`), `finish_reason: "tool_calls"`, `usage.prompt_tokens_details.cached_tokens` — `usage_json_oaicompat`. |
| `llama_reasoning_progress.sse` | То же; `prompt_progress {total, cache, processed, time_ms}` — `result_prompt_progress::to_json`, кладётся в последний чанк партии вместе с `delta {role, content:null}` (`return_progress: true`); рассуждение в `delta.reasoning_content` (`reasoning_format: auto`). |
| `vllm_reasoning.sse` | vLLM docs `features/reasoning_outputs` (поле `delta.reasoning`, прежнее имя `reasoning_content`); форма чанков и финального `choices: []` с `usage` — `vllm/entrypoints/openai/serving_chat.py` при `stream_options.include_usage`. |
| `openrouter_cost.sse` | OpenRouter API reference: streaming (комментарий `: OPENROUTER PROCESSING`), `delta.reasoning` и `delta.reasoning_details` (`reasoning.text` с `signature`, `format`, `index`) — guide «Reasoning tokens»; `usage.cost`, `prompt_tokens_details`, `completion_tokens_details.reasoning_tokens` — guide «Usage accounting». |
| `openai_parallel_tools.sse` | OpenAI API reference «Chat Completions — streaming»: `usage: null` в каждом чанке и итоговый чанк `choices: []` с `usage` при `include_usage`; параллельные вызовы `tool_calls[].index` 0 и 1, `id` и `name` в первом фрагменте, `arguments` по частям. |
| `error_midstream.sse` | llama.cpp `server-context.cpp`: ошибка после начала стрима — `data: {"error": {...}}`, тело `format_error_response` (`code` 503, `type` `unavailable_error`). |
