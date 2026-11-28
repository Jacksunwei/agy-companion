---
name: local-dev
description: Build agy-companion locally and deploy via scripts/dev-install.sh (compiled binary + embedded plugin via agy-companion install, no symlinks), iterate on the Sidecar Web UI via AGY_COMPANION_UI_HTML, handle stale proxy sidecar takeover, inspect proxy/sidecar logs, and run isolated live E2E tests with agy CLI.
---

# Local Antigravity App Development & Testing Runbook

Use this skill when building and deploying `agy-companion` locally into the Antigravity App, iterating rapidly on the embedded Sidecar Web UI, replacing a stale proxy sidecar PID, inspecting proxy/sidecar logs, or running isolated live E2E tests with `agy` CLI.

## 1. Prerequisites & Local Build

- Verified local toolchains: Rust (`cargo`, `rustc`, `clippy`, `rustfmt`), Node.js (`node`, `npm`), Python 3 (`python3`), and `agy` CLI.
- `ui/.npmrc` pins `registry=https://registry.npmjs.org/` so `npm` commands inside `ui/` bypass corporate registry overrides automatically.
- `companion-ui` embeds `ui/dist/index.html` at compile time via `include_str!("../../../ui/dist/index.html")`. Rebuild `ui/` before `cargo build` whenever baking UI changes into the Rust binary:

```bash
cd ui && npm ci && npm run typecheck && npm run build && cd ..
cargo build -p companion-cli

# Optional optimized release build:
cargo build --release -p companion-cli
```

## 2. Build & Deploy Locally into Antigravity App

Build the latest UI + Rust binary and deploy the compiled binary and embedded 6-file plugin bundle using `scripts/dev-install.sh` (delegates directly to `agy-companion install` — deploys real files with zero repo symlinks so `git pull` never causes plugin-vs-binary version skew, automatically migrates any legacy repo symlink, and restarts the running proxy whenever the binary bytes change):

```bash
./scripts/dev-install.sh              # builds UI + debug binary and runs `agy-companion install`
./scripts/dev-install.sh --release    # builds UI + release binary and runs `agy-companion install`
./scripts/dev-install.sh --no-build   # skip builds; deploy existing compiled binary
./scripts/dev-install.sh --dry-run    # preview build and install commands without touching disk
./scripts/dev-install.sh --uninstall  # delegate to `agy-companion uninstall` (supports --purge)
```

Alternatively, `cargo run -p companion-cli -- install` runs the installer directly from source (rebuild `ui/` first if UI files changed).

### How `plugin/scripts/run-companion.sh` Resolves Binaries

`plugin/scripts/run-companion.sh` resolves the native binary in strict 5-step order:

1. **`AGY_COMPANION_BIN`** env var (highest precedence; fails hard if set to a non-executable/non-regular file or to the launcher script itself).
2. **Legacy/manual symlinked checkout auto-detection** — if `~/.gemini/config/plugins/agy-companion` is manually symlinked to `<repo>/plugin`, follows symlinks on `$0` back to `<repo_root>` and checks `<repo_root>/target/{debug,release}/agy-companion` (preferring whichever is newer via `-nt`).
3. **Version-matched installed/cached binary** — checks `${XDG_DATA_HOME:-${HOME}/.local/share}/agy-companion/bin/agy-companion` first (so `agy-companion install` / `dev-install.sh` always beats an older same-version auto-download), then `${ANTIGRAVITY_EXECUTABLE_DATA_DIR}/bin/agy-companion`, verifying `.version` matches `plugin/plugin.json`.
4. **`command -v agy-companion`** on `$PATH` (with self-exec guard).
5. **HTTPS + SHA-256 verified GitHub Releases download** (`sha256sum` or `shasum -a 256`, traversal-checked tarball extraction, atomic swap into `ANTIGRAVITY_EXECUTABLE_DATA_DIR/bin` or XDG `bin`).

> **Same-version rebuild note:** `./scripts/dev-install.sh` restarts the running `:24141` proxy daemon whenever binary bytes change, even at the same version. However, an already-open Antigravity App standby Sidecar Web UI process keeps its in-memory baked HTML until you quit (`Cmd+Q`) and reopen Antigravity — or export `AGY_COMPANION_UI_HTML` for instant HTML reload below.

## 3. Aux Panel Surface & Instant UI Iteration

Antigravity Companion exposes its Aux Panel UI via the native **Sidecar Web UI**:

- Configured in `plugin/sidecars/proxy/sidecar.json` (`"has_web_ui": true`, `SIDECAR_UI_ENTRYPOINT_AUX_PANE`) and served by the proxy sidecar (`agy-companion serve` with `ANTIGRAVITY_SIDECAR_WEB_PORT` + `ANTIGRAVITY_SIDECAR_UI_TOKEN`). Open directly via `sidecar://agy-companion/proxy/`.
- `companion-ui` checks `AGY_COMPANION_UI_HTML` on **every** `GET /` request (validated regular UTF-8 file, `<= 10 MiB`, no `..` traversal or NUL bytes). Export it from the repo root for zero-Rust-rebuild UI iteration:

