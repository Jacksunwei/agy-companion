# `companion-providers`

Upstream LLM provider abstraction and raw HTTP forwarding for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | `ProviderAdapter` trait, `ForwardRequest`/`ForwardResponse` types, and `filter_and_order_models` (delegating curated Gemini 3.8+/4+ catalog & `model_capability_sort_key` to `companion-core::models`) |
| `src/gemini.rs` | Raw `reqwest` `GeminiProvider` with zero compression (`S4`), `x-goog-api-key` auth (stripping `?key=`), SSRF/HTTPS/no-redirect guards (`S3`), model discovery, and connection health checks |
