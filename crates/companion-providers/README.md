# `companion-providers`

Upstream LLM provider abstraction and raw HTTP forwarding for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | `ProviderAdapter` trait, `ForwardRequest`/`ForwardResponse` types, curated Gemini 3.8+/4.x catalog, `filter_and_order_models`, and `model_capability_sort_key` |
| `src/gemini.rs` | Raw `reqwest` `GeminiProvider` with zero compression (`S4`), `x-goog-api-key` auth (stripping `?key=`), SSRF/HTTPS/no-redirect guards (`S3`), model discovery, and connection health checks |
