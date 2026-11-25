# Antigravity Companion — UI & CUJ Design Spec (v2)

> **Status:** Locked design contract for the next UI/UX + config/telemetry iteration.
> **Scope:** MCP App UI (`ui/`), semantic MCP snapshot/tool contract (`companion-ui-mcp`), additive config/launcher/usage queries (`companion-core`, `companion-proxy`, `companion-cli`), and open-source copy hygiene.

---

## 1. Core Product Principles

1. **Zero-friction defaults:** Ship defaults that let a user paste an API key and start using models immediately without understanding proxy internals, ports, dotfiles, or account endpoints.
2. **Every control earns its pixels:** No internal codenames, no raw backend URLs, no read-only/unusable fields, no fake "Coming Soon" placeholders, and no manual Refresh button for state the app can sync automatically.
3. **Calm single-column instrument:** One centered single-column layout (`max-width: 560px`) across narrow Antigravity aux panes (`360px`–`460px`) and wide global settings pages (`760px+`), with 3 compact icon tabs (**Monitor** | **Usage** | **Settings**) and a minimal pinned status footer.
4. **Open-source clean:** Zero Google-internal codenames or raw `*-pa.googleapis.com` endpoints anywhere in user-facing UI strings.

---

## 2. Primary User CUJs

| CUJ | User Goal | Flow in UX v2 |
| --- | --- | --- |
| **CUJ 1 — Day-0 First Run** | Install plugin, paste API key, get latest Gemini 4 models in Antigravity with minimal steps | Plugin auto-enables the local proxy launch hook once on first start (respecting symlinked dotfiles and explicit opt-outs). UI opens on **Settings** with a top platform-aware banner (`Quit (⌘Q) and reopen Antigravity` on macOS). User clicks `Get API key ↗` (opens `https://aistudio.google.com/api-keys` via CSP-safe `app.openLink`), pastes key, clicks **Save**. **`4 Carbon`**, **`4 Neon`**, and **`4 Hydrogen`** are already enabled by default. |
| **CUJ 2 — Model Selection** | Control which proxy models appear in Antigravity's model picker and know which to pick | Inside the **Google AI Studio** card, multi-select **`Enabled models`** chips let the user toggle individual models ON/OFF with instant persistence and clamped hover/focus tooltips explaining what each model is best for. |
| **CUJ 3 — Live Session Monitoring** | Verify cache hits, prompt size, call health, and estimated session cost while coding | Once configured, UI defaults to the **Monitor** tab: live active-model card with latest prompt context/cache bar, call health dot strip (`Last N calls · no breaks`), 3 recessed `.tile` cells (`Session cache`, `Input`, `Output`), session estimated list price bar, and **Usage by model**. |
| **CUJ 4 — Multi-Day Usage & Cost Tracking** | Inspect spend and token trends across days, models, and providers | **Usage** tab (backed by local SQLite `usage.db`) provides `Today` \| `7 days` \| `30 days` filters, `$` \| `Tokens` toggle, hero total + cache savings, stacked daily bar chart, and **Model** \| `Provider` ranked breakdown. |
| **CUJ 5 — Multi-Backend Expansion** | Add future non-Google backends (e.g., OpenAI Codex / GPT) without redesigning the panel | **Settings** uses a modular `.provider-stack` where each provider card owns its credentials and its own `Enabled models` strip. Production ships only live providers (**Google AI Studio** today); future **OpenAI Codex** drops in as a sibling card. |
| **CUJ 6 — Safe Opt-Out & Recovery** | Temporarily disable the local proxy hook or fix account plan detection | Collapsed **Advanced** holds only **Antigravity plan** (`Auto` / `Personal` / `Organization`) and a discouraged right-aligned **`Disable proxy`** action (warning + 2-click confirmation). When disabled, every subsequent open surfaces a top **`Local proxy is disabled`** banner with one-click **`Re-enable proxy`** + quit-and-reopen prompt. |

---

## 3. Single-Column 3-Tab Architecture

Every viewport renders the exact same single-column DOM structure centered at `max-width: 560px` — no 2-column breakpoint.

