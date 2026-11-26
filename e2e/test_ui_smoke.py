"""Lean headless Chrome CDP UI smoke suite (`--suite ui-smoke`).

Renders the compiled `ui/dist/index.html` inside a pure-stdlib loopback mock
MCP App host and verifies:
1. Multi-viewport (`360px`/`460px`/`760px`) × Dark/Light × 3-tab rendering,
   single-column `.content-wrap` centering, zero horizontal overflow, zero
   legacy copy tokens, WCAG AA `>= 4.5:1` contrast (incl. Light `--brand`
   `#1558b0` on `.lnk` and enabled `button.btn-primary`), and zero console errors.
2. Health-only `Live` footer popover (required vs forbidden rows), `Escape`
   dismissal, right-side `XX MB RAM` telemetry, and offline RAM hiding + `Issue`.
"""

from __future__ import annotations

import json
import re
import time
from typing import Any

from cdp_smoke import (
    ChromeBrowser,
    UiCdpSession,
    parse_css_rgb,
    require_chrome_or_skip,
    wcag_contrast_ratio,
)
from harness import E2ETestCase, find_free_port, http_raw_request
from mock_gemini import JsonAction


_MOCK_PROXY_PORT = 24999
_FORBIDDEN_COPY_TOKENS: tuple[str, ...] = (
    "CloudCode",
    "Jetski",
    "Auto \u2192 Workspace",
    "Restart required",
    "Coming Soon",
)
_SUPPORTED_MODELS: list[dict[str, str]] = [
    {"id": "gemini-4-carbon", "display_name": "Gemini 4 Carbon", "short_label": "4 Carbon"},
    {"id": "gemini-4-neon", "display_name": "Gemini 4 Neon", "short_label": "4 Neon"},
    {"id": "gemini-4-hydrogen", "display_name": "Gemini 4 Hydrogen", "short_label": "4 Hydrogen"},
    {"id": "gemini-4-argon", "display_name": "Gemini 4 Argon", "short_label": "4 Argon"},
    {"id": "gemini-3.8-flash", "display_name": "Gemini 3.8 Flash", "short_label": "3.8 Flash"},
]


