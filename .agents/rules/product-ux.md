---
trigger: model_decision
description: Product, UX, and model-catalog guardrails for agy-companion. Activate when designing or modifying the Aux Panel UI, CLI onboarding/install UX, settings schema, or provider model catalogs.
---

# Product, UX & Model Catalog Guardrails

## Zero-Config, Immediate-Value Defaults

- **Minimal setup to first value:** Ship defaults that work immediately without manual wiring. Auto-enable proxy integration on first start and surface a clear top banner when the user needs to quit and reopen Antigravity.
- **No unnecessary knobs:** Do not expose raw port configuration in the UI — pick the default port at install time and auto-increment (`+1`) on collision.
- **Zero-terminal Aux Panel flows:** Prefer one-click actions inside the Aux Panel (e.g., browser OAuth login for providers like OpenAI Codex, one-click upgrade) over requiring users to open a terminal and run CLI subcommands.

## Model Catalog & Picker Conventions

- **Latest models only:** Include only current-generation models users actively adopt (e.g., Gemini 3.8 Flash+ / 4.x; latest GPT 6.x). Drop legacy/older-generation models rather than padding catalogs.
- **Capability ordering:** Order models from **most capable → least capable** in pickers and catalogs.
- **Additive, clearly labeled proxy models:** Always preserve the user's original built-in Antigravity models alongside proxy-provided models, and suffix proxy-provided display names with `(Proxy)`.

## Zero AI Slop in Aux Panel UI

- **Full-width single-column layout:** Keep the Aux Panel single-column with the inner content container spanning the full panel width (no narrow centered max-width box or wasted horizontal gutters); avoid 2-column dashboards or decorative cards.
- **Modern web platform baseline:** Prefer native Chromium/Baseline primitives (`light-dark()`, `popover` API, `:has()`, `scrollbar-gutter: stable`, `rem` typography, `prefers-contrast`) over custom JS/CSS re-implementations, with zero runtime polyfills or external CDN loads (S5).
- **No redundant metadata:** Never display facts the user already knows (e.g., OS/platform badge).
- **Unambiguous user copy:**
  - Say **"Quit and reopen Antigravity (`⌘Q`)"** — never vague "Restart" that sounds like an OS reboot.
  - Frame opt-out switches negatively with clear consequence (e.g., **"Disable proxy"** with a warning, rather than ambiguous "Auto-start with Antigravity").
- **Minimal footer:** Show only a compact status dot (`Live` / unhealthy alert), proxy RAM usage, and a health-only hover popover (do not clutter the footer with enabled-model lists).
- **Session-accurate Monitor tab:** Reflect the current session's live traffic and capture built-in Antigravity model requests even before an external provider API key is configured.
