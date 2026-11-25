# `agy-companion` Milestone 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Follow our repo workflow (`AGENTS.md`): focused branch per milestone/slice → verify with `./scripts/agent_check.sh` + `cargo test` → open PR → squash-merge → fast-forward `main`.

**Goal:** Ship Milestone 1 of `agy-companion` — a standalone Rust binary delivering a byte-for-byte Gemini API pass-through proxy with implicit-cache observability, an embedded Antigravity MCP App (`aux_pane` + `global`) Settings UI, a validated Antigravity plugin bundle, and npm/PyPI binary-shim packaging scaffolds.

**Architecture:** A 5-crate Rust workspace (`companion-core`, `companion-providers`, `companion-proxy`, `companion-ui-mcp`, `companion-cli`) compiles to a single zero-dependency binary `agy-companion`. `agy-companion serve` runs the `axum` + `reqwest` (`rustls`) loopback proxy on `127.0.0.1:24141`, forwarding Gemini API requests untouched while tee-ing `usageMetadata` to rolling stats and a local JSONL `UsageSink`. `agy-companion ui-mcp` runs a stdio `rmcp` server embedding a Vite single-file MCP App HTML bundle via `include_str!`.

**Tech Stack:** Rust 1.96 (`tokio 1`, `axum 0.8`, `reqwest 0.13` + `rustls`, `rmcp 3.11`, `clap 4`, `serde`/`serde_json`, `chrono`, `uuid`, `thiserror`, `tracing`), Vite 8 + `@modelcontextprotocol/ext-apps 2` + `vite-plugin-singlefile` (build-time only), npm (`@agy-companion/<os>-<arch>`), PyPI (`maturin` `bindings = "bin"`), Antigravity plugin CLI (`agy plugin validate --strict`).

---

## File Structure Map

| Path | Responsibility |
| --- | --- |
| `Cargo.toml` | Workspace manifest and single version source of truth (`0.1.0`) |
| `crates/companion-core/src/{lib,config,fs_safe,protocol,validation,models,redaction,usage/{mod,sqlite},integration}.rs` | Core domain & safe FS primitives: nested v1 `Config` (`schema_version = 1`), `fs_safe` TOCTOU/atomic guards, shared `protocol` wire DTOs, URL/path/header `validation`, Gemini 3.8+/4+ `models` catalog, nested `RedactedConfig`, `UsageEvent` + `UsageSink` trait + `SqliteUsageSink` behind optional `sqlite` feature, and login-shell `integration` |
| `crates/companion-providers/src/{lib,gemini/{mod,url}}.rs` | `ProviderAdapter` trait + raw `reqwest` (`rustls`, 15-min timeout) Gemini streaming pass-through adapter (`x-goog-api-key` header) |
| `crates/companion-proxy/src/{lib,activity,hot_reload,cloudcode/*,routes/*,stats,sse}.rs` | `axum` loopback server (`127.0.0.1`), `GET /health`, `GET /api/stats`, `POST /v1beta/models/{model_action}` streaming pass-through + SSE usage tee |
| `ui/{package.json,vite.config.ts,index.html,src/main.ts,src/style.css}` | Build-time Vite single-file MCP App UI (strict DOM APIs, zero `innerHTML`, host CSS tokens, write-only API key form, cache telemetry, copyable `customModelsConfig`) |
| `crates/companion-ui-mcp/src/{lib,server}.rs` | `rmcp` stdio MCP App server: `ui://companion/settings.html` (`include_str!`), `open_settings` (`aux_pane` + `global`), redacted `get_settings`, eager `save_settings`, `test_connection` |
| `crates/companion-cli/src/main.rs` | Single binary `agy-companion`: `serve`, `ui-mcp`, `doctor`, `models` |
| `plugin/{plugin.json,README.md,mcp_config.json,scripts/run-companion.sh,sidecars/proxy/sidecar.json,skills/companion/SKILL.md}` | Distributable Antigravity plugin bundle (`agy plugin validate --strict ./plugin`) |
| `packages/npm/agy-companion/{package.json,bin/agy-companion.js}` & `packages/npm/generate-platform-pkgs.mjs` | npm meta-package (`@agy-companion/<os>-<arch>` `optionalDependencies`) + platform shim generator |
| `packages/python/{pyproject.toml,python/agy_companion/__init__.py}` | PyPI `maturin` (`bindings = "bin"`) config + Python launcher shim |
| `scripts/agent_check.sh` | Updated Gate 1: governance checks + version sync + `cargo test` + `agy plugin validate --strict ./plugin` |