def _make_snapshot(
    *,
    reachable: bool = True,
    rss_bytes: int | None = 25_165_824,
    error: str | None = None,
) -> dict[str, Any]:
    stats: dict[str, Any] | None = None
    if reachable:
        stats = {
            "usage": {
                "requests": 2,
                "prompt_tokens": 4_000,
                "cached_tokens": 2_500,
                "output_tokens": 800,
                "reasoning_tokens": 160,
                "cache_hit_ratio": 0.625,
            },
            "cloudcode": {
                "mode": "auto",
                "resolved_upstream": "https://cloudcode-pa.googleapis.com",
                "last_catalog_refresh_at_ms": 1_747_299_990_000,
                "last_catalog_included_proxy_models": True,
            },
            "proxy_rss_bytes": rss_bytes,
            "recent_calls": [
                {
                    "timestamp_ms": 1_747_300_000_000,
                    "provider": "antigravity",
                    "source": "builtin",
                    "model": "gemini-3.8-pro",
                    "input_tokens": 1_000,
                    "cached_tokens": 0,
                    "output_tokens": 200,
                    "reasoning_tokens": 40,
                    "latency_ms": 320,
                    "cold_start": True,
                    "cache_break": False,
                },
                {
                    "timestamp_ms": 1_747_300_060_000,
                    "provider": "gemini",
                    "source": "proxy",
                    "model": "gemini-4-carbon",
                    "input_tokens": 3_000,
                    "cached_tokens": 2_500,
                    "output_tokens": 600,
                    "reasoning_tokens": 120,
                    "latency_ms": 480,
                    "cold_start": False,
                    "cache_break": False,
                },
            ],
            "by_model": [
                {
                    "provider": "gemini",
                    "source": "proxy",
                    "model": "gemini-4-carbon",
                    "requests": 1,
                    "input_tokens": 3_000,
                    "cached_tokens": 2_500,
                    "output_tokens": 600,
                    "reasoning_tokens": 120,
                    "total_latency_ms": 480,
                },
                {
                    "provider": "antigravity",
                    "source": "builtin",
                    "model": "gemini-3.8-pro",
                    "requests": 1,
                    "input_tokens": 1_000,
                    "cached_tokens": 0,
                    "output_tokens": 200,
                    "reasoning_tokens": 40,
                    "total_latency_ms": 320,
                },
            ],
        }
    return {
        "config": {
            "schema_version": 1,
            "proxy": {"host": "127.0.0.1", "port": _MOCK_PROXY_PORT},
            "cloudcode": {"upstream": "auto"},
            "providers": {
                "gemini": {
                    "base_url": "https://generativelanguage.googleapis.com",
                    "default_model": "gemini-4-hydrogen",
                    "enabled_models": ["gemini-4-carbon", "gemini-4-neon", "gemini-4-hydrogen"],
                    "timeout_secs": 900,
                    "api_key_configured": True,
                    "api_key_hint": "test...1234",
                }
            },
            "launcher_auto_start": True,
        },
        "binary_version": "0.1.0",
        "proxy": {
            "reachable": reachable,
            "status": "ok" if reachable else "offline",
            "version": "0.1.0" if reachable else None,
            "protocol_version": 1 if reachable else None,
            "error": error,
        },
        "version_skew": False,
        "stats": stats,
        "launcher_integration": {
            "state": "active",
            "enabled": True,
            "hybrid_active": reachable,
            "shell": "zsh",
            "rc_path": "/Users/dev/.zlogin",
            "env_script_path": "/Users/dev/.local/share/agy-companion/env.sh",
            "binary_path": "/Users/dev/.local/share/agy-companion/bin/agy-companion",
            "symlink_target": None,
            "manual_snippet": None,
            "message": None,
        },
        "cloudcode": {
            "mode": "auto",
            "configured_upstream": "auto",
            "resolved_upstream": "https://cloudcode-pa.googleapis.com",
        },
        "supported_models": _SUPPORTED_MODELS,
        "platform": "macos",
        "account_plan": {"mode": "auto", "resolved": "organization"},
        "proxy_rss_bytes": rss_bytes,
    }


def _make_usage_7d() -> dict[str, Any]:
    return {
        "range": "7d",
        "since_utc": "2026-05-09T00:00:00Z",
        "totals": {
            "label": "",
            "requests": 3,
            "input_tokens": 3_500,
            "cached_tokens": 2_000,
            "output_tokens": 850,
            "reasoning_tokens": 170,
        },
        "buckets": [
            {"label": "2026-05-12", "requests": 1, "input_tokens": 500, "cached_tokens": 100, "output_tokens": 150, "reasoning_tokens": 20},
            {"label": "2026-05-15", "requests": 2, "input_tokens": 3_000, "cached_tokens": 1_900, "output_tokens": 700, "reasoning_tokens": 150},
        ],
        "by_model": [
            {"key": "gemini-4-carbon", "provider": "gemini", "source": "proxy", "model_count": None, "requests": 1, "input_tokens": 2_000, "cached_tokens": 1_500, "output_tokens": 500, "reasoning_tokens": 100, "avg_latency_ms": 500},
            {"key": "gemini-3.8-pro", "provider": "antigravity", "source": "builtin", "model_count": None, "requests": 1, "input_tokens": 1_000, "cached_tokens": 400, "output_tokens": 200, "reasoning_tokens": 50, "avg_latency_ms": 300},
        ],
        "by_provider": [
            {"key": "gemini", "provider": "gemini", "source": None, "model_count": 1, "requests": 2, "input_tokens": 2_500, "cached_tokens": 1_600, "output_tokens": 650, "reasoning_tokens": 120, "avg_latency_ms": 300},
            {"key": "antigravity", "provider": "antigravity", "source": None, "model_count": 1, "requests": 1, "input_tokens": 1_000, "cached_tokens": 400, "output_tokens": 200, "reasoning_tokens": 50, "avg_latency_ms": 300},
        ],
    }