```text
+---------------------------------------------------------------+
| [mark] Antigravity Companion 0.1.0        [pulse][bars][gear] | <- Pinned Header (44px)
+---------------------------------------------------------------+
|                      max-width: 560px                         |
|  +---------------------------------------------------------+  |
|  | [!] Quit (⌘Q) and reopen Antigravity                    |  | <- Conditional Top Banner
|  |     The local proxy is enabled. Quit (⌘Q) and reopen    |  |    (at most 1 active;
|  |     Antigravity to use your enabled models.             |  |     platform-aware copy)
|  +---------------------------------------------------------+  |
|                                                               |
|  [Monitor tab]  Live session card (active model, latest       |
|                 prompt bar, call health strip, 3 .tile cells, |
|                 est. list price bar) + Usage by model         |
|                                                               |
|  [Usage tab]    Today|7d|30d + $|Tokens, hero total, stacked  |
|                 daily bar chart, Model|Provider breakdown     |
|                                                               |
|  [Settings tab] MODEL PROVIDERS                               |
|  +---------------------------------------------------------+  |
|  | Google AI Studio          [● AQ.A…_J5w] Get API key ↗   |  | <- Provider Card 1
|  | [ Paste new key to replace                            ] |  |
|  | [ Save ]  [ Test ]                              Clear   |  |
|  | ------------------------------------------------------- |  |
|  | Enabled models                                3 enabled |  |
|  | [✓ 4 Carbon] [✓ 4 Neon] [✓ 4 Hydrogen]                  |  | <- Multi-select chips
|  | [  4 Argon ] [  3.8 Flash ]                             |  |    + clamped tooltips
|  +---------------------------------------------------------+  |
|  | ▸ Advanced                            Plan · Local proxy | <- Collapsed by default
|  +---------------------------------------------------------+  |
|    (Expanded: Antigravity plan [Auto/Personal/Organization],  |
|               Local proxy [Active]          [Disable proxy]   |
|               "Not recommended — Antigravity needs...")       |
|                                                               |
+---------------------------------------------------------------+
|  ● Live                                            18 MB RAM  | <- Pinned Footer (36px)
|  ^ hover/focus/click health popover         RAM (when live)   |
+---------------------------------------------------------------+
```

### Header & Tab Landing Rules
- **3 icon tabs (`role="tablist"`):**
  1. **Monitor** (pulse icon) — live current-session view.
  2. **Usage** (bar-chart icon) — historical SQLite `usage.db` view.
  3. **Settings** (gear icon) — providers + collapsed Advanced. Shows a subtle amber attention dot when not active and any action is needed (API key unset, proxy disabled, manual symlink step, or reopen required).
- **CUJ-adaptive landing:** Opens on **Monitor** whenever `hybrid_active` is true (even without a Google AI Studio API key configured) or `api_key_configured` is true; opens on **Settings** on Day-0 before Hybrid Mode is active and no API key is configured (`!hybrid_active && !api_key_configured`).