---

### Task 1: Workspace Scaffold & `companion-core` (Config, Redaction, UsageMetadata, UsageSink)

**Files:**
- Create: `Cargo.toml`
- Create: `crates/companion-core/Cargo.toml`
- Create: `crates/companion-core/src/lib.rs`
- Create: `crates/companion-core/src/config.rs`
- Create: `crates/companion-core/src/redaction.rs`
- Create: `crates/companion-core/src/usage.rs`

- [ ] **Step 1: Create root `Cargo.toml` and `crates/companion-core/Cargo.toml`**

```toml
# Cargo.toml
[workspace]
resolver = "2"
members = [
  "crates/companion-core",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
repository = "https://github.com/Jacksunwei/agy-companion"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
```

```toml
# crates/companion-core/Cargo.toml
[package]
name = "companion-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
chrono.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
uuid.workspace = true

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write failing unit tests in `companion-core` covering config validation, secret redaction, Gemini `usageMetadata` extraction, and `JsonlUsageSink`**

Create `crates/companion-core/src/lib.rs`, `config.rs`, `redaction.rs`, and `usage.rs` with stub signatures and unit tests:
- `config::tests::rejects_non_loopback_bind_host` (asserts `"0.0.0.0"` fails validation; `"127.0.0.1"` succeeds)
- `config::tests::rejects_http_remote_upstream_allows_https_and_localhost` (asserts `"http://example.com"` fails; `"https://generativelanguage.googleapis.com"` and `"http://127.0.0.1:9999"` pass)
- `config::tests::defaults_to_schema_version_1`
- `redaction::tests::masks_api_key_without_leaking_middle` (`"AIzaSyD1234567890abcd"` -> `"AIza...abcd"`; short keys -> `"****"`; empty -> `None`)
- `usage::tests::parses_gemini_usage_metadata_with_cached_and_thoughts`
- `usage::tests::jsonl_sink_appends_without_body_content`

- [ ] **Step 3: Run `cargo test -p companion-core` and confirm tests fail on unimplemented stubs**

Run: `cargo test -p companion-core`
Expected: FAIL (`not yet implemented`).

- [ ] **Step 4: Implement `config.rs`, `redaction.rs`, and `usage.rs` minimally to go green**

Key types & behaviors:
- `Config` (`schema_version: u32` defaulting to `1`, `proxy: ProxyConfig { host: LoopbackHost, port: NonZeroU16 }` defaulting to `127.0.0.1:24141`, `cloudcode: CloudCodeConfig { upstream: CloudCodeUpstream }` defaulting to `"auto"`, `providers: ProvidersConfig { gemini: GeminiConfig { base_url, api_key: Option<ApiKey>, default_model, timeout_secs } }`, `usage_dir: Option<PathBuf>`).
- `Config::load_or_default(path)` and `Config::save_secure(path)` (creates parent dirs, writes atomically via temp file + rename, enforces mode `0600` on Unix).
- `RedactedConfig` (nested mirror of `Config`: `schema_version`, `proxy`, `cloudcode`, `providers.gemini` carrying `api_key_configured: bool` and `api_key_hint: Option<String>`, never plaintext `api_key`).
- `GeminiUsageMetadata::parse_from_json(&[u8]) -> Option<GeminiUsageMetadata>` extracting `promptTokenCount`, `candidatesTokenCount`, `cachedContentTokenCount`, `thoughtsTokenCount`.
- `UsageEvent`, `UsageSink` trait (`fn record(&self, event: UsageEvent)`), `NoopUsageSink`, and `JsonlUsageSink` writing monthly `YYYY-MM.jsonl` files with mode `0600`.

- [ ] **Step 5: Run `cargo test -p companion-core` and verify all tests pass**

Run: `cargo test -p companion-core`
Expected: PASS (`test result: ok. 6 passed; 0 failed`).

- [ ] **Step 6: Commit Task 1**

```bash
git add Cargo.toml Cargo.lock crates/companion-core
git commit -m "feat(core): add config validation, secret redaction, and usage sink seam"
```

---

### Task 2: `companion-providers` — Raw HTTP Gemini Streaming Pass-Through Adapter

**Files:**
- Modify: `Cargo.toml` (add `"crates/companion-providers"` + `reqwest`, `bytes`, `futures-util`, `http`)
- Create: `crates/companion-providers/Cargo.toml`
- Create: `crates/companion-providers/src/lib.rs`
- Create: `crates/companion-providers/src/gemini.rs`

- [ ] **Step 1: Define `ProviderAdapter` trait and `GeminiProvider` scaffold**

`ProviderAdapter` trait in `crates/companion-providers/src/lib.rs`:
- `fn name(&self) -> &'static str`
- `async fn forward_gemini(&self, req: ForwardRequest) -> Result<ForwardResponse, ProviderError>`
- `async fn health_check(&self) -> Result<(), ProviderError>`

