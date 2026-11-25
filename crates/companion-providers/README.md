# `companion-providers`

Upstream LLM provider abstraction and raw HTTP forwarding for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | `ProviderAdapter` trait, `ForwardRequest`/`ForwardResponse` types, `curated_gemini_models`, and `filter_and_order_models` (delegating curated Gemini 3.8+/4+ catalog & `model_capability_sort_key` to `companion-core::models`) |
| `src/gemini/mod.rs` | Raw `reqwest` `GeminiProvider` with zero compression (`S4`), `x-goog-api-key` auth, `forward_gemini`, `list_models`, `health_check`, and sanitized upstream error extraction |
| `src/gemini/url.rs` | `EnvKeySource`, upstream URL normalization, SSRF/HTTPS/loopback/no-redirect guards (`S3`), `?key=` query stripping (preserving `?alt=sse`), and RFC 3986 path/header/API-key validation delegating to `companion-core::validation` |
| `src/gemini/tests.rs` | Unit tests for `GeminiProvider` URL/header validation, auth, streaming pass-through, and model discovery |