### Responsive Padding (`max-width: 560px` centered)
- **Narrow aux pane (`320px–400px`):** `padding: 12px`; chips wrap naturally (`gap: 6px`); tooltips clamp within `[10px, width - tipWidth - 10px]`.
- **Standard aux pane (`401px–600px`):** `padding: 14px 16px`.
- **Wide global view (`> 600px`):** `max-width: 560px; margin: 0 auto; padding: 18px 16px` (matches Antigravity's native single-column Account settings page).

---

## 4. Tab-by-Tab Detailed Specification

### 4.1 Top Priority Banner Queue (At Most 1 Visible)

Rendered above the active tab's content whenever attention is needed, in strict priority order:

| Priority | Condition | Banner Title | Banner Body & Action |
| --- | --- | --- | --- |
| 1 | `manual_required === true` (symlinked/read-only shell profile, S8) | **One manual step needed** | `Your shell profile is symlinked. Add this line, then quit and reopen Antigravity:` + copyable snippet + **Copy** & **Re-check** buttons. *(99% of users never see this.)* |
| 2 | `!proxy.reachable` | **Local proxy offline** | `Retrying automatically in background. If this persists, quit and reopen Antigravity.` *(macOS adds `(⌘Q)`.)* |
| 3 | `!launcher.enabled` (Unix explicit opt-out) | **Local proxy is disabled** | `Your enabled models won't appear in Antigravity. Re-enable the proxy, then quit and reopen Antigravity.` + inline **`Re-enable proxy`** button (one click re-enables and transitions to Priority 4). |
| 4 | `launcher.enabled && !hybrid_active` (first run or just re-enabled) | **macOS:** `Quit (⌘Q) and reopen Antigravity` / **Linux:** `Quit and reopen Antigravity` | `The local proxy is enabled. Quit and reopen Antigravity to use your enabled models.` *(macOS includes `(⌘Q)`.)* |
| 4 (Win) | `platform === "windows" && !hybrid_active` | **Reopen Antigravity via Companion shortcut** | `Close Antigravity and launch it from the "Antigravity (Companion)" Start Menu shortcut to use your enabled models.` |

### 4.2 Tab 1 — Monitor (Live Session)

- **Idle states (`requests === 0`):** Never asks the user to add a Google AI Studio API key and never shows an **`Open Settings`** button (live monitoring works for built-in Antigravity models even without an API key). No fake `0 / 0 / 0` tiles:
  - **`!hybrid_active`:** Single calm card titled **`Waiting for Antigravity to connect`** explaining to quit and reopen Antigravity (platform-aware: `⌘Q` on macOS only; Start Menu `"Antigravity (Companion)"` shortcut on Windows) so requests route through the local proxy, noting that live monitoring works for built-in Antigravity models even without an API key.
  - **`hybrid_active`:** Single calm card titled **`Waiting for requests`** explaining to send a message in Antigravity to see live context usage, cache hit health, and cost estimates (for built-in Antigravity models as well as your own API keys).
- **Active state (`requests > 0`):** Tracks both built-in Antigravity models (`provider = "antigravity"`, `source = "builtin"`) passively teed on `/v1internal:*` pass-through and BYOM models (`provider = "gemini"`, `source = "proxy"`).
  1. **Live Session Card:**
     - Header: Provider badge (`Antigravity` built-in / `Gemini` BYOM / future `Codex`) + latest active model + latest call time (`HH:MM`).
     - **Latest prompt bar:** Hero input token count (`148K of 1M` · `15%`) + proportional stacked bar (**from cache** green · **new input** amber · **output** blue) + concise legend.
     - **Call health strip:** `Last N calls` + compact pill strip (green = cache hit, amber = cold prompt / cache break) + summary (`no breaks` or `N breaks`).
     - **3 borderless recessed `.tile` cells (`tabular-nums`):**
       - `Session cache`: overall cache hit % + `latest XX%`
       - `Input`: total session input tokens + `N calls`
       - `Output`: total session output tokens + `incl. XK think`
     - **Session cost estimate footer:** `This session · est. list price` → `≈ $X.XX` + proportional cost bar (`read from cache` · `output` · `new input`).
  2. **Usage by model Card (current session):**
     - Ranked list of models used in this session (provider tag, model name, `≈ $X.XX · YY%`, horizontal proportional bar, `N calls · X in (Y% cache) · Z out`).

### 4.3 Tab 2 — Usage (Historical Analytics via SQLite `usage.db`)

Backed directly by `crates/companion-core/src/usage/sqlite.rs` (`usage_events` table: `timestamp_utc`, `provider`, `source`, `model`, `input_tokens`, `output_tokens`, `cached_tokens`, `reasoning_tokens`, `latency_ms`), recording both built-in Antigravity models (`provider = "antigravity"`, `source = "builtin"`) and BYOM models (`provider = "gemini"`, `source = "proxy"`).

1. **Filter row:** Range segmented control (**`Today`** \| **`7 days`** \| **`30 days`**) + unit toggle (**`$` \| `Tokens`**).
2. **Summary & Daily Chart Card:**
   - Hero total (`≈ $38.40` `est. list price` or `142M` `total tokens`) + date anchor (`since Nov 19`).
   - Summary line: `142M tokens in · 2.1M out · 612 calls · 89% cached (≈ $29 saved)`.
   - Proportional breakdown bar (`read from cache` · `output` · `new input`).
   - **Stacked daily bar chart** with peak callout (`Peak Nov 23 · ≈ $8.20`) and date axis.
3. **Breakdown Card:**
   - Segmented switcher (**`Model`** \| **`Provider`**).
   - Ranked rows with proportional horizontal bars, estimated cost or token total, calls, cache %, and average latency.

### 4.4 Tab 3 — Settings

#### Section A — `Model providers` (`.provider-stack`)
- **`Google AI Studio` Card:**
  - Header: **`Google AI Studio`** + masked status pill (`● AQ.A…_J5w` green when configured, muted `Not set` when unset) + right-aligned **`Get API key ↗`** button.
  - **`Get API key ↗` link handling (S5 CSP-safe):** Calls `@modelcontextprotocol/ext-apps` `app.openLink({ url: "https://aistudio.google.com/api-keys" })`; falls back to clipboard copy + inline `"Copied aistudio.google.com/api-keys"` toast if the host lacks `openLinks`.
  - **Write-only API key row (S2):** Password input (`Paste Google AI Studio API key` / `Paste new key to replace`), **`Save`** (enabled when non-empty), **`Test`** (enabled when saved & no unsaved draft), and right-aligned **`Clear`** text action (when configured; opens inline 2-click `Confirm clear` / `Cancel` — never `window.confirm()`).
  - **Divider + `Enabled models` multi-select chips:**
    - Interactive `<button type="button" role="checkbox" aria-checked>` chips with instant persistence (`save_settings({ enabled_models })`) and viewport-clamped accessible tooltips (`mouseenter` / `focus`, dismissed on `mouseleave` / `blur` / `Escape`):

    | Chip | Model ID | Default | Tooltip (`Best for ...`, verified against official Gemini API docs) | Meta Line |
    | --- | --- | --- | --- | --- |
    | **`4 Carbon`** | `gemini-4-carbon` | **ON** | Best for deep verification, hard debugging, security audits, and long-horizon tasks. | Strongest reasoning · Deliberate · Paid API tier |
    | **`4 Neon`** | `gemini-4-neon` | **ON** | Best for complex multi-file coding, refactoring, and analysis with faster turnaround than Carbon. | Frontier quality · Balanced speed · Paid API tier |
    | **`4 Hydrogen`** | `gemini-4-hydrogen` | **ON** | Best for fast everyday coding, routine edits, and parallel agent workflows at lower cost. | Fast & efficient · Free & Paid API tiers |
    | **`4 Argon`** | `gemini-4-argon` | OFF | Best for efficient Gemini 4 coding, math, and science with lower token spend per turn than Carbon. | Efficient frontier · Paid API tier |
    | **`3.8 Flash`** | `gemini-3.8-flash` | OFF | Best for cost-efficient previous-gen Flash coding, multi-file edits, and steady tool execution. | Previous-gen Flash · 1M in / 64K out |

    - **0-models edge case:** Unchecking all models is allowed as a non-destructive pause; shows an inline notice (`No models enabled — none will appear in Antigravity's selector.`) with a one-click **`Restore defaults`** button.
    - **Future multi-backend support:** `.provider-stack` renders from a typed provider registry (`ProviderDescriptor[]`). Future **OpenAI Codex** / GPT support adds a sibling card cleanly without changing layout. Do **not** render fake disabled "Coming Soon" cards in production.

#### Section B — Collapsed `Advanced` (`Plan · Local proxy`)
Closed by default; contains **only** two controls:
1. **`Antigravity plan`** (3 radio cards — saves on click; zero internal codenames, zero URLs, zero `Custom`):
   - **`Auto`** *(badge shows resolved `Organization` or `Personal`)* — `Detect from your signed-in Antigravity account`
   - **`Personal`** — `Google AI Pro or personal Google account`
   - **`Organization`** — `Managed organization, Workspace, or PayGo plan`
2. **`Local proxy`** *(Unix only; hidden on Windows)*:
   - Left column: **`Local proxy`** + status tag (`Active` / `Reopen Antigravity` / `Disabled` / `Manual`) and warning copy (`Not recommended to disable — Antigravity needs the local proxy to load your enabled models.`).
   - Right-aligned action button: **`Disable proxy`** (red-tinted; opens inline 2-click `Confirm disable` / `Cancel`). When disabled, flips to primary **`Re-enable proxy`** button.

### 4.5 Pinned Footer (`36px`) & Automatic Sync
- **Left status trigger (`<button aria-expanded aria-controls="status-popover">`):**
  - Healthy: green dot + **`Live`** (no port string, no `Auto → Workspace`).
  - Unhealthy / disabled: amber dot + concise status (**`Proxy offline · Retrying…`**, **`Proxy disabled`**, or **`Update pending`**).
  - Hover, keyboard focus, or click opens an anchored **Proxy health** popover (keeping keyboard focus stable without rebuilding the footer DOM) strictly focused on proxy health, showing ONLY: `Endpoint` (`127.0.0.1:<port>` read-only), `Antigravity plan` (`Auto (Organization)` / `Auto` / `Personal` / `Organization`), `Proxy / CLI` versions (`v0.1.0 / v0.1.0`), `Memory` (`XX MB RAM` or `—`), and `Issue` (only when unhealthy). Do **not** include `Enabled models`, `Session`, or `Platform` rows.
- **Right-side memory indicator:** Quiet `XX MB RAM` displayed on the right side of the pinned footer when the proxy is reachable and `proxy_rss_bytes` is available (hidden when offline).
- **No Refresh button:** Replaced by mutation-returned snapshots + visibility-aware background polling (`10s` healthy while visible, paused while hidden, throttled re-check on focus/visibility, `3s → 5s → 10s` unhealthy backoff), preserving `snapshotEpoch` and write lockout.

---

## 5. Explicitly Removed Controls (Anti-Slop Audit)

| Removed Element | Why Removed | Backend / CLI Compatibility |
| --- | --- | --- |
| **2-column wide grid** | Unnecessary split; left settings + right idle tiles wasted space and differed from Antigravity's native single-column settings pages. | N/A (UI only) |
| **Editable `Listen port` input** | Users never need to pick a port manually. Install/first-start uses `24141` and auto-probes `+1` (`24142..=24150`) if occupied; active endpoint is shown read-only in the `● Live` popover. | `config.listen_port` and `--port` CLI flag stay intact. |
| **`Upstream base URL` input** | Google AI Studio–specific and effectively read-only for end users. | `config.gemini.base_url` and partial-update `save_settings` remain supported for tests/power users. |
| **`Custom` plan & `Custom` model pills** | No end user knows internal routing URLs or arbitrary unverified model IDs. | `config.cloudcode_upstream_url` and custom model IDs in `config.json` / CLI remain supported. |
| **`CLI & manual setup` section** | Desktop plugin users don't need launch/JSON snippets once the local proxy hook is enabled by default. | `agy-companion launch` and `agy-companion models --json` stay intact in CLI. |
| **Bottom-right `Refresh` button & `Auto → Workspace` footer text** | Polling handles freshness automatically; routing mode belongs inside health popover / Advanced. | N/A (UI only) |
| **Internal codenames & raw URLs in UI** | Open-source hygiene — never expose internal codenames or `*-pa.googleapis.com` URLs in `ui/`. | MCP server maps semantic `account_plan` ↔ upstream URLs server-side. |

---

## 6. First-Run Auto-Enable State Machine

Add `#[serde(default)] pub launcher_auto_start: Option<bool>` to `Config` (`schema_version = 1`):

| `launcher_auto_start` | Context | Action | Resulting State |
| --- | --- | --- | --- |
| `None` | **Plugin startup** (`serve --auto-enable-launcher` primary + idempotent `open_settings` fallback) | Acquire cross-process lockfile `<data_dir>/integration.lock` (`0600`), re-read `config.json`; if still `None` and Unix `supported && !enabled && !hybrid_active`, run `integration::enable` with `follow_rc_symlink: false` (S8). | Persist `Some(true)` (including on `manual_required` so symlinked profiles don't re-attempt on every start). Show top reopen/manual banner. |
| `None` | **Standalone CLI / E2E `serve`** (no `--auto-enable-launcher`) | Do nothing — never mutate host RC files during CLI or E2E runs. | Stays `None`. |
| `Some(false)` | Explicit opt-out (`Disable proxy`, `install --no-launcher-hook`, or `integration disable`) | Respect opt-out forever; never auto-enable silently. | Show top **`Local proxy is disabled`** banner with **`Re-enable proxy`** button on every open until re-enabled. |
| `Some(true)` | Enabled (auto or explicit) | Reflect `integration::status` cleanly without rewriting RC files. | Active (`hybrid_active`), Manual banner (`manual_required`), or Reopen banner (`!hybrid_active`). |

Live Hybrid Mode detection continues to use `stats.last_catalog_refresh_at_ms.is_some()` (since Antigravity strips proxy env vars from MCP subprocesses).

---

## 7. Backend & MCP Contract Delta + Implementation Roadmap

### Contract Delta (Aligned with Post-PR #37 Decomposed Workspace)

| Crate / Area | Additive Changes |
| --- | --- |
| **`companion-core`** (`config.rs`, `integration.rs`, `usage/sqlite.rs`, `protocol.rs`) | 1) Add `gemini.enabled_models: Vec<String>` (default `["gemini-4-carbon", "gemini-4-neon", "gemini-4-hydrogen"]`) and `launcher_auto_start: Option<bool>` (`None`), keeping `default_model` backward-compatible on deserialization. 2) Add cross-process `<data_dir>/integration.lock` (`0600`). 3) Add read-only SQLite aggregation queries (`query_usage_summary(range)` by day, model, provider) over existing `usage_events` table. |
| **`companion-proxy`** (`routes/`, `cloudcode/catalog.rs`, `stats.rs`, `rss.rs`) | 1) Filter injected `(Proxy)` catalog models strictly to `active.enabled_models` (allowing `[]` for 0 injected models; default 3 models use 11/32 slots in `M334..=M365`). 2) Track a bounded in-memory ring buffer of recent calls (`model`, `provider`, `source`, `input`, `cached`, `output`, `thinking`, `timestamp_ms`, `latency_ms`, `cache_break`) plus per-model session rollups on `/api/stats`. 3) Passively tee `usageMetadata` and model ID on `POST /v1internal:streamGenerateContent` and `POST /v1internal:generateContent` (`provider = "antigravity"`, `source = "builtin"`), strictly skipping compressed (`Content-Encoding != identity`) responses per S4, recording into both `ProxyStats` and SQLite `usage.db`. 4) Measure proxy RSS in pure safe Rust (`#![forbid(unsafe_code)]`, ~10–30s TTL cache) and expose `proxy_rss_bytes: Option<u64>` in `/api/stats`. |
| **`companion-cli`** (`cli.rs`, `install/`, `integration.rs`, `serve.rs`, `plugin/sidecars/proxy/sidecar.json`, `plugin/mcp_config.json`) | Add `serve --auto-enable-launcher` (used by `sidecar.json` only), `ui-mcp --auto-enable-launcher` (used by `mcp_config.json` only), optional `--config <PATH>` on `integration {enable,disable,status}`, and persist `launcher_auto_start` on `install [--no-launcher-hook]` and `integration enable\|disable` under `integration.lock`. |
| **`companion-ui-mcp`** (`types.rs`, `tools.rs`, `server.rs`) | 1) Accept `enabled_models` and semantic `account_plan` (`auto \| personal \| organization`) in `save_settings`. 2) Expose semantic `account_plan`, `enabled_models`, `proxy_rss_bytes`, live Monitor session metrics (built-in + BYOM), and historical `get_usage(range)` tool backed by `usage.db` (optional `sqlite` feature). 3) Persist `launcher_auto_start` in `set_launcher_integration` and gate locked `open_settings` first-run auto-enable fallback on `UiServerOptions::auto_enable_launcher` (default `false`). |
| **`ui/`** (`index.html`, `src/style.css`, `src/*.ts`) | Implement single-column 3-tab UI (**Monitor** \| **Usage** \| **Settings**), `hybrid_active`-first landing tab, built-in + BYOM Monitor/Usage views (zero API key prompt or `Open Settings` button in Monitor idle states), `Google AI Studio` provider card + `app.openLink`, multi-select `Enabled models` + clamped tooltips, streamlined `Advanced` (`Antigravity plan` + right-aligned `Disable proxy`), minimal `● Live` footer with strict health-only popover + right-side `XX MB RAM` when reachable, visibility-aware polling, and WCAG AA Light-mode `--brand: #1558b0`. |

