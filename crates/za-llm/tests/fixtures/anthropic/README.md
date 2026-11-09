# Фикстуры Anthropic Messages

Составлены по документации 2026-11-09 (platform.claude.com/docs: «Streaming messages»,
«Refusals and fallback», справочник Claude API), не записаны с живого API. При доступе
к ключу (`ANTHROPIC_TEST_KEY`) заменить записями живых ответов. Байты не нормализуются
git (`-text` в `.gitattributes`). Значения `id`, подписи и токенов — условные.

| Файл | Источник формы |
|---|---|
| `text.sse` | «Streaming messages» — basic streaming request: `message_start` с `usage`, `content_block_start` `text`, `ping`, `text_delta`, `content_block_stop`, `message_delta` со `stop_reason` и `usage.output_tokens`, `message_stop`. Поля кэша `cache_creation_input_tokens`, `cache_read_input_tokens` в `usage` `message_start` — пример с web search того же раздела. |
| `thinking_tool_use.sse` | «Streaming messages» — streaming with thinking (`thinking_delta`, затем `signature_delta` перед `content_block_stop`) и streaming request with tool use (`tool_use {id, name, input:{}}`, `input_json_delta.partial_json`, в том числе пустой первый фрагмент, `stop_reason: "tool_use"`). |
| `overloaded_midstream.sse` | «Streaming messages» — error events: `event: error`, `{"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}` после начала ответа. |
| `refusal.sse` | «Refusals and fallback» / справочник: отказ классификатора до вывода — HTTP 200, пустой `content`, `stop_reason: "refusal"` и `stop_details {type, category, explanation}` в `message_delta`. |
