# `companion-proxy`

Loopback-only Axum HTTP proxy that routes Gemini API requests to your API key while transparently bridging Antigravity's CloudCode RPCs in Hybrid Mode.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | `#![forbid(unsafe_code)]`, `ProxyError`, `ProxyState` (`new -> Result`, infallible `with_cloudcode` DI constructor, `from_config`, `from_config_with_options`, `from_config_with_codex_auth`), `ProxyStateOptions` (including optional `role`), loopback listener binding & re-verification (`S1`), and `serve*` entrypoints |
| `src/activity.rs` | `ActivityTracker` and `InFlightGuard` tracking active in-flight requests and idle duration for `--idle-shutdown-secs` (ignoring passive `/health` and `/api/stats` probes) |
| `src/hot_reload.rs` | Non-blocking async `HotReloadProvider` (`tokio::sync::Mutex` + `spawn_blocking` outside `RwLock`, zero stat debounce, `companion-core::fs_safe` TOCTOU `(dev, ino)` & symlink/FIFO safety watching `config.json`, `auth/gemini.json`, and `auth/codex.json`, preserving `CloudCodeClient` LRU and `CodexProvider` when unchanged) |
| `src/cloudcode/mod.rs` | `MAX_CLOUDCODE_CATALOG_BYTES` (`16 MiB`), `CloudCodeError`, `CloudCodeStreamResponse`, `CloudCodeBufferedResponse`, and public CloudCode re-exports |
| `src/cloudcode/routing.rs` | `AccountRoute`, 8-entry 128-bit SipHash-1-3 `CloudCodeRoutingCache` LRU, request/response header allowlists, `is_cloudcode_path`, and RFC 3986 `validate_cloudcode_path` delegating to `companion-core::validation` |
| `src/cloudcode/client.rs` | `CloudCodeClient` (`auto`/`fixed` upstream routing, conditional `.no_proxy()` on loopback, `.http1_only()`, `>=600s` stream read timeout, `.without_url()` error sanitization), `forward_raw`, and `forward_fetch_available_models` |
| `src/cloudcode/metadata.rs` | Unified `execute_buffered_metadata_rpc` driving `loadCodeAssist`, `listExperiments`, and `fetchAvailableModels` daily↔prod fallback and metadata-only Enterprise `403` `x-goog-user-project` strip retry |
| `src/cloudcode/slots.rs` | `ThinkingVariant` (`Low`/`Medium`/`High`/`XHigh`), `CURATED_SLOT_TABLE` (`M334..=M350` Gemini, `M351..=M364` Codex, `31/32` slots), deterministic `MODEL_PLACEHOLDER_M334..=M365` slot assignment, and multi-provider `default_model`-priority slot reservation |
| `src/cloudcode/catalog.rs` | `pretty_model_name`, `merge_available_models`, `merge_available_models_multi_provider`, upstream Gemini template inheritance with derived `modelExperiments` compaction window override (`85%` of `maxTokens - maxOutputTokens`), and explicit Codex metadata (`supportsThoughtCirculation: true`, `1.05M` context, `modelExperiments` compaction window) |
| `src/routes/mod.rs` | `MAX_PROXY_BODY_BYTES` (`64 MiB`), `build_router` (`/health`, `/api/stats`, `/v1beta/models`, `/v1beta/models/{*rest}`, `/v1internal:fetchAvailableModels`, `/v1internal:*` fallback), `json_error_response`, and `S7` security headers |
| `src/routes/middleware.rs` | `track_request_activity`, `enforce_get_post_only` (`405`), upfront `enforce_content_length_limit` (`400`/`413`), streaming `read_bounded_request_body` (`413` strictly on overflow, `400` on disconnect/read error), and inbound API-key/content-type validation |
| `src/routes/internal.rs` | `GET /health` (with optional `role`), `GET /api/stats`, and `GET /v1beta/models` handlers |
| `src/routes/gemini.rs` | `POST /v1beta/models/{*rest}` multi-provider dispatch (`is_supported_gemini_model` → `GeminiProvider` with Gemini-only thought-signature retry vs `is_codex_model` → `CodexProvider`), pre-translation `conversation_id` extraction, and SSE/JSON usage tee wiring (`UsageOrigin::{GeminiProxy,CodexProxy}`) |
| `src/routes/cloudcode.rs` | `POST /v1internal:fetchAvailableModels` (multi-provider `gemini_ready \|\| codex_ready` gating; Day-0 untouched pass-through vs merged `(AI Studio)`/`(Codex)` catalog) and `/v1internal:*` raw streaming fallback (`TrackedStream`) |
| `src/routes/thought_sig.rs` | `400 INVALID_THOUGHT_SIGNATURE` retry-strip fallback with strict `64 KiB` `sniff_error_prefix` cap and zero-copy `leftover` byte-stream replay |
| `src/routes/tee.rs` | `UsageTeeMode`, `UsageCollector` (`DEFAULT_MAX_JSON_TEE_BYTES = 8 MiB`), `TrackedStream`, and `UsageTeeStream` |
| `src/sse.rs` | Zero-buffering SSE/JSON stream tee that extracts `usageMetadata` without mutating forwarded bytes (`S4`) |
| `src/stats.rs` | In-memory atomic telemetry counters and CloudCode/catalog status backing `/api/stats` |
| `src/{tests,cloudcode/tests,routes/tests}.rs` | Unit tests covering `ProxyState` constructors/options, non-blocking hot-reload, `ActivityTracker`, CloudCode routing/slots/catalog, and route middleware/thought-signature/body-limit edge cases |