### Incremental PR Sequence
1. **PR A — Config, Enabled Models, First-Run Auto-Enable & Spec Sync (`companion-core`, `companion-proxy`, `companion-cli`, `docs/design/ui-ux-v2.md`):** `enabled_models`, `launcher_auto_start`, `<data_dir>/integration.lock`, catalog filtering, `serve --auto-enable-launcher`, CLI opt-out parity, and spec sync.
2. **PR B — Usage Query API, Proxy RSS, Built-in Usage Tee & Semantic MCP Contract (`companion-core`, `companion-proxy`, `companion-ui-mcp`, `plugin/mcp_config.json`):** `proxy_rss_bytes`, passive `/v1internal:*` built-in usage tee (S4 compressed-response skip), SQLite `usage.db` range aggregations, session recent-calls ring buffer, semantic `account_plan` mapping, `get_usage`, and locked `open_settings` fallback (`ui-mcp --auto-enable-launcher`).
3. **PR C — UX v2 Single-Column 3-Tab UI (`ui/`, `e2e/`):** Implement **Monitor**, **Usage**, and **Settings** tabs, rebuild `ui/dist/index.html` (`< 512 KB`, 0 S5 sinks/external tags, 0 internal codenames), and update E2E UI contract assertions.
4. **PR D — Open-Source Copy Hygiene (`plugin/README.md`, `plugin/skills/companion/SKILL.md`):** Scrub remaining user-facing internal codenames and raw backend URLs from plugin docs.