```bash
export AGY_COMPANION_UI_HTML="$(pwd)/ui/dist/index.html"
npm --prefix ui run build
```

Close and reopen the **Antigravity Companion** aux pane to load the rebuilt bundle immediately. UI changes baked into the binary take effect after rebuilding `ui/` + `companion-cli` and restarting the proxy sidecar (see Section 4).

## 4. Reloading Config & Replacing a Stale Proxy Sidecar PID

- `api_key` (persisted in sibling `0700`/`0600` `auth/gemini.json`), `base_url`, `default_model`, and `enabled_models` hot-reload in the running proxy via `config.json` and `auth/gemini.json` mtime without restarting the proxy.
- Rust binary updates, baked Sidecar Web UI updates, or `listen_port` changes require restarting the `proxy` sidecar.
- When an older `agy-companion` proxy holds port `24141`, a newly spawned sidecar enters monitor mode (`detected running agy-companion proxy; monitoring until shutdown or takeover`) until the stale PID exits. Execute this directly during local debugging rather than asking the user to run `kill`:

```bash
# Inspect current running version and listening PID
curl -s http://127.0.0.1:24141/health
lsof -nP -iTCP:24141 -sTCP:LISTEN

# Terminate the stale proxy so the newly built sidecar takes over automatically
kill <STALE_PID>
sleep 1
curl -s http://127.0.0.1:24141/health
```

## 5. Logs, CloudCode Routing Diagnostics & Standalone Proxy Run

- **Proxy routing & request log (primary trace for CloudCode/Mendel debugging):**
  ```bash
  tail -n 100 ~/.local/share/agy-companion/proxy.log
  ```
  Check `proxy.log` to verify upstream CloudCode selection (`cloudcode-pa.googleapis.com` for PayGo/enterprise vs `daily-cloudcode-pa.googleapis.com` for consumer). Misrouted `POST /v1internal:listExperiments` calls can silently disable Mendel flags (including plugin UI visibility).
- **Antigravity App sidecar logs:**
  ```bash
  ls -la ~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/
  ```
- **Foreground standalone proxy** (logs strictly to stderr; `hyper`, `hyper_util`, `h2`, and `reqwest` tracing are hard-capped at `INFO` to protect secrets and tokens):
  ```bash
  cargo run -p companion-cli -- serve

  # Optional verbose proxy debug logs:
  RUST_LOG=companion_proxy=debug,companion_providers=debug cargo run -p companion-cli -- serve
  ```
- **Probe endpoints directly:**
  ```bash
  curl -s http://127.0.0.1:24141/health
  curl -s http://127.0.0.1:24141/api/stats
  ```

## 6. Live E2E Testing with `agy` CLI & XDG Isolation

Use isolated temporary `XDG_CONFIG_HOME` / `XDG_DATA_HOME` directories (absolute paths, no `..`) and an ephemeral port so personal config and usage database remain untouched. When `GEMINI_API_KEY` is already exported in the developer's shell environment, run live `agy -p` E2E verification directly (never echo, log, or commit the key) instead of asking the user to run a manual test turn:

```bash
TMP_DEV="$(mktemp -d)"
export XDG_CONFIG_HOME="${TMP_DEV}/config"
export XDG_DATA_HOME="${TMP_DEV}/data"

./target/debug/agy-companion serve --port 14141 &
PROXY_PID=$!

./target/debug/agy-companion doctor --port 14141 --json
./target/debug/agy-companion models --json --port 14141
GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:14141" agy -p "Reply with OK"
curl -s http://127.0.0.1:14141/api/stats
ls -la "${XDG_DATA_HOME}/agy-companion/usage/"
sqlite3 "${XDG_DATA_HOME}/agy-companion/usage/usage.db" "SELECT timestamp_utc, model, input_tokens, output_tokens, cached_tokens FROM usage_events ORDER BY id DESC LIMIT 5;"

kill "${PROXY_PID}"
rm -rf "${TMP_DEV}"
```

On the default port (`24141`), use `GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:24141"`, `agy-companion doctor`, and `agy-companion models --json`.

## 7. Safety & Secret Hygiene

- **Never** commit, log, echo, or embed a real `GEMINI_API_KEY` (`AIza...`) in any repo file, test fixture, commit message, or PR body. `./scripts/agent_check.sh` scans tracked files, staged index, commit diffs, and commit messages for `AIza[0-9A-Za-z_-]{35}`.
- **Never** auto-mutate `~/.gemini/config/config.json` or `~/.gemini/antigravity-cli/settings.json` from repo scripts. Generate CLI model snippets via `agy-companion models --json` for explicit user review and copy/paste.