`ForwardRequest`: `path_and_query: String` (validated to start with `/v1beta/models/` and contain no `..` or `key=` query secret), `body: Bytes`, `content_type: Option<String>`, `api_key_override: Option<String>`.
`ForwardResponse`: `status: u16`, `content_type: Option<String>`, `stream: ByteStream`.

- [ ] **Step 2: Write failing integration tests using an in-process mock `axum` upstream bound to `127.0.0.1:0`**

Tests in `crates/companion-providers/src/gemini.rs`:
- `forwards_exact_request_bytes_and_sets_x_goog_api_key_header` (verifies byte-for-byte body equality, `x-goog-api-key` header present, no `?key=` in URL)
- `streams_sse_chunks_without_mutating_payload` (verifies multi-chunk SSE passes through intact)
- `propagates_upstream_error_status_and_body` (e.g., upstream `429` or `401` returned cleanly without panicking)
- `rejects_path_traversal_in_model_path` (verifies `../` returns `ProviderError::InvalidPath`)

- [ ] **Step 3: Run `cargo test -p companion-providers` and confirm tests fail**

Run: `cargo test -p companion-providers`
Expected: FAIL.

- [ ] **Step 4: Implement `GeminiProvider` using `reqwest::Client` (`rustls-tls`, 900s / 15-min timeout)**

- Builds target URL from validated `base_url` + sanitized `/v1beta/models/...` path.
- Strips any incoming `key` query param if a caller sent one; sends key strictly in `x-goog-api-key` header (preferring configured key, falling back to caller header if configured key is unset).
- Streams `resp.bytes_stream()` directly without buffering the entire response.
- Never logs `x-goog-api-key`.

- [ ] **Step 5: Run `cargo test -p companion-providers` and verify green**

Run: `cargo test -p companion-providers`
Expected: PASS (`test result: ok. 4 passed; 0 failed`).

- [ ] **Step 6: Commit Task 2**

```bash
git add Cargo.toml Cargo.lock crates/companion-providers
git commit -m "feat(providers): implement raw HTTP Gemini streaming pass-through adapter"
```

---

### Task 3: `companion-proxy` — Axum Loopback Proxy, SSE Usage Tee & Live Stats

**Files:**
- Modify: `Cargo.toml` (add `"crates/companion-proxy"`, `axum`, `tower-http`)
- Create: `crates/companion-proxy/Cargo.toml`
- Create: `crates/companion-proxy/src/lib.rs`
- Create: `crates/companion-proxy/src/stats.rs`
- Create: `crates/companion-proxy/src/sse.rs`
- Create: `crates/companion-proxy/src/routes.rs`

- [ ] **Step 1: Write failing tests for SSE usage tee, rolling stats, `/health`, `/api/stats`, and proxy pass-through**

