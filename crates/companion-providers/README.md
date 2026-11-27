# `companion-providers`

Upstream LLM provider abstraction and raw HTTP forwarding for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | `ProviderAdapter` trait, `ForwardRequest`/`ForwardResponse` types, `curated_gemini_models`, `filter_and_order_models` (delegating curated Gemini 3.8+/4+ catalog & `model_capability_sort_key` to `companion-core::models`), `ls_client`, and `probe_client` re-exports |
| `src/ls_client.rs` | Loopback-only, zero-DNS Antigravity Language Server RPC client (`LsClient`, `LsSummaryStream`, `LsError`, strict allowlist DTOs): IPv4→IPv6 `AtomicU8` latch, `.no_proxy()`, `Policy::none()`, dual sensitive CSRF headers (`x-antigravity-csrf-token` + `x-codeium-csrf-token`), `Accept-Encoding: identity` & fail-closed compression checks, 5-byte Connect HTTP/1.1 framing (`4 MiB` cap), and `from_env()` |
| `src/gemini/mod.rs` | Raw `reqwest` `GeminiProvider` with zero compression (`S4`), `x-goog-api-key` auth, `forward_gemini`, `list_models`, `health_check`, and sanitized upstream error extraction |
| `src/gemini/url.rs` | `EnvKeySource`, upstream URL normalization, SSRF/HTTPS/loopback/no-redirect guards (`S3`), `?key=` query stripping (preserving `?alt=sse`), and RFC 3986 path/header/API-key validation delegating to `companion-core::validation` |
| `src/gemini/tests.rs` | Unit tests for `GeminiProvider` URL/header validation, auth, streaming pass-through, and model discovery |
| `src/codex/mod.rs` & `src/codex/auth.rs` | OpenAI Codex OAuth 2.0 + PKCE S256 authentication (`CodexAuth`, `CodexAuthManager`), bounded loopback `127.0.0.1:1455` callback listener with cancellation/TTL, 4-form manual fallback parser, zero-crate Base64URL JWT decoder, and cross-process `.codex-refresh.lock` refresh serialization |
| `src/probe_client.rs` | Shared pooled `LoopbackProxyClient` (`.no_proxy()`, `.redirect(Policy::none())`, upfront `Content-Length` + streaming `64 KiB` body cap, zero-URL-leak `ProbeError`, `probe_health`, `probe_healthy_companion`, `probe_stats`, and `evaluate_health` status/version/protocol skew checks) |
