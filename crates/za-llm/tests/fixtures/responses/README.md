# Фикстуры OpenAI Responses

Составлены 2026-11-09 по типам SDK `openai-python` (ветка main; типы генерируются из
OpenAPI-спецификации OpenAI: `types/responses/response_*_event.py`, `response_usage.py`,
`response_reasoning_item.py`, `response_error.py`) и руководствам developers.openai.com
(«Function calling», «Reasoning»), не записаны с живого API. При доступе к ключу
(`OPENAI_TEST_KEY`) заменить записями живых ответов. Байты не нормализуются git (`-text` в
`.gitattributes`). Значения `id`, `encrypted_content` и токенов — условные.

| Файл | Форма |
|---|---|
| `text.sse` | `response.created`, `response.in_progress`, `output_item.added` (`message`), `content_part.added`, два `output_text.delta` (с `obfuscation`), `output_text.done`, `content_part.done`, `output_item.done`, `response.completed` с `usage` (`input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens`). |
| `function_call.sse` | `output_item.added` (`function_call` с `call_id`, `name`, пустыми `arguments`), два `function_call_arguments.delta`, `function_call_arguments.done`, `output_item.done`, `response.completed`. |
| `reasoning_encrypted.sse` | Запрос с `store: false` и `include: ["reasoning.encrypted_content"]`: `output_item.added` (`reasoning`, неполный `encrypted_content`), `reasoning_summary_part.added`, два `reasoning_summary_text.delta`, `.done`, `reasoning_summary_part.done`, `output_item.done` (`reasoning` целиком с полным `encrypted_content` — его и нужно возвращать), затем вызов функции. |
| `failed.sse` | `response.failed`: `response.status: "failed"`, `response.error {code: "server_error", message}`. |
