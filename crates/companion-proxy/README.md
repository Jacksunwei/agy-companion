# `companion-proxy`

Loopback-only Axum HTTP proxy that routes Gemini API requests to your API key while transparently bridging Antigravity's CloudCode RPCs in Hybrid Mode.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | Loopback listener binding (`S1`), live config hot-reload watcher (with symlink/FIFO safety), and `IdleTracker` for on-demand daemon shutdown |
| `src/routes.rs` | HTTP router (`/health`, `/api/stats`, `/v1beta/models`, `/v1beta/models/{*rest}`, `/v1internal:*`), upfront `64 MiB` `Content-Length` enforcement (`S7`), `400 INVALID_THOUGHT_SIGNATURE` retry-strip fallback, and security response headers |
| `src/cloudcode.rs` | CloudCode client (`auto`/`fixed` upstream routing, per-account 128-bit SipHash LRU, Enterprise `403` metadata retry), Day-0 untouched catalog, capability-ordered `(Proxy)` catalog merging with deterministic `M334..=M365` slots, and upstream compaction metadata inheritance |
| `src/sse.rs` | Zero-buffering SSE/JSON stream tee that extracts `usageMetadata` without mutating forwarded bytes (`S4`) |
| `src/stats.rs` | In-memory atomic telemetry counters and CloudCode/catalog status backing `/api/stats` |
