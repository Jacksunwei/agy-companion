---
name: local-dev
description: Build agy-companion locally, link ./plugin into Antigravity App with scripts/dev-install.sh, iterate on the MCP App UI via AGY_COMPANION_UI_HTML, live-reload MCP servers without restarting Antigravity, inspect sidecar logs, and run isolated live E2E tests with agy CLI.
---

# Local Antigravity App Development & Testing Runbook

Use this skill when building `agy-companion` locally, linking `./plugin` into the Antigravity App, iterating rapidly on the embedded MCP App UI, reloading the MCP server without restarting Antigravity, inspecting proxy sidecar logs, or running isolated live E2E tests with `agy` CLI.

## 1. Prerequisites & Local Build

- Verified local toolchains: Rust (`cargo`, `rustc`, `clippy`, `rustfmt`), Node.js (`node`, `npm`), Python 3 (`python3`), and `agy` CLI.
- `ui/.npmrc` pins `registry=https://registry.npmjs.org/` so `npm` commands inside `ui/` bypass corporate registry overrides automatically.
- `companion-ui-mcp` embeds `ui/dist/index.html` at compile time via `include_str!("../../../ui/dist/index.html")`. Rebuild `ui/` before `cargo build` whenever baking UI changes into the Rust binary:

```bash
cd ui && npm ci && npm run typecheck && npm run build && cd ..
cargo build -p companion-cli

# Optional optimized release build:
cargo build --release -p companion-cli
```

## 2. Link `./plugin` into Antigravity App & Binary Resolution

Link `./plugin` into `~/.gemini/config/plugins/agy-companion` using the dev helper (safe symlink only; never overwrites a real directory and never touches user Antigravity settings):

```bash
./scripts/dev-install.sh              # builds UI + debug binary and symlinks ./plugin
./scripts/dev-install.sh --release    # builds UI + release binary and symlinks ./plugin
./scripts/dev-install.sh --no-build   # symlink only (skip UI and Cargo builds)
./scripts/dev-install.sh --dry-run    # preview actions without touching disk
./scripts/dev-install.sh --uninstall  # safely remove symlink if it points to this repo
```

Alternatively, `agy plugin install ./plugin` installs the plugin through the Antigravity CLI (note that linking via `./scripts/dev-install.sh` keeps the plugin pointing directly at your working checkout so local Cargo builds are picked up automatically).

### How `plugin/scripts/run-companion.sh` Resolves Binaries

`plugin/scripts/run-companion.sh` resolves the native binary in strict 5-step order:

1. **`AGY_COMPANION_BIN`** env var (highest precedence; fails hard if set to a non-executable/non-regular file or to the launcher script itself).
2. **Linked dev checkout auto-detection** — follows symlinks on `$0` back to `<repo_root>` and checks `<repo_root>/target/debug/agy-companion` and `<repo_root>/target/release/agy-companion` (preferring whichever is newer via `-nt`). Because of this step, a linked dev checkout **never downloads from GitHub Releases**.
3. **`command -v agy-companion`** on `$PATH` (with self-exec guard).
4. **Cached binary** at `${ANTIGRAVITY_EXECUTABLE_DATA_DIR:-${HOME}/.local/share/agy-companion}/bin/agy-companion` (verifying `.version` matches `plugin/plugin.json` when `.version` is present).
5. **HTTPS + SHA-256 verified GitHub Releases download** (`sha256sum` or `shasum -a 256`, traversal-checked tarball extraction, atomic swap).

## 3. Instant UI Iteration (`AGY_COMPANION_UI_HTML`, Zero Rust Rebuild)

`companion-ui-mcp` checks the `AGY_COMPANION_UI_HTML` environment variable on **every** `resources/read` request for `ui://companion/settings.html` (validated regular UTF-8 file, `<= 10 MiB`, no `..` traversal or NUL bytes).

From the repo root, export `AGY_COMPANION_UI_HTML` in the environment launching `ui-mcp`:

```bash
export AGY_COMPANION_UI_HTML="$(pwd)/ui/dist/index.html"
```

Then iterate on `ui/src/` with zero Rust recompilation:

```bash
npm --prefix ui run build
```

Close and reopen the **Antigravity Companion** aux pane to load the rebuilt bundle immediately.

## 4. Live Reload MCP App Server Without Restarting Antigravity App

The plugin `"agy-companion"` combined with server key `"hub"` in `plugin/mcp_config.json` registers in Antigravity App as MCP server **`agy-companion_hub`**.

- **UI method (recommended):** Open **Settings > MCP Servers > `agy-companion_hub` > Restart**.
- **RPC method:** Invoke the `RefreshMcpServers` RPC with payload:
  ```json
  {"serverName": "agy-companion_hub"}
  ```
- **What reloads when:**
  - Rust binary changes take effect on MCP server restart (via `run-companion.sh` Step 2).
  - `api_key`, `base_url`, and `default_model` hot-reload in the running proxy via config mtime without restarting the proxy.
  - Proxy binary updates or `listen_port` changes require restarting the `proxy` sidecar.

## 5. Sidecar Logs & Standalone Proxy Run

- **Antigravity App sidecar logs:**
  ```bash
  ls -la ~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/
  ```
- **Foreground standalone proxy** (logs strictly to stderr; `rmcp` tracing is hard-capped at `INFO` to protect secrets):
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

Use isolated temporary `XDG_CONFIG_HOME` / `XDG_DATA_HOME` directories (absolute paths, no `..`) and a non-default port so personal config and usage logs remain untouched:

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

kill "${PROXY_PID}"
rm -rf "${TMP_DEV}"
```

On the default port (`24141`), use `GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:24141"`, `agy-companion doctor`, and `agy-companion models --json`.

## 7. Safety & Secret Hygiene

- **Never** commit, log, echo, or embed a real `GEMINI_API_KEY` (`AIza...`) in any repo file, test fixture, commit message, or PR body. `./scripts/agent_check.sh` scans tracked files, staged index, commit diffs, and commit messages for `AIza[0-9A-Za-z_-]{35}`.
- **Never** auto-mutate `~/.gemini/config/config.json` or `~/.gemini/antigravity-cli/settings.json` from repo scripts. Generate CLI model snippets via `agy-companion models --json` for explicit user review and copy/paste.