---

## 8. S1–S8 Invariants Compliance

- **S1 (Loopback-only):** No host or port inputs exposed in UI; loopback `127.0.0.1` enforced server-side.
- **S2 (Secret redaction & atomic persistence):** Write-only API key input, `PROXY_PLACEHOLDER_API_KEY` quarantine, mutex + cross-process `<data_dir>/integration.lock` (`0600`) serialization, zero real keys in UI/logs/SQLite.
- **S3 (Upstream auth & SSRF):** Removing `Upstream base URL` and `Custom` plan inputs shrinks user-editable URL surface while keeping server-side HTTPS/loopback validation intact.
- **S4 (Byte-for-byte streaming):** Unchanged; Monitor & Usage read passive `usageMetadata` tees and SQLite `usage.db`.
- **S5 (Pure safe DOM, CSP & `< 512 KB` bundle):** Pure `createElement` / `textContent` / `replaceChildren` / `createElementNS`, zero `innerHTML`/`eval`/`fetch`/`confirm`, CSP-safe `app.openLink` + clipboard fallback, WCAG AA contrast `>= 4.5:1` in Dark and Light (`--brand: #1558b0`), `snapshotEpoch` + write lockout preserved.
- **S6 (XDG + Windows paths):** Fail-closed XDG/Windows path checks unchanged; Windows renders Start Menu shortcut guidance instead of Unix shell-hook controls.
- **S7 (Minimal HTTP surface):** Unchanged (`GET`/`POST`, `64 MiB` cap, `nosniff` + `no-store`).
- **S8 (Launcher & symlink safety):** First-run auto-enable strictly enforces `follow_rc_symlink: false` under `<data_dir>/integration.lock`; symlinked profiles never mutate and surface `One manual step needed`.