_OPAQUE_BG_HELPER_JS = """
const opaqueBg = (el) => {
  let cur = el;
  while (cur && cur.nodeType === 1) {
    const bg = getComputedStyle(cur).backgroundColor;
    const m = bg && bg.match(/rgba?\\(\\s*([\\d.]+)\\s*,\\s*([\\d.]+)\\s*,\\s*([\\d.]+)(?:\\s*,\\s*([\\d.]+))?\\s*\\)/);
    if (m && (m[4] === undefined || parseFloat(m[4]) > 0.99)) return bg;
    cur = cur.parentElement;
  }
  return getComputedStyle(document.documentElement).backgroundColor;
};
"""


class TestUiSmoke(E2ETestCase):
    SUITE = "ui-smoke"
    FEATURE = "UI_SMOKE"

    def _load_dist_html(self) -> bytes:
        dist_path = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(dist_path.is_file(), f"Compiled UI bundle missing at {dist_path}")
        return dist_path.read_bytes()

    def test_ui_smoke_multi_viewport_dark_light_overflow_and_wcag(self) -> None:
        """360/460/760px × Dark/Light × 3 tabs: zero overflow, centered <=560px column, WCAG AA >=4.5:1, zero legacy copy or console errors."""
        chrome_bin = require_chrome_or_skip(self)
        env = self.make_env()
        html = self._load_dist_html()
        boot = {
            "theme": "dark",
            "initialSnapshot": _make_snapshot(reachable=True, rss_bytes=25_165_824),
            "initialUsage": {"7d": _make_usage_7d()},
        }

        with UiCdpSession(
            html,
            chrome_bin=chrome_bin,
            boot_config=boot,
            viewport=(360, 720),
            user_data_dir=env.root / "chrome-profile",
        ) as session:
            # Verify exactly 3 tabs and zero visible Refresh button.
            header = session.eval_app(
                """(() => {
                  const tabs = [...document.querySelectorAll('[role="tablist"] [role="tab"]')].map(
                    (el) => el.getAttribute('aria-label') || ''
                  );
                  const refreshBtns = [...document.querySelectorAll('button')].filter(
                    (b) => !b.hidden && /refresh/i.test((b.textContent || '') + ' ' + (b.getAttribute('aria-label') || ''))
                  ).length;
                  return { tabs, refreshBtns };
                })()"""
            )
            self.assertEqual(header["tabs"], ["Monitor", "Usage & Costs", "Settings"])
            self.assertEqual(header["refreshBtns"], 0)

            usage_loaded = False
            checked_light_brand = False

            for width, height in ((360, 720), (460, 760), (760, 760)):
                session.set_viewport(width, height)
                for theme in ("dark", "light"):
                    session.set_theme(theme)
                    for tab_label in ("Monitor", "Usage & Costs", "Settings"):
                        session.eval_app(
                            f"""(() => {{
                              const btn = [...document.querySelectorAll('[role="tablist"] [role="tab"]')].find(
                                (b) => b.getAttribute('aria-label') === {json.dumps(tab_label)}
                              );
                              if (!btn) throw new Error('Tab missing: {tab_label}');
                              btn.click();
                              return true;
                            }})()"""
                        )
                        if tab_label == "Usage & Costs" and not usage_loaded:
                            session.wait_for(
                                """(() => {
                                  const panel = document.querySelector('[role="tabpanel"][aria-label="Usage"]');
                                  return Boolean(panel && !panel.hidden && !/Loading usage history/i.test(panel.textContent || ''));
                                })()""",
                                timeout=5.0,
                            )
                            usage_loaded = True
                        else:
                            session._flush_frames()

                        metrics = session.eval_app(
                            f"""(() => {{
                              {_OPAQUE_BG_HELPER_JS}
                              const docEl = document.documentElement;
                              const scroll = document.querySelector('.pane-scroll');
                              const wrap = document.querySelector('.content-wrap');
                              const rect = wrap ? wrap.getBoundingClientRect() : null;
                              const brand = document.querySelector('.brand-name');
                              const card = [...document.querySelectorAll('.card')].find(
                                (c) => c.offsetParent !== null
                              );
                              return {{
                                docOverflow: docEl.scrollWidth - docEl.clientWidth,
                                scrollOverflow: scroll ? scroll.scrollWidth - scroll.clientWidth : 0,
                                wrapOverflow: wrap ? wrap.scrollWidth - wrap.clientWidth : 0,
                                wrapRect: rect ? {{ left: rect.left, right: rect.right, width: rect.width }} : null,
                                bodyText: document.body.innerText || '',
                                brandFg: brand ? getComputedStyle(brand).color : null,
                                brandBg: brand ? opaqueBg(brand) : null,
                                cardFg: card ? getComputedStyle(card).color : null,
                                cardBg: card ? opaqueBg(card) : null,
                              }};
                            }})()"""
                        )
                        ctx = f"viewport={width}x{height} theme={theme} tab={tab_label}"
                        self.assertLessEqual(metrics["docOverflow"], 1, f"documentElement overflow ({ctx})")
                        self.assertLessEqual(metrics["scrollOverflow"], 1, f".pane-scroll overflow ({ctx})")
                        self.assertLessEqual(metrics["wrapOverflow"], 1, f".content-wrap overflow ({ctx})")

                        if width == 760:
                            rect = metrics["wrapRect"]
                            self.assertIsNotNone(rect, f"missing .content-wrap ({ctx})")
                            self.assertLessEqual(rect["width"], 560.5, f".content-wrap too wide ({ctx})")
                            self.assertLessEqual(
                                abs(rect["left"] - (760.0 - rect["right"])),
                                4.0,
                                f".content-wrap not centered ({ctx}): {rect}",
                            )

                        for token in _FORBIDDEN_COPY_TOKENS:
                            self.assertNotIn(token, metrics["bodyText"], f"legacy copy {token!r} ({ctx})")

                        # Primary text WCAG AA >= 4.5:1 in both Dark and Light.
                        brand_ratio = wcag_contrast_ratio(
                            parse_css_rgb(metrics["brandFg"]), parse_css_rgb(metrics["brandBg"])
                        )
                        self.assertGreaterEqual(brand_ratio, 4.5, f".brand-name contrast ({ctx}): {brand_ratio:.2f}")
                        if metrics["cardFg"] and metrics["cardBg"]:
                            card_ratio = wcag_contrast_ratio(
                                parse_css_rgb(metrics["cardFg"]), parse_css_rgb(metrics["cardBg"])
                            )
                            self.assertGreaterEqual(card_ratio, 4.5, f".card contrast ({ctx}): {card_ratio:.2f}")

                        # Light --brand (#1558b0) contrast on `.lnk` and enabled `button.btn-primary`.
                        if theme == "light" and tab_label == "Settings" and not checked_light_brand:
                            session.eval_app(
                                """(() => {
                                  const input = document.getElementById('gemini-api-key');
                                  input.value = 'test-smoke-draft-key';
                                  input.dispatchEvent(new Event('input', { bubbles: true }));
                                  return true;
                                })()"""
                            )
                            # Allow .btn's 120ms background transition to settle on --brand (#1558b0).
                            session.wait_for(
                                """(() => {
                                  const btn = document.querySelector('button.btn-primary');
                                  return Boolean(btn && !btn.disabled && getComputedStyle(btn).backgroundColor === 'rgb(21, 88, 176)');
                                })()""",
                                timeout=3.0,
                            )
                            brand_check = session.eval_app(
                                f"""(() => {{
                                  {_OPAQUE_BG_HELPER_JS}
                                  const rootBrand = getComputedStyle(document.documentElement)
                                    .getPropertyValue('--brand').trim();
                                  const lnk = document.querySelector('button.lnk');
                                  const lnkFg = lnk ? getComputedStyle(lnk).color : null;
                                  const lnkBg = lnk ? opaqueBg(lnk) : null;
                                  const input = document.getElementById('gemini-api-key');
                                  const saveBtn = document.querySelector('button.btn-primary');
                                  const btnDisabled = saveBtn.disabled;
                                  const btnFg = getComputedStyle(saveBtn).color;
                                  const btnBg = opaqueBg(saveBtn);
                                  input.value = '';
                                  input.dispatchEvent(new Event('input', {{ bubbles: true }}));
                                  return {{ rootBrand, lnkFg, lnkBg, btnDisabled, btnFg, btnBg }};
                                }})()"""
                            )
                            self.assertEqual(parse_css_rgb(brand_check["rootBrand"]), (21, 88, 176))
                            self.assertFalse(brand_check["btnDisabled"])
                            self.assertEqual(parse_css_rgb(brand_check["btnBg"]), (21, 88, 176))
                            lnk_ratio = wcag_contrast_ratio(
                                parse_css_rgb(brand_check["lnkFg"]), parse_css_rgb(brand_check["lnkBg"])
                            )
                            btn_ratio = wcag_contrast_ratio(
                                parse_css_rgb(brand_check["btnFg"]), parse_css_rgb(brand_check["btnBg"])
                            )
                            self.assertGreaterEqual(lnk_ratio, 4.5, f"Light .lnk contrast: {lnk_ratio:.2f}")
                            self.assertGreaterEqual(btn_ratio, 4.5, f"Light .btn-primary contrast: {btn_ratio:.2f}")
                            checked_light_brand = True

            self.assertTrue(checked_light_brand)
            session.assert_no_console_errors()

    def test_ui_smoke_footer_health_popover_and_ram_telemetry(self) -> None:
        """Footer shows `Live` (no port) + `24 MB RAM`, health-only popover with `Escape` close, and offline RAM hide + `Issue` row."""
        chrome_bin = require_chrome_or_skip(self)
        env = self.make_env()
        html = self._load_dist_html()
        healthy = _make_snapshot(reachable=True, rss_bytes=25_165_824)
        offline = _make_snapshot(reachable=False, rss_bytes=None, error="connection refused")

        with UiCdpSession(
            html,
            chrome_bin=chrome_bin,
            boot_config={"theme": "dark", "initialSnapshot": healthy},
            viewport=(460, 760),
            user_data_dir=env.root / "chrome-profile",
        ) as session:
            read_footer_js = """(() => {
              const trigger = document.querySelector('button.status-trigger');
              const ram = document.querySelector('span.footer-ram');
              const pop = document.getElementById('status-popover');
              const footer = document.querySelector('footer.status-bar');
              return {
                triggerText: (trigger && trigger.innerText || '').trim(),
                expanded: trigger ? trigger.getAttribute('aria-expanded') : null,
                ramHidden: ram ? Boolean(ram.hidden) : true,
                ramText: (ram && ram.textContent || '').trim(),
                footerText: (footer && footer.innerText || '').trim(),
                popVisible: Boolean(pop && !pop.hidden && pop.offsetParent !== null),
                popText: (pop && pop.innerText || '').trim(),
              };
            })()"""

            initial = session.eval_app(read_footer_js)
            self.assertIn("Live", initial["triggerText"])
            self.assertNotIn(str(_MOCK_PROXY_PORT), initial["triggerText"])
            self.assertFalse(initial["ramHidden"])
            self.assertRegex(initial["ramText"], r"\b24\s*MB\s*RAM\b")
            self.assertFalse(initial["popVisible"])

            # Open popover via click from clean closed state (avoids mouseenter->click toggle race).
            session.eval_app("document.querySelector('button.status-trigger').click()")
            session._flush_frames()
            opened = session.eval_app(read_footer_js)
            self.assertTrue(opened["popVisible"])
            self.assertEqual(opened["expanded"], "true")
            for required in (
                "Endpoint",
                f"127.0.0.1:{_MOCK_PROXY_PORT}",
                "Antigravity plan",
                "Auto (Organization)",
                "Proxy / CLI",
                "v0.1.0 / v0.1.0",
                "Memory",
                "24 MB RAM",
            ):
                self.assertIn(required, opened["popText"])
            for forbidden in ("Enabled models", "Session", "Platform", "Issue"):
                self.assertNotIn(forbidden, opened["popText"])

            # Focus trigger inside iframe and dismiss via real CDP Escape key.
            session.eval_app("document.querySelector('button.status-trigger').focus()")
            session.press_key("Escape")
            closed = session.eval_app(read_footer_js)
            self.assertFalse(closed["popVisible"])
            self.assertEqual(closed["expanded"], "false")

            # Push offline snapshot: RAM hides and `Issue` row appears inside popover.
            session.set_snapshot(offline)
            session.wait_for(
                "Boolean(document.querySelector('button.status-trigger') && /Proxy offline/i.test(document.querySelector('button.status-trigger').innerText || ''))",
                timeout=5.0,
            )
            off = session.eval_app(read_footer_js)
            self.assertIn("Proxy offline", off["triggerText"])
            self.assertTrue(off["ramHidden"])
            self.assertNotIn("MB RAM", off["footerText"])

            session.eval_app("document.querySelector('button.status-trigger').click()")
            session._flush_frames()
            off_open = session.eval_app(read_footer_js)
            self.assertTrue(off_open["popVisible"])
            self.assertIn("Issue", off_open["popText"])
            self.assertIn("connection refused", off_open["popText"])
            for forbidden in ("Enabled models", "Session", "Platform"):
                self.assertNotIn(forbidden, off_open["popText"])

            # Click trigger again to close cleanly.
            session.eval_app("document.querySelector('button.status-trigger').click()")
            session._flush_frames()
            self.assertFalse(session.eval_app(read_footer_js)["popVisible"])
            session.assert_no_console_errors()

    def test_ui_smoke_live_sidecar_web_ui_per_session_monitor_and_theme_sync(self) -> None:
        """Real `agy-companion serve` Sidecar Web UI renders in Chrome, isolates per-session Monitor stats, and syncs host theme-change."""
        chrome_bin = require_chrome_or_skip(self)
        self.require_binary()
        mock = self.start_mock_gemini()
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": "alpha"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 1200,
                        "candidatesTokenCount": 40,
                        "cachedContentTokenCount": 900,
                        "thoughtsTokenCount": 15,
                    },
                },
            ),
            path_contains="/v1beta/models/gemini-4-neon:generateContent",
        )
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": "beta"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 2500,
                        "candidatesTokenCount": 80,
                        "cachedContentTokenCount": 0,
                        "thoughtsTokenCount": 30,
                    },
                },
            ),
            path_contains="/v1beta/models/gemini-4-carbon:generateContent",
        )

        env = self.make_env()
        proxy_port = find_free_port()
        sidecar_port = find_free_port()
        while sidecar_port == proxy_port:
            sidecar_port = find_free_port()

        cfg = env.write_config(
            env.make_config_dict(
                listen_port=proxy_port,
                base_url=mock.base_url,
                api_key="test-sidecar-cdp-key-01",
            )
        )
        token = "sidecar-cdp-token-42"
        self.start_proxy(
            env,
            config_path=cfg,
            port=proxy_port,
            extra_env={
                "ANTIGRAVITY_SIDECAR_WEB_PORT": str(sidecar_port),
                "ANTIGRAVITY_SIDECAR_UI_TOKEN": token,
            },
        )

        # Send two calls in two distinct Antigravity conversations.
        for model, conv_id in (
            ("gemini-4-neon", "conv-alpha-111111"),
            ("gemini-4-carbon", "conv-beta-222222"),
        ):
            payload = json.dumps(
                {
                    "systemInstruction": {
                        "parts": [{"text": f"Conversation ID: {conv_id}\n"}]
                    },
                    "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                }
            ).encode("utf-8")
            resp = http_raw_request(
                "127.0.0.1",
                proxy_port,
                "POST",
                f"/v1beta/models/{model}:generateContent",
                headers={"Content-Type": "application/json"},
                body=payload,
            )
            self.assertEqual(resp.status, 200)

        browser = ChromeBrowser(
            chrome_bin, env.root / "chrome-sidecar-profile", startup_timeout=15.0
        ).start()
        self.stack.callback(browser.stop)
        cdp = browser.open_page_client(timeout=15.0)
        self.stack.callback(cdp.close)
        cdp.call("Page.enable")
        cdp.call("Runtime.enable")
        cdp.call("Log.enable")
        cdp.set_viewport(420, 760)

        def wait_expr(expr: str, timeout: float = 8.0) -> Any:
            deadline = time.monotonic() + timeout
            last: Any = None
            while time.monotonic() < deadline:
                last = cdp.evaluate(expr)
                if last:
                    return last
                time.sleep(0.03)
            self.fail(f"Timed out waiting for {expr!r} (last={last!r})")

        # 1. Open Sidecar Web UI for conv-alpha-111111: must show Gemini 4 Neon and NOT Gemini 4 Carbon.
        cdp.call(
            "Page.navigate",
            {
                "url": f"http://127.0.0.1:{sidecar_port}/?token={token}&conversationId=conv-alpha-111111"
            },
        )
        wait_expr(
            "Boolean(document.body && /Gemini 4 Neon/.test(document.body.innerText || '') && /MB RAM/.test(document.body.innerText || ''))"
        )
        alpha_text = cdp.evaluate("document.body.innerText || ''")
        self.assertIn("Gemini 4 Neon", alpha_text)
        self.assertNotIn("Gemini 4 Carbon", alpha_text)
        self.assertIn("75%", alpha_text)
        self.assertIn("MB RAM", alpha_text)

        # 2. Live theme-change postMessage from Antigravity host switches Light <-> Dark.
        cdp.evaluate(
            """window.postMessage({
              type: 'theme-change',
              payload: { cssVariables: { '--background': '#ffffff' } }
            }, '*')"""
        )
        wait_expr("document.documentElement.dataset.theme === 'light'")
        cdp.evaluate(
            """window.postMessage({
              type: 'theme-change',
              payload: { cssVariables: { '--background': '#141414' } }
            }, '*')"""
        )
        wait_expr("document.documentElement.dataset.theme === 'dark'")

        # 3. Switch to conv-beta-222222: must show Gemini 4 Carbon and NOT Gemini 4 Neon.
        cdp.call(
            "Page.navigate",
            {
                "url": f"http://127.0.0.1:{sidecar_port}/?token={token}&conversationId=conv-beta-222222"
            },
        )
        wait_expr(
            "Boolean(document.body && /Gemini 4 Carbon/.test(document.body.innerText || '') && /MB RAM/.test(document.body.innerText || ''))"
        )
        beta_text = cdp.evaluate("document.body.innerText || ''")
        self.assertIn("Gemini 4 Carbon", beta_text)
        self.assertNotIn("Gemini 4 Neon", beta_text)

        # 4. Switch to an empty conversation: calm empty Monitor + healthy RAM footer & global Usage tab.
        cdp.call(
            "Page.navigate",
            {
                "url": f"http://127.0.0.1:{sidecar_port}/?token={token}&conversationId=conv-empty-999999"
            },
        )
        wait_expr(
            "Boolean(document.body && /Waiting for (?:requests|Antigravity to connect)/.test(document.body.innerText || '') && /MB RAM/.test(document.body.innerText || ''))"
        )
        empty_text = cdp.evaluate("document.body.innerText || ''")
        self.assertNotIn("Gemini 4 Neon", empty_text)
        self.assertNotIn("Gemini 4 Carbon", empty_text)
        # Open Usage tab and confirm global SQLite history loaded both calls across sessions.
        cdp.evaluate(
            """(() => {
              const btn = [...document.querySelectorAll('[role="tablist"] [role="tab"]')].find(
                (b) => b.getAttribute('aria-label') === 'Usage & Costs'
              );
              btn.click();
              return true;
            })()"""
        )
        wait_expr(
            """(() => {
              const panel = document.querySelector('[role="tabpanel"][aria-label="Usage"]');
              return Boolean(panel && !panel.hidden && /2 calls/i.test(panel.innerText || ''));
            })()"""
        )

        cdp.pump_events(0.05)
        self.assertEqual(cdp.console_errors, [], f"Console errors: {cdp.console_errors}")
        self.assertEqual(cdp.exceptions, [], f"JS exceptions: {cdp.exceptions}")
