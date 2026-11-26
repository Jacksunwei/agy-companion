# Security Policy

## Supported Versions

| Version | Supported |
| --- | --- |
| `0.0.x` | Yes (`>= 0.0.2`) |
| `< 0.0.2` | No (pre-release placeholder) |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues or pull requests.**

Report vulnerabilities privately using [GitHub Security Advisories](https://github.com/Jacksunwei/agy-companion/security/advisories/new).

When submitting a report, please include:
- A clear description of the vulnerability and potential impact.
- Minimal reproduction steps or a proof of concept.
- The affected `agy-companion --version`, OS/architecture, and relevant configuration flags.

> **Never include real API keys, bearer tokens, or private prompts** in vulnerability reports, logs, or public issues. Use short redacted placeholders such as `YOUR_GEMINI_API_KEY` or `AIza...abcd`.

We aim to acknowledge private reports within **72 hours** and coordinate remediation and disclosure once a fix is verified.

## Security Scope

Because `agy-companion` runs locally and handles model API keys on behalf of Antigravity, the following surfaces are explicitly in scope:

- **Loopback isolation (S1):** Any way to bind or expose `companion-proxy` beyond `127.0.0.1` / `localhost`.
- **Secret handling & storage (S2):** Plaintext API key exposure via Sidecar `/_api/*` endpoints, logs, error responses, query strings (`?key=`), or weak file permissions on `config.json`, `auth/gemini.json` (`0700` `auth/` dir), `env.sh`, `proxy.pid`, `proxy.log`, or `usage.db`.
- **Upstream auth & SSRF (S3):** Bypassing `https://` enforcement on remote `base_url`, HTTP redirect following, credential smuggling, or unauthorized retry of non-idempotent requests.
- **Shell RC & launcher integrity (S8):** Command injection in generated shell hooks (`env.sh`, `~/.zlogin`, `~/.profile`, `fish/conf.d/`), symlink traversal during install/uninstall, or download/archive verification bypasses in `run-companion.sh`.
- **Sidecar Web UI XSS & CSP (S5):** Injection into the embedded UI (`ui/`), forbidden DOM sinks, or unauthorized network/resource loads.
- **Filesystem & TOCTOU safety (S6):** Path traversal (`..`), NUL byte handling, symlink/FIFO attacks against config, usage database, or `AGY_COMPANION_UI_HTML`.

## Security Architecture Reference

Every Rust crate in this workspace enforces `#![forbid(unsafe_code)]`. For the full S1–S8 security invariants and automated verification gates, see [`AGENTS.md`](AGENTS.md#architecture--security-invariants) and [`docs/design/architecture.md`](docs/design/architecture.md).