Tests:
- `sse::tests::extracts_usage_from_split_utf8_sse_chunks` (splits `data: {"usageMetadata":{...}}\n\n` across 2 TCP chunks; verifies exact bytes pass through untouched AND `GeminiUsageMetadata` is extracted)
- `stats::tests::computes_cache_hit_ratio_accurately`
- `routes::tests::health_returns_version_and_protocol_version` (verifies `200 OK`, `version == env!("CARGO_PKG_VERSION")`, `protocol_version == 1`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`)
- `routes::tests::proxies_stream_generate_content_and_records_usage_event` (mock Gemini upstream -> `companion-proxy` -> verifies exact SSE bytes, `UsageSink` received 1 `UsageEvent` with `cached_tokens`, and `GET /api/stats` reflects the request & hit rate)

- [ ] **Step 2: Run `cargo test -p companion-proxy` and confirm tests fail**

Run: `cargo test -p companion-proxy`
Expected: FAIL.

- [ ] **Step 3: Implement `sse.rs`, `stats.rs`, `routes.rs`, and `lib.rs`**

Routes (strict method allow-list: `GET` and `POST` only):
- `GET /health` → `{ "status": "ok", "version": "<pkg>", "protocol_version": 1 }`
- `GET /api/stats` → `{ "usage": { "requests": N, "prompt_tokens": ..., "cached_tokens": ..., "output_tokens": ..., "reasoning_tokens": ..., "cache_hit_ratio": ... }, "cloudcode": { "mode": ..., "resolved_upstream": ..., "last_catalog_refresh_at_ms": ..., "last_catalog_included_proxy_models": ... } }`
- `POST /v1beta/models/{*rest}` → byte-for-byte pass-through via `GeminiProvider` with streaming SSE tee (for `:streamGenerateContent`) or JSON parse tee (for `:generateContent`), recording `UsageEvent` asynchronously into `UsageSink` and updating `ProxyStats`.
- Enforce loopback-only `SocketAddr` (`127.0.0.1:<port>`) in `serve()`.
- Attach `X-Content-Type-Options: nosniff` and `Cache-Control: no-store` on `/health` and `/api/stats`.

- [ ] **Step 4: Run `cargo test -p companion-proxy` and verify green**

Run: `cargo test -p companion-proxy`
Expected: PASS (`test result: ok. 4 passed; 0 failed`).

- [ ] **Step 5: Commit Task 3**

```bash
git add Cargo.toml Cargo.lock crates/companion-proxy
git commit -m "feat(proxy): add loopback Gemini pass-through server with SSE usage tee"
```

---

### Task 4: `ui/` Frontend + `companion-ui-mcp` (`rmcp` Aux Panel & Global Settings UI)

**Files:**
- Create: `ui/package.json`
- Create: `ui/tsconfig.json`
- Create: `ui/vite.config.ts`
- Create: `ui/index.html`
- Create: `ui/src/main.ts`
- Create: `ui/src/style.css`
- Modify: `Cargo.toml` (add `"crates/companion-ui-mcp"`, `rmcp`, `schemars`)
- Create: `crates/companion-ui-mcp/Cargo.toml`
- Create: `crates/companion-ui-mcp/src/lib.rs`
- Create: `crates/companion-ui-mcp/src/server.rs`

- [ ] **Step 1: Build the Vite single-file MCP App frontend under `ui/`**

- Uses `@modelcontextprotocol/ext-apps` (`App`, `applyDocumentTheme`, `applyHostStyleVariables`) and `vite-plugin-singlefile` to output `ui/dist/index.html`.
- Registers `app.ontoolinput`, `app.ontoolresult`, and `app.onhostcontextchanged` **before** calling `await app.connect()`.
- Strictly uses `document.createElement`, `textContent`, and `replaceChildren()` — **zero** `innerHTML`, `outerHTML`, or `document.write`; **zero** `localStorage`; **zero** `alert()`/`confirm()`.
- Sections rendered:
  1. **Proxy Status & Version Skew Bar** (`Running`/`Stopped`, version, `127.0.0.1:24141`, cache hit rate %).
  2. **Gemini Settings Form** (write-only API key password input with masked hint badge, default model input, upstream base URL input, eager Save via `save_settings` + **Test Connection** via `test_connection`).
  3. **Antigravity CLI Setup Snippet** (copyable `customModelsConfig` JSON block for `~/.gemini/antigravity-cli/settings.json`).
  4. **Usage & Cost Tab Placeholder** (clean read-only card showing live session tokens + note that full `agy-companion usage` reports are coming next).

Run: `cd ui && npm install && npm run build`
Expected: `ui/dist/index.html` generated cleanly (< 500 KB, single self-contained HTML file).

- [ ] **Step 2: Write failing Rust unit tests for `companion-ui-mcp`**

Tests in `crates/companion-ui-mcp/src/server.rs`:
- `open_settings_tool_declares_aux_pane_and_global_entrypoints` (verifies `_meta.ui.resourceUri == "ui://companion/settings.html"` and `com.google.antigravity/ui.entrypoints` contains `aux_pane` and `global`)
- `ui_resource_serves_mcp_app_mime_and_non_empty_html` (`mime_type == "text/html;profile=mcp-app"`, length < 10 MiB)
- `get_settings_never_returns_plaintext_api_key` (saves a config with `"AIzaSySecret1234567890"`, calls `get_settings`, asserts serialized JSON does NOT contain `"AIzaSySecret1234567890"` and DOES contain `"AIza...7890"`)
- `save_settings_persists_eagerly_and_validates_inputs` (rejects invalid remote `http://` base URL; persists valid config with mode `0600`)

- [ ] **Step 3: Run `cargo test -p companion-ui-mcp` and confirm tests fail**

Run: `cargo test -p companion-ui-mcp`
Expected: FAIL.

- [ ] **Step 4: Implement `companion-ui-mcp` with `rmcp` (`server`, `macros`, `transport-io`)**

- Embeds `ui/dist/index.html` via `include_str!("../../../ui/dist/index.html")` (with optional `AGY_COMPANION_UI_HTML` env override for fast local UI dev iteration, validated against canonical file existence).
- Tools:
  - `open_settings` (accepts `{}`; `_meta` declares `ui.resourceUri = "ui://companion/settings.html"` + `com.google.antigravity/ui.entrypoints = [{type: "aux_pane", title: "AGY Companion"}, {type: "global", title: "AGY Companion"}]`; returns plain-text `content` summary + `structuredContent` redacted state).
  - `get_settings` (`_meta.ui.visibility = ["app"]`; returns redacted config + proxy `/health` & `/api/stats` snapshot).
  - `save_settings` (`_meta.ui.visibility = ["app"]`; validates & saves `Config` atomically at mode `0600`).
  - `test_connection` (`_meta.ui.visibility = ["app"]`; runs `GeminiProvider::health_check()` and returns status without leaking key).

- [ ] **Step 5: Run `cargo test -p companion-ui-mcp` and verify green**

Run: `cargo test -p companion-ui-mcp`
Expected: PASS (`test result: ok. 4 passed; 0 failed`).

- [ ] **Step 6: Commit Task 4**

```bash
git add Cargo.toml Cargo.lock ui crates/companion-ui-mcp
git commit -m "feat(ui-mcp): add embedded MCP App aux-pane settings UI and rmcp server"
```

---

### Task 5: `companion-cli` — Single Binary (`serve`, `ui-mcp`, `doctor`, `models`)

**Files:**
- Modify: `Cargo.toml` (add `"crates/companion-cli"`, `clap`, `tracing-subscriber`)
- Create: `crates/companion-cli/Cargo.toml`
- Create: `crates/companion-cli/src/main.rs`

- [ ] **Step 1: Write failing CLI tests (argument parsing + `models` snippet output + `doctor` skew check)**

Tests in `crates/companion-cli/src/main.rs`:
- `parses_serve_ui_mcp_doctor_and_models_subcommands`
- `models_command_emits_valid_custom_models_config_json` (verifies `gemini-api://http://127.0.0.1:24141/models/...` entries)
- `doctor_detects_version_skew_between_binary_and_health_response`

- [ ] **Step 2: Run `cargo test -p companion-cli` and confirm tests fail**

Run: `cargo test -p companion-cli`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/companion-cli/src/main.rs` (binary name `agy-companion`)**

Subcommands:
- `agy-companion serve [--port 24141] [--config <path>]` — starts loopback proxy with structured stderr logging (never prints secrets).
- `agy-companion ui-mcp [--data-dir <path>] [--config <path>]` — runs stdio `rmcp` server cleanly (stdout reserved strictly for JSON-RPC; all logs to stderr!).
- `agy-companion doctor` — checks config file permissions, proxy `/health` reachability, version match, and whether `GEMINI_API_KEY` / `GOOGLE_GEMINI_BASE_URL` are configured.
- `agy-companion models [--json]` — prints ready-to-paste `customModelsConfig` entries for `~/.gemini/antigravity-cli/settings.json`.

- [ ] **Step 4: Build & test workspace**

Run: `cargo test --workspace && cargo run -p companion-cli -- --version`
Expected: All workspace tests PASS and prints `agy-companion 0.1.0`.

- [ ] **Step 5: Commit Task 5**

```bash
git add Cargo.toml Cargo.lock crates/companion-cli
git commit -m "feat(cli): assemble agy-companion binary with serve, ui-mcp, doctor, and models"
```

---

### Task 6: Antigravity Plugin Bundle (`plugin/`) + `agy plugin validate --strict`

**Files:**
- Create: `plugin/plugin.json`
- Create: `plugin/README.md`
- Create: `plugin/mcp_config.json`
- Create: `plugin/scripts/run-companion.sh` (executable `0755`)
- Create: `plugin/sidecars/proxy/sidecar.json`
- Create: `plugin/skills/companion/SKILL.md`

- [ ] **Step 1: Author `plugin/plugin.json` matching `https://antigravity.google/schemas/v1/plugin.json`**

Include `name: "agy-companion"`, `displayName: "Antigravity Companion"`, `version: "0.1.0"`, SEO/ASO `description`, `suggestedPrompts` (3 crisp prompts), `repository`, `license: "Apache-2.0"`, and `keywords` (`["antigravity", "agy", "byom", "proxy", "gemini", "ollama", "usage", "cost"]`).

- [ ] **Step 2: Author `plugin/scripts/run-companion.sh`, `plugin/sidecars/proxy/sidecar.json`, `plugin/mcp_config.json`, `plugin/skills/companion/SKILL.md`, and `plugin/README.md`**

- `plugin/scripts/run-companion.sh`:
  1. Checks `command -v agy-companion` on `$PATH` → `exec agy-companion "$@"`
  2. Checks local build/cache binary in `${AGY_COMPANION_BIN:-}` or `${ANTIGRAVITY_EXECUTABLE_DATA_DIR:-${HOME}/.local/share/agy-companion}/bin/agy-companion` → `exec`
  3. Otherwise downloads release tarball for `$(uname -s)-$(uname -m)` from `https://github.com/Jacksunwei/agy-companion/releases/download/v<version>/...`, verifies SHA-256 against `checksums.txt`, extracts into cache dir, and `exec`s it.
- `plugin/sidecars/proxy/sidecar.json`: runs `../../scripts/run-companion.sh` with `args: ["serve"]`, `restart_policy: "on-failure"`, `display_name: "AGY Companion Proxy"`.
- `plugin/mcp_config.json`: server key `"companion"` launching `${PLUGIN_ROOT}/scripts/run-companion.sh` with `["ui-mcp", "--data-dir=${PLUGIN_DATA}"]`.
- `plugin/skills/companion/SKILL.md`: concise instructions for opening the Settings panel, configuring `customModelsConfig`, and running `agy-companion doctor`.

- [ ] **Step 3: Validate with `agy plugin validate --strict ./plugin` and `bash -n`**

Run: `chmod +x plugin/scripts/run-companion.sh && bash -n plugin/scripts/run-companion.sh && agy plugin validate --strict ./plugin`
Expected: Exits 0 with zero errors and zero warnings.

- [ ] **Step 4: Commit Task 6**

```bash
git add plugin
git commit -m "feat(plugin): add Antigravity plugin bundle with sidecar, MCP App, and skill"
```

---

### Task 7: Cross-Platform npm & PyPI Packaging Shims + Gate 1 Integration

**Files:**
- Create: `packages/npm/agy-companion/package.json`
- Create: `packages/npm/agy-companion/bin/agy-companion.js`
- Create: `packages/npm/generate-platform-pkgs.mjs`
- Create: `packages/python/pyproject.toml`
- Create: `packages/python/python/agy_companion/__init__.py`
- Modify: `scripts/agent_check.sh`

- [ ] **Step 1: Author npm meta-package (`agy-companion`) with `@agy-companion/<os>-<arch>` `optionalDependencies` (`Option B`)**

- `packages/npm/agy-companion/package.json`: `"version": "0.1.0"`, `"bin": { "agy-companion": "bin/agy-companion.js", "agyc": "bin/agy-companion.js" }`, `"optionalDependencies"` listing `@agy-companion/darwin-arm64`, `@agy-companion/darwin-x64`, `@agy-companion/linux-x64`, `@agy-companion/linux-arm64`, `@agy-companion/win32-x64`.
- `packages/npm/agy-companion/bin/agy-companion.js`: maps `(process.platform, process.arch)` via a strict hardcoded allow-list to `@agy-companion/<target>/bin/agy-companion(.exe)` and runs `execFileSync` (never shell `exec`).
- `packages/npm/generate-platform-pkgs.mjs`: generates per-target `package.json` manifests with `"os"` and `"cpu"` fields synced to workspace `Cargo.toml` version.

- [ ] **Step 2: Author PyPI `maturin` (`bindings = "bin"`) package config under `packages/python/`**

- `packages/python/pyproject.toml`: `build-backend = "maturin"`, `[tool.maturin] bindings = "bin"`, `manifest-path = "../../crates/companion-cli/Cargo.toml"`, version `"0.1.0"`, keywords covering `antigravity`, `agy`, `byom`, `proxy`, `gemini`, `ollama`, `usage`, `cost`.
- `packages/python/python/agy_companion/__init__.py`: exposes `find_binary()` helper locating the installed console binary.

- [ ] **Step 3: Upgrade `scripts/agent_check.sh` with version-sync check, `cargo test`, `bash -n` on `plugin/scripts/*.sh`, and `agy plugin validate --strict ./plugin`**

- Add Check 5: Verify `Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json`, and `packages/python/pyproject.toml` all share the exact same version string.
- Add Check 6: `cargo test --workspace` passes.
- Add Check 7: `agy plugin validate --strict ./plugin` passes (when `agy` is on `$PATH`).

- [ ] **Step 4: Run full `./scripts/agent_check.sh` and `node --check`**

Run: `node --check packages/npm/agy-companion/bin/agy-companion.js && node --check packages/npm/generate-platform-pkgs.mjs && ./scripts/agent_check.sh`
Expected: All 7 checks print `PASS` and exit 0.

- [ ] **Step 5: Commit Task 7**

```bash
git add packages scripts/agent_check.sh
git commit -m "chore(release): add npm/PyPI binary shims and wire workspace gates into agent_check"
```

---

### Task 8: End-to-End Verification & Security Checklist

- [ ] **Step 1: Local proxy smoke test with `agy-companion serve` + `agy-companion doctor`**

Run `agy-companion serve --port 14141` bound to `127.0.0.1`, verify `GET http://127.0.0.1:14141/health` and `agy-companion doctor`, and confirm clean shutdown.

- [ ] **Step 2: Live end-to-end check with local `agy` CLI (when `GEMINI_API_KEY` is available)**

Run with `GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:14141` against a real Gemini API key (or local mock upstream in automated tests) and verify:
- Request reaches `companion-proxy` on `/v1beta/models/...`
- Streaming SSE completes cleanly in `agy`
- `GET /api/stats` increments request count and reports `cached_tokens`
- Local JSONL file in `~/.local/share/agy-companion/usage/` appends 1 record containing zero prompt/response text and zero API key material.

- [ ] **Step 3: Security Verification Plan Execution**

Verify every security invariant from `docs/design/architecture.md` Section 7:
1. **Loopback binding:** Confirm `companion-proxy` refuses `0.0.0.0` and listens strictly on `127.0.0.1`.
2. **Secret redaction & zero leaks:** Confirm `get_settings` returns only masked hints; confirm logs, URLs, and JSONL usage files contain zero API key substrings; confirm config file permissions are `0600`.
3. **Frontend XSS & CSP:** Grep `ui/src/` for `innerHTML`, `outerHTML`, `document.write`, `eval`, `localStorage`, `alert(` — confirm **0 hits**.
4. **Command & path hygiene:** Confirm `bin/agy-companion.js` uses `execFileSync` with a hardcoded target allow-list; confirm `companion-providers` rejects `..` path traversal and remote `http://` URLs; confirm `run-companion.sh` verifies SHA-256 before executing downloaded binaries.
5. **Plugin validation:** `agy plugin validate --strict ./plugin` exits 0.

- [ ] **Step 4: Open PR, verify `./scripts/agent_check.sh` green, squash-merge, and fast-forward `main`**
