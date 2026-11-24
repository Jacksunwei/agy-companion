"""Tier 1 Rollout: Happy-Path Opaque-Box Feature Coverage (`R01` .. `R25`).

Covers every feature in the 3-PR rollout (`R01`–`R12` PR 1 CloudCode Pro
transparency & proxy hardening, `R13`–`R19` PR 2 Dock/Launcher integration & UI,
`R20`–`R25` PR 3 one-command install/uninstall, Windows parity & docs) with >=2
isolated opaque-box happy-path tests per feature.
"""

from __future__ import annotations

import gzip
import json
import os
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from harness import (
    E2ETestCase,
    IsolatedEnv,
    find_free_port,
    http_raw_request,
    http_stream_request,
    run_cli,
)
from mock_gemini import CapturedRequest, JsonAction, RawBytesAction, SseStreamAction


_PLACEHOLDER_KEY = "agy-companion-local"
_PLACEHOLDER_SLOT_RE = re.compile(r"^MODEL_PLACEHOLDER_M(\d+)$")


def _sample_ccpa_catalog() -> dict[str, Any]:
    return {
        "models": {
            "gemini-4-carbon-high": {
                "model": "MODEL_PLACEHOLDER_M427",
                "displayName": "Gemini 4 Carbon (High)",
                "thinkingLevel": 3,
            },
            "existing-m334": {
                "model": "MODEL_PLACEHOLDER_M334",
                "displayName": "Existing Upstream Slot",
            },
        },
        "defaultAgentModelId": "gemini-4-carbon-high",
        "agentModelSorts": [
            {
                "displayName": "Recommended",
                "groups": [
                    {
                        "displayName": "Google AI Pro",
                        "modelIds": ["gemini-4-carbon-high"],
                    }
                ],
            }
        ],
        "groupedModels": {
            "gemini-4-carbon": {
                "displayName": "Gemini 4 Carbon",
                "supportedThinkingLevels": [
                    {"thinkingSuffix": "high", "thinkingDisplayName": "High"}
                ],
            }
        },
    }


def _install_dummy_plugin_json(env: IsolatedEnv, version: str = "0.1.0") -> Path:
    plugin_dir = env.home_dir / ".gemini" / "config" / "plugins" / "agy-companion"
    plugin_dir.mkdir(parents=True, exist_ok=True)
    pj = plugin_dir / "plugin.json"
    pj.write_text(
        json.dumps({"name": "agy-companion", "version": version}, indent=2) + "\n",
        encoding="utf-8",
    )
    return pj


def _build_safe_bin_dir(env: IsolatedEnv, name: str = "safe_bin") -> Path:
    safe_bin = env.root / name
    safe_bin.mkdir(parents=True, exist_ok=True)
    tools = (
        "awk",
        "basename",
        "bash",
        "sh",
        "cat",
        "chmod",
        "cp",
        "cut",
        "dirname",
        "env",
        "grep",
        "head",
        "id",
        "ln",
        "ls",
        "mkdir",
        "mktemp",
        "mv",
        "printf",
        "pwd",
        "readlink",
        "rm",
        "sed",
        "shasum",
        "sha256sum",
        "sleep",
        "tar",
        "tr",
        "uname",
        "wc",
    )
    for tool in tools:
        for base in ("/usr/bin", "/bin", "/usr/local/bin"):
            cand = Path(base) / tool
            if cand.is_file() and os.access(cand, os.X_OK):
                dst = safe_bin / tool
                if not dst.exists():
                    os.symlink(cand, dst)
                break
    return safe_bin


def _write_env_dumper_script(script_path: Path, dump_path: Path) -> None:
    script_path.write_text(
        f"#!/bin/sh\n"
        f"exec {shlex.quote(sys.executable)} -c "
        f'\'import json,os,pathlib,sys; p=pathlib.Path(sys.argv[1]); t=p.with_suffix(".tmp"); '
        f't.write_text(json.dumps(dict(os.environ)), encoding="utf-8"); os.replace(t, p)\' '
        f"{shlex.quote(str(dump_path))}\n",
        encoding="utf-8",
    )
    os.chmod(script_path, 0o755)


def _wait_for_json_file(path: Path, timeout: float = 5.0) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            try:
                data = json.loads(path.read_text(encoding="utf-8"))
                if isinstance(data, dict):
                    return data
            except (OSError, json.JSONDecodeError):
                pass
        time.sleep(0.05)
    raise AssertionError(f"Timed out waiting for JSON file {path} after {timeout}s")


def _wait_for_health(port: int, timeout: float = 5.0) -> dict[str, Any] | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            resp = http_raw_request("127.0.0.1", port, "GET", "/health", timeout=0.5)
            if resp.status == 200:
                data = resp.json()
                if isinstance(data, dict):
                    return data
        except Exception:
            pass
        time.sleep(0.05)
    return None


def _strip_rust_comments_and_tests(source: str) -> str:
    comment_re = re.compile(
        r'(r#*"[\s\S]*?"#*|"(?:\\.|[^"\\])*")|//[^\n]*|/\*[\s\S]*?\*/'
    )
    no_comments = comment_re.sub(
        lambda m: m.group(1) if m.group(1) is not None else " ",
        source,
    )
    out: list[str] = []
    pos = 0
    cfg_test_re = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
    while True:
        m = cfg_test_re.search(no_comments, pos)
        if m is None:
            out.append(no_comments[pos:])
            break
        out.append(no_comments[pos : m.start()])
        brace_start = no_comments.find("{", m.end())
        semi = no_comments.find(";", m.end())
        if semi != -1 and (brace_start == -1 or semi < brace_start):
            pos = semi + 1
            continue
        if brace_start == -1:
            break
        depth = 0
        i = brace_start
        n = len(no_comments)
        in_str = False
        escape = False
        while i < n:
            ch = no_comments[i]
            if in_str:
                if escape:
                    escape = False
                elif ch == "\\":
                    escape = True
                elif ch == '"':
                    in_str = False
            else:
                if ch == '"':
                    in_str = True
                elif ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                    if depth == 0:
                        i += 1
                        break
            i += 1
        pos = i
    return "".join(out)


class TestTier1RolloutR01CloudCodeDailyFirst(E2ETestCase):
    TIER = 1
    FEATURE = "R01"

    def test_t1_r01_google_ai_pro_managed_project_routes_daily_first_with_prod_fallback(self) -> None:
        """R01: Auto fetchAvailableModels with managed project tries daily first then falls back to prod."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR01Pro1234",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        t0 = time.monotonic()
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer rollout-r01-pro-token",
                "Content-Type": "application/json",
            },
            body=b'{"project":"managed-caic-123"}',
            timeout=5.0,
        )
        elapsed = time.monotonic() - t0
        self.assertEqual(resp.status, 502, resp.text)
        self.assertLess(elapsed, 5.0)
        targets = connect_proxy.wait_for_connects(2, timeout=3.0)
        self.assertGreaterEqual(len(targets), 2, f"Expected daily->prod fallback, got {targets!r}")
        self.assertEqual(targets[0], "daily-cloudcode-pa.googleapis.com:443")
        self.assertEqual(targets[1], "cloudcode-pa.googleapis.com:443")

    def test_t1_r01_unresolved_empty_body_fetch_models_routes_daily_first(self) -> None:
        """R01: Unresolved auto fetchAvailableModels with empty JSON body defaults to daily before prod."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR01Empty12",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer rollout-r01-empty-token",
                "Content-Type": "application/json",
            },
            body=b"{}",
            timeout=5.0,
        )
        self.assertEqual(resp.status, 502, resp.text)
        targets = connect_proxy.wait_for_connects(2, timeout=3.0)
        self.assertGreaterEqual(len(targets), 2, targets)
        self.assertEqual(targets[0], "daily-cloudcode-pa.googleapis.com:443")
        self.assertEqual(targets[1], "cloudcode-pa.googleapis.com:443")


class TestTier1RolloutR02EnterpriseGcpTos(E2ETestCase):
    TIER = 1
    FEATURE = "R02"

    def test_t1_r02_entitlement_user_tier_and_auth_method_select_expected_host(self) -> None:
        """R02: Enterprise userTier and auth_method=gcp|adc route prod first; auth_method=consumer routes daily."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR02Tier123",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )

        cases: list[tuple[str, dict[str, str], bytes, str, str]] = [
            (
                "entitlement_enterprise",
                {"Authorization": "Bearer r02-ent-tok", "Content-Type": "application/json"},
                b'{"entitlement":{"userTier":"enterprise"}}',
                "cloudcode-pa.googleapis.com:443",
                "daily-cloudcode-pa.googleapis.com:443",
            ),
            (
                "ua_auth_method_gcp",
                {
                    "Authorization": "Bearer r02-gcp-tok",
                    "User-Agent": "antigravity/2.34.0 auth_method=gcp",
                    "Content-Type": "application/json",
                },
                b"{}",
                "cloudcode-pa.googleapis.com:443",
                "daily-cloudcode-pa.googleapis.com:443",
            ),
            (
                "ua_auth_method_adc",
                {
                    "Authorization": "Bearer r02-adc-tok",
                    "User-Agent": "antigravity/2.34.0 auth_method=adc",
                    "Content-Type": "application/json",
                },
                b"{}",
                "cloudcode-pa.googleapis.com:443",
                "daily-cloudcode-pa.googleapis.com:443",
            ),
            (
                "ua_auth_method_consumer",
                {
                    "Authorization": "Bearer r02-consumer-tok",
                    "User-Agent": "antigravity/2.34.0 auth_method=consumer",
                    "Content-Type": "application/json",
                },
                b'{"project":"managed-caic-999"}',
                "daily-cloudcode-pa.googleapis.com:443",
                "cloudcode-pa.googleapis.com:443",
            ),
        ]
        for label, hdrs, body, first_host, second_host in cases:
            with self.subTest(case=label):
                connect_proxy.clear_targets()
                resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1internal:fetchAvailableModels",
                    headers=hdrs,
                    body=body,
                    timeout=5.0,
                )
                self.assertEqual(resp.status, 502, resp.text)
                targets = connect_proxy.wait_for_connects(2, timeout=3.0)
                self.assertGreaterEqual(len(targets), 2, f"{label}: {targets!r}")
                self.assertEqual(targets[0], first_host, f"{label}: {targets!r}")
                self.assertEqual(targets[1], second_host, f"{label}: {targets!r}")

    def test_t1_r02_load_code_assist_routes_prod_first_with_daily_fallback(self) -> None:
        """R02: Auto loadCodeAssist probes prod first and falls back to daily on failure."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR02Lca1234",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:loadCodeAssist",
            headers={
                "Authorization": "Bearer r02-lca-tok",
                "Content-Type": "application/json",
            },
            body=b'{"metadata":{"ideType":"ANTIGRAVITY"}}',
            timeout=5.0,
        )
        self.assertEqual(resp.status, 502, resp.text)
        targets = connect_proxy.wait_for_connects(2, timeout=3.0)
        self.assertGreaterEqual(len(targets), 2, targets)
        self.assertEqual(targets[0], "cloudcode-pa.googleapis.com:443")
        self.assertEqual(targets[1], "daily-cloudcode-pa.googleapis.com:443")


class TestTier1RolloutR03SipHashLruHotReload(E2ETestCase):
    TIER = 1
    FEATURE = "R03"

    def test_t1_r03_per_account_lru_isolates_strip_user_project_across_tokens(self) -> None:
        """R03: Account A strip_user_project cache entry does not leak to Account B."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()

        def handler(req: CapturedRequest) -> Any:
            if ":fetchAvailableModels" in req.path:
                if req.header("x-goog-user-project") is not None:
                    return JsonAction(status=403, payload={"error": {"message": "forbidden user project"}})
                return JsonAction(status=200, payload=_sample_ccpa_catalog())
            return SseStreamAction(chunks=[b'data: {"ok":true}\n\n'], status=200)

        mock_ccpa.set_handler(handler)
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR03Lru1234",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # Token A learns strip_user_project=true via metadata 403 retry.
        r_a_meta = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer r03-account-alpha",
                "x-goog-user-project": "proj-alpha",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r_a_meta.status, 200, r_a_meta.text)
        mock_ccpa.clear_requests()

        # Subsequent stream for Token A strips x-goog-user-project.
        r_a_stream = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r03-account-alpha",
                "x-goog-user-project": "proj-alpha",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(r_a_stream.status, 200, r_a_stream.text)

        # Subsequent stream for Token B preserves x-goog-user-project.
        r_b_stream = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r03-account-beta",
                "x-goog-user-project": "proj-beta",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(r_b_stream.status, 200, r_b_stream.text)

        self.assertEqual(len(mock_ccpa.requests), 2)
        self.assertIsNone(mock_ccpa.requests[0].header("x-goog-user-project"))
        self.assertEqual(mock_ccpa.requests[1].header("x-goog-user-project"), "proj-beta")

    def test_t1_r03_hot_reload_preserves_routing_cache_when_cloudcode_unchanged(self) -> None:
        """R03: Config hot-reload changing gemini settings preserves CloudCode per-account LRU."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        mock_ccpa = self.start_mock_gemini()

        def handler(req: CapturedRequest) -> Any:
            if ":fetchAvailableModels" in req.path:
                if req.header("x-goog-user-project") is not None:
                    return JsonAction(status=403, payload={"error": {"message": "strip needed"}})
                return JsonAction(status=200, payload=_sample_ccpa_catalog())
            return SseStreamAction(chunks=[b'data: {"reloaded":true}\n\n'], status=200)

        mock_ccpa.set_handler(handler)
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key="AIzaSyRolloutR03Init123",
                default_model="gemini-4-hydrogen",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        resp1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer r03-hotreload-token",
                "x-goog-user-project": "proj-hr",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(resp1.status, 200, resp1.text)
        mock_ccpa.clear_requests()

        # Hot-reload gemini api_key and default_model while keeping cloudcode_upstream_url & timeout_secs constant.
        reloaded_key = "AIzaSyRolloutR03Next456"
        env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key=reloaded_key,
                default_model="gemini-4-argon",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)

        resp2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r03-hotreload-token",
                "x-goog-user-project": "proj-hr",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(resp2.status, 200, resp2.text)
        self.assertEqual(len(mock_ccpa.requests), 1)
        self.assertIsNone(
            mock_ccpa.requests[0].header("x-goog-user-project"),
            "Hot-reload wiped CloudCode per-account LRU cache!",
        )

        # Verify hot-reload actually took effect by confirming the new api_key reaches mock_gemini.
        mock_gemini.clear_requests()
        mock_gemini.enqueue(JsonAction(status=200, payload={"candidates": []}))
        gen_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[{"parts":[{"text":"verify-hot-reload"}]}]}',
        )
        self.assertEqual(gen_resp.status, 200, gen_resp.text)
        self.assertEqual(len(mock_gemini.requests), 1)
        self.assertEqual(
            mock_gemini.requests[0].header("x-goog-api-key"),
            reloaded_key,
            "Expected hot-reloaded api_key to reach mock_gemini",
        )


class TestTier1RolloutR04Enterprise403Strip(E2ETestCase):
    TIER = 1
    FEATURE = "R04"

    def test_t1_r04_metadata_403_retries_once_without_user_project_and_caches(self) -> None:
        """R04: Metadata 403 retries once without x-goog-user-project on fetchAvailableModels and loadCodeAssist."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()

        def handler(req: CapturedRequest) -> Any:
            if req.header("x-goog-user-project") is not None:
                return JsonAction(status=403, payload={"error": {"message": "enterprise 403"}})
            if ":loadCodeAssist" in req.path:
                return JsonAction(status=200, payload={"currentTier": {"id": "gcp-standard-tier"}})
            return JsonAction(status=200, payload=_sample_ccpa_catalog())

        mock_ccpa.set_handler(handler)
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR04Meta123",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # 1. fetchAvailableModels 403 -> retry without x-goog-user-project -> 200
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer r04-fetch-token",
                "x-goog-user-project": "proj-ent-fetch",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r1.status, 200, r1.text)
        self.assertEqual(len(mock_ccpa.requests), 2)
        self.assertEqual(mock_ccpa.requests[0].header("x-goog-user-project"), "proj-ent-fetch")
        self.assertIsNone(mock_ccpa.requests[1].header("x-goog-user-project"))

        # 2. Separate token on loadCodeAssist 403 -> retry without x-goog-user-project -> 200
        mock_ccpa.clear_requests()
        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:loadCodeAssist",
            headers={
                "Authorization": "Bearer r04-lca-token",
                "x-goog-user-project": "proj-ent-lca",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r2.status, 200, r2.text)
        self.assertEqual(len(mock_ccpa.requests), 2)
        self.assertEqual(mock_ccpa.requests[0].header("x-goog-user-project"), "proj-ent-lca")
        self.assertIsNone(mock_ccpa.requests[1].header("x-goog-user-project"))

    def test_t1_r04_entitlement_user_tier_strips_user_project_proactively(self) -> None:
        """R04: Request with entitlement.userTier strips x-goog-user-project on first attempt without a 403."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_ccpa_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR04Proact1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer r04-proactive-token",
                "x-goog-user-project": "proj-ent-proactive",
                "Content-Type": "application/json",
            },
            body=b'{"entitlement":{"userTier":"enterprise"}}',
        )
        self.assertEqual(resp.status, 200, resp.text)
        self.assertEqual(len(mock_ccpa.requests), 1)
        self.assertIsNone(mock_ccpa.requests[0].header("x-goog-user-project"))


class TestTier1RolloutR05TransportHeaderFidelity(E2ETestCase):
    TIER = 1
    FEATURE = "R05"

    def test_t1_r05_forwards_request_reason_and_content_encoding_bypassing_proxy_on_loopback(self) -> None:
        """R05: Forwards x-goog-request-reason and content-encoding verbatim while loopback bypasses HTTPS_PROXY."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(SseStreamAction(chunks=[b'data: {"ok":true}\n\n'], status=200))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR05Hdrs123",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        # Hostile HTTP_PROXY / HTTPS_PROXY must be ignored for loopback Fixed upstream.
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={
                "HTTP_PROXY": "http://127.0.0.1:1",
                "HTTPS_PROXY": "http://127.0.0.1:1",
            },
        )
        gz_payload = gzip.compress(b'{"model":"gemini-4-carbon-high"}')
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r05-hdr-token",
                "Content-Type": "application/json",
                "Content-Encoding": "gzip",
                "x-goog-request-reason": "rollout-r05-test-reason",
            },
            body=gz_payload,
        )
        self.assertEqual(resp.status, 200, resp.text)
        self.assertEqual(len(mock_ccpa.requests), 1)
        captured = mock_ccpa.requests[0]
        self.assertEqual(captured.header("x-goog-request-reason"), "rollout-r05-test-reason")
        self.assertEqual(captured.header("content-encoding"), "gzip")
        self.assertEqual(captured.body, gz_payload)

    def test_t1_r05_day0_preserves_response_content_encoding_and_merged_strips_it(self) -> None:
        """R05: Day-0 untouched response preserves upstream Content-Encoding; merged JSON strips it."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        raw_catalog_json = json.dumps(_sample_ccpa_catalog()).encode("utf-8")
        gz_catalog = gzip.compress(raw_catalog_json)

        # 1. Day-0 (unconfigured api_key): preserves gzip bytes and Content-Encoding header verbatim.
        mock_ccpa.enqueue(
            RawBytesAction(
                status=200,
                content_type="application/json",
                body=gz_catalog,
                headers={"Content-Encoding": "gzip"},
            )
        )
        cfg = env.write_config(
            env.make_config_dict(
                api_key=None,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        day0_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r05-enc-token", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(day0_resp.status, 200, day0_resp.text)
        self.assertEqual((day0_resp.header("content-encoding") or "").lower(), "gzip")
        self.assertEqual(day0_resp.body, gz_catalog)

        # 2. Configured real key with plain JSON + explicit Content-Encoding: identity -> merged JSON strips it.
        mock_ccpa.enqueue(
            RawBytesAction(
                status=200,
                content_type="application/json",
                body=raw_catalog_json,
                headers={"Content-Encoding": "identity"},
            )
        )
        env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR05Merge12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)
        merged_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r05-enc-token", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(merged_resp.status, 200, merged_resp.text)
        self.assertIsNone(merged_resp.header("content-encoding"))
        self.assertIn("companion/gemini-4-hydrogen-low", merged_resp.json()["models"])


class TestTier1RolloutR06DeterministicSlotsNeonXHigh(E2ETestCase):
    TIER = 1
    FEATURE = "R06"

    def test_t1_r06_gemini_4_neon_includes_xhigh_thinking_level_7(self) -> None:
        """R06: Merged catalog exposes gemini-4-neon with 4 levels including XHigh thinkingLevel 7."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_ccpa_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR06Neon123",
                default_model="gemini-4-neon",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r06-neon-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200, resp.text)
        self.assertEqual(resp.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", resp.header("cache-control") or "")
        data = resp.json()
        models = data["models"]

        expected_levels = {"low": 1, "medium": 2, "high": 3, "xhigh": 7}
        for suffix, level in expected_levels.items():
            mid = f"companion/gemini-4-neon-{suffix}"
            self.assertIn(mid, models, f"Missing {mid} in merged catalog")
            self.assertEqual(models[mid]["thinkingLevel"], level)
        self.assertTrue(
            models["companion/gemini-4-neon-xhigh"]["displayName"].endswith("(XHigh)"),
            models["companion/gemini-4-neon-xhigh"],
        )
        grouped = data["groupedModels"]["companion/gemini-4-neon"]
        levels = grouped["supportedThinkingLevels"]
        self.assertIn({"thinkingSuffix": "xhigh", "thinkingDisplayName": "Extra"}, levels)

    def test_t1_r06_placeholder_slots_remain_stable_across_default_model_changes(self) -> None:
        """R06: Injected placeholder slots stay identical in M334..=M365 when default_model changes."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=200, payload=_sample_ccpa_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR06Slot123",
                default_model="gemini-4-hydrogen",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r06-slot-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r1.status, 200, r1.text)
        slots_first = {
            mid: details["model"]
            for mid, details in r1.json()["models"].items()
            if mid.startswith("companion/")
        }
        self.assertTrue(slots_first, "Expected injected companion/* models")
        self.assertEqual(len(slots_first.values()), len(set(slots_first.values())))
        for mid, enum_val in slots_first.items():
            m = _PLACEHOLDER_SLOT_RE.match(enum_val)
            self.assertIsNotNone(m, f"{mid} has unexpected slot {enum_val!r}")
            assert m is not None
            slot_num = int(m.group(1))
            self.assertGreaterEqual(slot_num, 334)
            self.assertLessEqual(slot_num, 365)
            self.assertNotEqual(enum_val, "MODEL_PLACEHOLDER_M334", "Must avoid upstream M334 collision")

        # Change default_model via hot-reload and re-fetch.
        env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR06Slot123",
                default_model="gemini-4-argon",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r06-slot-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r2.status, 200, r2.text)
        slots_second = {
            mid: details["model"]
            for mid, details in r2.json()["models"].items()
            if mid.startswith("companion/")
        }
        shared = set(slots_first) & set(slots_second)
        self.assertTrue(shared)
        for mid in sorted(shared):
            self.assertEqual(
                slots_first[mid],
                slots_second[mid],
                f"Slot shifted for {mid}: {slots_first[mid]} -> {slots_second[mid]}",
            )


class TestTier1RolloutR07PlaceholderKeyQuarantine(E2ETestCase):
    TIER = 1
    FEATURE = "R07"

    def test_t1_r07_proxy_and_mcp_quarantine_placeholder_api_key(self) -> None:
        """R07: Proxy and ui-mcp treat agy-companion-local as unconfigured and never forward it upstream."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=find_free_port(),
                base_url=mock_gemini.base_url,
                api_key=_PLACEHOLDER_KEY,
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"GEMINI_API_KEY": _PLACEHOLDER_KEY},
        )
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={
                "Content-Type": "application/json",
                "x-goog-api-key": _PLACEHOLDER_KEY,
            },
            body=b'{"contents":[{"parts":[{"text":"hi"}]}]}',
        )
        self.assertEqual(resp.status, 401, resp.text)
        self.assertEqual(len(mock_gemini.requests), 0, "Placeholder key leaked upstream!")

        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"GEMINI_API_KEY": _PLACEHOLDER_KEY},
        )
        gs = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = gs.get("structuredContent", {})
        self.assertIs(sc["config"]["api_key_configured"], False)
        self.assertIsNone(sc["config"]["api_key_hint"])

        tc = mcp.call_tool("test_connection", {})
        mcp.assert_clean_stdout()
        tc_sc = tc.get("structuredContent", {})
        self.assertIs(tc_sc.get("ok"), False, tc_sc)
        self.assertEqual(len(mock_gemini.requests), 0, "test_connection leaked placeholder upstream!")

    def test_t1_r07_doctor_quarantines_placeholder_gemini_api_key_env(self) -> None:
        """R07: doctor --json does not treat GEMINI_API_KEY=agy-companion-local as a configured key."""
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(listen_port=find_free_port(), api_key=None))
        res = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--json"],
            env=env.build_env(extra={"GEMINI_API_KEY": _PLACEHOLDER_KEY}),
        )
        data = res.json_stdout()
        self.assertIs(data["env"]["gemini_api_key_set"], False, data["env"])
        checks_by_name = {c["name"]: c for c in data.get("checks", [])}
        if "env_gemini_api_key" in checks_by_name:
            self.assertNotEqual(
                checks_by_name["env_gemini_api_key"]["status"],
                "pass",
                checks_by_name["env_gemini_api_key"],
            )


class TestTier1RolloutR08Day0UntouchedCatalog(E2ETestCase):
    TIER = 1
    FEATURE = "R08"

    def test_t1_r08_day0_unconfigured_returns_upstream_catalog_verbatim(self) -> None:
        """R08: Without a real API key, fetchAvailableModels returns upstream JSON untouched without Proxy models."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        raw_upstream = json.dumps(_sample_ccpa_catalog(), separators=(",", ":")).encode("utf-8")
        mock_ccpa.enqueue(
            RawBytesAction(
                status=200,
                content_type="application/json; charset=utf-8",
                body=raw_upstream,
            )
        )
        cfg = env.write_config(
            env.make_config_dict(
                api_key=None,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r08-day0-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200, resp.text)
        self.assertEqual(resp.body, raw_upstream)
        self.assertNotIn("(Proxy)", resp.text)
        self.assertNotIn("companion/", resp.text)

        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(stats["last_catalog_included_proxy_models"], False)
        self.assertIsInstance(stats["last_catalog_refresh_at_ms"], int)
        self.assertGreater(stats["last_catalog_refresh_at_ms"], 0)

    def test_t1_r08_saving_real_key_transitions_day0_to_merged_catalog(self) -> None:
        """R08: Adding a real API key via hot-reload switches fetchAvailableModels from untouched to merged."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=200, payload=_sample_ccpa_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key=None,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        r_day0 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r08-trans-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_day0.status, 200, r_day0.text)
        self.assertNotIn("companion/gemini-4-hydrogen-low", r_day0.json()["models"])

        env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR08DayOne1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)

        r_day1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r08-trans-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_day1.status, 200, r_day1.text)
        self.assertIn("companion/gemini-4-hydrogen-low", r_day1.json()["models"])
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(stats["last_catalog_included_proxy_models"], True)


class TestTier1RolloutR09ExtendedApiStats(E2ETestCase):
    TIER = 1
    FEATURE = "R09"

    def test_t1_r09_stats_exposes_catalog_and_cloudcode_fields_in_fixed_mode(self) -> None:
        """R09: /api/stats exposes all 6 legacy and 4 new catalog/cloudcode fields with security headers."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_ccpa_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR09Stats12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        resp0 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats")
        self.assertEqual(resp0.status, 200, resp0.text)
        self.assertEqual(resp0.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", resp0.header("cache-control") or "")
        s0 = resp0.json()
        required = {
            "requests",
            "prompt_tokens",
            "cached_tokens",
            "output_tokens",
            "reasoning_tokens",
            "cache_hit_ratio",
            "last_catalog_refresh_at_ms",
            "last_catalog_included_proxy_models",
            "cloudcode_mode",
            "cloudcode_resolved_upstream",
        }
        self.assertTrue(required.issubset(set(s0.keys())), f"Missing keys: {required - set(s0.keys())}")
        self.assertIsNone(s0["last_catalog_refresh_at_ms"])
        self.assertIsNone(s0["last_catalog_included_proxy_models"])
        self.assertEqual(s0["cloudcode_mode"], "fixed")
        self.assertIsInstance(s0["cloudcode_resolved_upstream"], str)
        self.assertTrue(s0["cloudcode_resolved_upstream"])

        fetch = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r09-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(fetch.status, 200, fetch.text)

        s1 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIsInstance(s1["last_catalog_refresh_at_ms"], int)
        self.assertGreater(s1["last_catalog_refresh_at_ms"], 0)
        self.assertIs(s1["last_catalog_included_proxy_models"], True)

    def test_t1_r09_stats_auto_mode_initial_state_and_mcp_legacy_6_field_compat(self) -> None:
        """R09: Auto mode reports cloudcode_mode=auto and ui-mcp accepts legacy 6-field /api/stats."""
        env = self.make_env()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutR09Auto123",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        s_auto = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(s_auto["cloudcode_mode"], "auto")
        self.assertIsNone(s_auto["cloudcode_resolved_upstream"])
        proxy.stop()

        # Legacy 6-field MockHealthServer must deserialize cleanly in ui-mcp.
        legacy_health = self.start_mock_health()
        cfg_legacy = env.write_config(
            env.make_config_dict(listen_port=legacy_health.port, api_key="AIzaSyRolloutR09Leg1234"),
            path=env.root / "legacy_cfg.json",
        )
        mcp = self.start_mcp(env, config_path=cfg_legacy)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = res.get("structuredContent", {})
        self.assertTrue(sc["proxy"]["reachable"])
        self.assertIsInstance(sc.get("stats"), dict)


class TestTier1RolloutR10NonGemini34Hint(E2ETestCase):
    TIER = 1
    FEATURE = "R10"

    def test_t1_r10_legacy_gemini_25_pro_400_includes_unset_base_url_hint(self) -> None:
        """R10: Non-Gemini-3/4 generateContent returns 400 with unset GOOGLE_GEMINI_BASE_URL hint."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key="AIzaSyRolloutR10Hint123",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[{"parts":[{"text":"hello"}]}]}',
        )
        self.assertEqual(resp.status, 400, resp.text)
        self.assertEqual(resp.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", resp.header("cache-control") or "")
        msg = resp.json()["error"]["message"]
        self.assertIn("Gemini 3 and Gemini 4", msg)
        self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", msg)
        self.assertEqual(len(mock_gemini.requests), 0)

    def test_t1_r10_stream_and_foreign_models_400_include_unset_base_url_hint(self) -> None:
        """R10: Streaming and foreign model requests return 400 with unset GOOGLE_GEMINI_BASE_URL hint."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key="AIzaSyRolloutR10For1234",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        for path in (
            "/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse",
            "/v1beta/models/gpt-4o:generateContent",
        ):
            with self.subTest(path=path):
                resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    path,
                    headers={"Content-Type": "application/json"},
                    body=b"{}",
                )
                self.assertEqual(resp.status, 400, resp.text)
                msg = resp.json()["error"]["message"]
                self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", msg)
        self.assertEqual(len(mock_gemini.requests), 0)


class TestTier1RolloutR11HealthProbeAdoptRunning(E2ETestCase):
    TIER = 1
    FEATURE = "R11"

    def test_t1_r11_serve_adopt_running_takes_over_promptly_when_primary_exits(self) -> None:
        """R11: serve --adopt-running polls tightly and takes over the port in under 2.5s after primary stops."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        primary = self.start_proxy(env, config_path=cfg, port=port)

        adopter = subprocess.Popen(
            [str(binary), "serve", "--port", str(port), "--config", str(cfg), "--adopt-running"],
            env=env.build_env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            time.sleep(0.4)
            self.assertIsNone(adopter.poll(), "Adopter exited prematurely while primary was healthy")
            primary.stop()
            t0 = time.monotonic()
            health = _wait_for_health(port, timeout=2.5)
            elapsed = time.monotonic() - t0
            self.assertIsNotNone(health, "Adopter failed to take over port within 2.5s")
            self.assertLess(elapsed, 2.5)
            self.assertIsNone(adopter.poll(), "Adopter should own the port now")
        finally:
            if adopter.poll() is None:
                adopter.send_signal(signal.SIGTERM)
                try:
                    adopter.wait(timeout=3.0)
                except subprocess.TimeoutExpired:
                    adopter.kill()
                    adopter.wait(timeout=2.0)

    def test_t1_r11_serve_adopt_running_exits_zero_on_sigterm_while_parked(self) -> None:
        """R11: serve --adopt-running parks cleanly behind a healthy proxy and exits 0 on SIGTERM."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        _primary = self.start_proxy(env, config_path=cfg, port=port)

        adopter = subprocess.Popen(
            [str(binary), "serve", "--port", str(port), "--config", str(cfg), "--adopt-running"],
            env=env.build_env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            time.sleep(0.5)
            self.assertIsNone(adopter.poll())
            adopter.send_signal(signal.SIGTERM)
            rc = adopter.wait(timeout=5.0)
            self.assertEqual(rc, 0)
        finally:
            if adopter.poll() is None:
                adopter.kill()
                adopter.wait(timeout=2.0)


class TestTier1RolloutR12ManagedDaemonIdlePidLog(E2ETestCase):
    TIER = 1
    FEATURE = "R12"

    def test_t1_r12_serve_idle_shutdown_writes_0600_pidfile_and_exits_cleanly(self) -> None:
        """R12: serve --idle-shutdown-secs writes 0600 JSON proxy.pid, self-exits 0 after idle, and removes pidfile."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        proc = subprocess.Popen(
            [
                str(binary),
                "serve",
                "--port",
                str(port),
                "--config",
                str(cfg),
                "--idle-shutdown-secs",
                "2",
            ],
            env=env.build_env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            health = _wait_for_health(port, timeout=5.0)
            self.assertIsNotNone(health, "Idle-shutdown proxy did not become healthy")
            pid_path = env.data_dir / "proxy.pid"
            self.assertTrue(pid_path.is_file() and not pid_path.is_symlink(), f"Missing {pid_path}")
            if os.name != "nt":
                self.assertEqual(stat.S_IMODE(os.lstat(pid_path).st_mode), 0o600)
            pid_data = json.loads(pid_path.read_text(encoding="utf-8"))
            self.assertEqual(pid_data["pid"], proc.pid)
            self.assertEqual(pid_data["port"], port)
            self.assertIsInstance(pid_data["version"], str)
            self.assertGreater(pid_data["started_at_ms"], 0)

            rc = proc.wait(timeout=8.0)
            self.assertEqual(rc, 0, f"Expected clean idle exit 0, got {rc}")
            self.assertFalse(pid_path.exists(), "proxy.pid should be removed on clean shutdown")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=2.0)

    def test_t1_r12_active_sse_stream_prevents_premature_idle_shutdown(self) -> None:
        """R12: Active SSE stream outliving --idle-shutdown-secs completes intact before proxy self-exits."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        mock_gemini = self.start_mock_gemini()
        chunks = [
            b'data: {"candidates":[{"content":{"parts":[{"text":"part1"}]}}]}\n\n',
            b'data: {"candidates":[{"content":{"parts":[{"text":"part2"}]}}]}\n\n',
            b'data: {"candidates":[{"content":{"parts":[{"text":"part3"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}\n\n',
        ]
        mock_gemini.enqueue(SseStreamAction(chunks=chunks, chunk_delay_s=1.2))
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                base_url=mock_gemini.base_url,
                api_key="AIzaSyRolloutR12Sse1234",
            )
        )

        proc = subprocess.Popen(
            [
                str(binary),
                "serve",
                "--port",
                str(port),
                "--config",
                str(cfg),
                "--idle-shutdown-secs",
                "2",
            ],
            env=env.build_env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            self.assertIsNotNone(_wait_for_health(port, timeout=5.0))
            received = bytearray()
            with http_stream_request(
                "127.0.0.1",
                port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
                headers={"Content-Type": "application/json"},
                body=b'{"contents":[{"parts":[{"text":"stream"}]}]}',
                timeout=10.0,
            ) as stream:
                self.assertEqual(stream.status, 200)
                for chunk, _ts in stream.iter_chunks():
                    received.extend(chunk)
            self.assertEqual(bytes(received), b"".join(chunks))
            rc = proc.wait(timeout=8.0)
            self.assertEqual(rc, 0)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=2.0)


class TestTier1RolloutR13IntegrationEnvSh(E2ETestCase):
    TIER = 1
    FEATURE = "R13"

    def test_t1_r13_enable_writes_0700_0600_zero_eval_env_sh_valid_across_shells(self) -> None:
        """R13: integration enable writes 0700/0600 zero-eval env.sh passing sh, /bin/bash, and zsh syntax checks."""
        if os.name == "nt":
            self.skipTest("POSIX env.sh test")
        binary = self.require_binary()
        env = self.make_env()
        fresh_data = env.root / "fresh_data"
        res = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--json"],
            env=env.build_env(
                extra={
                    "SHELL": "/bin/zsh",
                    "ZDOTDIR": str(env.home_dir),
                    "XDG_DATA_HOME": str(fresh_data),
                }
            ),
        )
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        env_sh = fresh_data / "agy-companion" / "env.sh"
        self.assertTrue(env_sh.is_file() and not env_sh.is_symlink())
        self.assertEqual(stat.S_IMODE(os.lstat(env_sh).st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.lstat(env_sh.parent).st_mode), 0o700)

        text = env_sh.read_text(encoding="utf-8")
        self.assertIsNone(
            re.search(r"(?m)^[^#]*\beval\b", text),
            "env.sh must contain zero eval statements",
        )
        for shell_cmd in (["/bin/sh", "-n", str(env_sh)], ["/bin/bash", "-n", str(env_sh)]):
            chk = run_cli(shell_cmd)
            self.assertEqual(chk.returncode, 0, f"{shell_cmd} failed: {chk.stderr}")
        if Path("/bin/zsh").is_file():
            chk_zsh = run_cli(["/bin/zsh", "-n", str(env_sh)])
            self.assertEqual(chk_zsh.returncode, 0, chk_zsh.stderr)

    def test_t1_r13_sourcing_env_sh_activates_and_scrubs_across_posix_shells(self) -> None:
        """R13: Sourcing env.sh activates on ANTIGRAVITY_RESOLVING_ENVIRONMENT=1 and scrubs in child terminal."""
        if os.name == "nt":
            self.skipTest("POSIX env.sh test")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        _install_dummy_plugin_json(env)

        en = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--binary", str(binary), "--json"],
            env=env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}),
        )
        self.assertEqual(en.returncode, 0, en.stderr)
        env_sh = env.data_dir / "env.sh"

        shells = ["/bin/sh", "/bin/bash"]
        if Path("/bin/zsh").is_file():
            shells.append("/bin/zsh")

        for sh_bin in shells:
            with self.subTest(shell=sh_bin):
                sh_prefix = [sh_bin, "-f", "-c"] if sh_bin.endswith("zsh") else [sh_bin, "-c"]
                # 1. Plain shell without ANTIGRAVITY_RESOLVING_ENVIRONMENT is a no-op.
                plain = run_cli(
                    [*sh_prefix, '. "$1"; printf "%s|%s" "${CLOUD_CODE_URL:-}" "${AGY_COMPANION_HOOK_ACTIVE:-}"', "_", str(env_sh)],
                    env=env.build_env(extra={"ZDOTDIR": str(env.home_dir)}),
                )
                self.assertEqual(plain.returncode, 0, plain.stderr)
                self.assertEqual(plain.stdout, "|")

                # 2. Activation + integrated terminal scrub in child shell.
                script = (
                    '. "$1"\n'
                    'printf "ACT:%s|%s|%s|%s|%s|%s\\n" '
                    '"${CLOUD_CODE_URL:-}" "${GOOGLE_GEMINI_BASE_URL:-}" "${GEMINI_API_KEY:-}" '
                    '"${AGY_COMPANION_HOOK_ACTIVE:-}" "${JETSKI_MCP_ENV_RESTORE_PREFIX:-}" "${JETSKI_MCP_ENV_RESTORE_VARS:-}"\n'
                    'unset ANTIGRAVITY_RESOLVING_ENVIRONMENT\n'
                    '. "$1"\n'
                    'printf "SCRUB:%s|%s|%s|%s\\n" '
                    '"${CLOUD_CODE_URL-<UNSET>}" "${GOOGLE_GEMINI_BASE_URL-<UNSET>}" '
                    '"${GEMINI_API_KEY-<UNSET>}" "${AGY_COMPANION_HOOK_ACTIVE-<UNSET>}"\n'
                )
                act = run_cli(
                    [*sh_prefix, script, "_", str(env_sh)],
                    env=env.build_env(
                        extra={
                            "ZDOTDIR": str(env.home_dir),
                            "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                            "AGY_COMPANION_CONFIG": str(cfg),
                        }
                    ),
                )
                self.assertEqual(act.returncode, 0, act.stderr)
                lines = act.stdout.splitlines()
                self.assertGreaterEqual(len(lines), 2, act.stdout)
                act_fields = lines[0].removeprefix("ACT:").split("|")
                self.assertEqual(act_fields[0], f"http://127.0.0.1:{port}")
                self.assertEqual(act_fields[1], f"http://127.0.0.1:{port}")
                self.assertEqual(act_fields[2], _PLACEHOLDER_KEY)
                self.assertEqual(act_fields[3], "1")
                self.assertTrue(act_fields[4], "Expected non-empty JETSKI_MCP_ENV_RESTORE_PREFIX")
                self.assertIn("CLOUD_CODE_URL", act_fields[5])
                self.assertIn("GOOGLE_GEMINI_BASE_URL", act_fields[5])
                self.assertIn("GEMINI_API_KEY", act_fields[5])
                self.assertNotIn("GOOGLE_API_KEY", act_fields[5])
                self.assertEqual(lines[1], "SCRUB:<UNSET>|<UNSET>|<UNSET>|<UNSET>")


class TestTier1RolloutR14ShellRcHooksSafety(E2ETestCase):
    TIER = 1
    FEATURE = "R14"

    def test_t1_r14_zsh_bash_fish_rc_targets_and_idempotent_enable_disable(self) -> None:
        """R14: Resolves Zsh .zlogin, Bash .profile/existing, Fish conf.d drop-in cleanly and toggles idempotently."""
        if os.name == "nt":
            self.skipTest("POSIX shell RC test")
        binary = self.require_binary()
        env = self.make_env()

        # 1. Zsh -> ${ZDOTDIR:-$HOME}/.zlogin (never touches .zshrc)
        custom_zdot = env.home_dir / "zdot"
        custom_zdot.mkdir(parents=True, exist_ok=True)
        z_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(custom_zdot)})
        for _ in range(2):
            r = run_cli([str(binary), "integration", "enable", "--shell", "zsh", "--json"], env=z_env)
            self.assertEqual(r.returncode, 0, r.stderr)
        zlogin = custom_zdot / ".zlogin"
        self.assertTrue(zlogin.is_file())
        self.assertFalse((custom_zdot / ".zshrc").exists())
        ztext = zlogin.read_text(encoding="utf-8")
        self.assertEqual(ztext.count("# >>> agy-companion"), 1)
        self.assertEqual(ztext.count("# <<< agy-companion <<<"), 1)
        r_dis = run_cli([str(binary), "integration", "disable", "--shell", "zsh", "--json"], env=z_env)
        self.assertEqual(r_dis.returncode, 0, r_dis.stderr)
        self.assertFalse(zlogin.exists(), "Empty .zlogin should be deleted on disable")

        # 2. Bash fallback -> ~/.profile (never creates ~/.bash_profile when absent)
        b_env = env.build_env(extra={"SHELL": "/bin/bash"})
        rb = run_cli([str(binary), "integration", "enable", "--shell", "bash", "--json"], env=b_env)
        self.assertEqual(rb.returncode, 0, rb.stderr)
        self.assertTrue((env.home_dir / ".profile").is_file())
        self.assertFalse((env.home_dir / ".bash_profile").exists())

        # 3. Fish -> ${XDG_CONFIG_HOME}/fish/conf.d/agy-companion.fish (zero | source, zero eval)
        f_env = env.build_env(extra={"SHELL": "/usr/bin/fish"})
        rf = run_cli([str(binary), "integration", "enable", "--shell", "fish", "--json"], env=f_env)
        self.assertEqual(rf.returncode, 0, rf.stderr)
        fish_dropin = env.config_root / "fish" / "conf.d" / "agy-companion.fish"
        self.assertTrue(fish_dropin.is_file())
        ftext = fish_dropin.read_text(encoding="utf-8")
        self.assertNotIn("| source", ftext)
        self.assertIsNone(re.search(r"(?m)^[^#]*\beval\b", ftext))
        if shutil.which("fish"):
            fchk = run_cli(["fish", "-n", str(fish_dropin)])
            self.assertEqual(fchk.returncode, 0, fchk.stderr)
        rf_dis = run_cli([str(binary), "integration", "disable", "--shell", "fish", "--json"], env=f_env)
        self.assertEqual(rf_dis.returncode, 0, rf_dis.stderr)
        self.assertFalse(fish_dropin.exists())

    def test_t1_r14_enable_and_disable_preserve_user_rc_content_and_crlf(self) -> None:
        """R14: Enabling and disabling RC hook preserves surrounding user lines and CRLF line endings."""
        if os.name == "nt":
            self.skipTest("POSIX shell RC test")
        binary = self.require_binary()
        env = self.make_env()
        profile = env.home_dir / ".profile"
        original = b"export USER_KEEP_A=1\r\nexport USER_KEEP_B=two\r\n"
        profile.write_bytes(original)

        b_env = env.build_env(extra={"SHELL": "/bin/bash"})
        r_en = run_cli([str(binary), "integration", "enable", "--shell", "bash", "--json"], env=b_env)
        self.assertEqual(r_en.returncode, 0, r_en.stderr)
        after_en = profile.read_bytes()
        self.assertIn(b"export USER_KEEP_A=1\r\n", after_en)
        self.assertIn(b"# >>> agy-companion", after_en)

        r_dis = run_cli([str(binary), "integration", "disable", "--shell", "bash", "--json"], env=b_env)
        self.assertEqual(r_dis.returncode, 0, r_dis.stderr)
        after_dis = profile.read_bytes()
        self.assertEqual(after_dis, original)


class TestTier1RolloutR15HookEnvDaemon(E2ETestCase):
    TIER = 1
    FEATURE = "R15"

    def test_t1_r15_hook_env_cold_starts_detached_daemon_and_prints_port(self) -> None:
        """R15: hook-env --ensure-running --print-port cold-starts detached daemon and prints validated port."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        _install_dummy_plugin_json(env)

        res = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), str(port))
        health = _wait_for_health(port, timeout=3.0)
        self.assertIsNotNone(health, "Detached daemon from hook-env did not serve /health")
        pid_path = env.data_dir / "proxy.pid"
        self.assertTrue(pid_path.is_file() and not pid_path.is_symlink())
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(pid_path).st_mode), 0o600)

    def test_t1_r15_hook_env_warm_hit_prints_port_fast_without_restarting_proxy(self) -> None:
        """R15: hook-env --ensure-running --print-port returns port quickly on warm hit without restarting proxy."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        _install_dummy_plugin_json(env)
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        orig_pid = proxy.pid

        res = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), str(port))
        self.assertLess(res.duration_s, 1.5, f"Warm hit too slow: {res.duration_s}s")
        self.assertIsNone(proxy.returncode)
        self.assertEqual(proxy.pid, orig_pid)


class TestTier1RolloutR16IntegrationCli(E2ETestCase):
    TIER = 1
    FEATURE = "R16"

    def test_t1_r16_integration_status_enable_disable_json_lifecycle(self) -> None:
        """R16: integration status/enable/disable --json reports expected structured fields across lifecycle."""
        if os.name == "nt":
            self.skipTest("POSIX integration CLI test")
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        st0 = run_cli([str(binary), "integration", "status", "--json"], env=proc_env)
        self.assertEqual(st0.returncode, 0, st0.stderr)
        d0 = st0.json_stdout()
        self.assertIs(d0["supported"], True)
        self.assertIs(d0["enabled"], False)
        self.assertEqual(d0["shell"], "zsh")

        en = run_cli([str(binary), "integration", "enable", "--json"], env=proc_env)
        self.assertEqual(en.returncode, 0, en.stderr)
        d1 = en.json_stdout()
        self.assertIs(d1["enabled"], True)
        self.assertIs(d1["manual_required"], False)
        self.assertTrue(Path(d1["rc_path"]).is_file())
        self.assertTrue(Path(d1["env_script_path"]).is_file())

        dis = run_cli([str(binary), "integration", "disable", "--json"], env=proc_env)
        self.assertEqual(dis.returncode, 0, dis.stderr)
        d2 = dis.json_stdout()
        self.assertIs(d2["enabled"], False)

    def test_t1_r16_binary_path_precedence_flag_over_stable_cache_over_current_exe(self) -> None:
        """R16: integration enable honors --binary > data_dir/bin/agy-companion > current_exe in env.sh."""
        if os.name == "nt":
            self.skipTest("POSIX integration CLI test")
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        # 1. Default without stable binary uses current_exe()
        r_cur = run_cli([str(binary), "integration", "enable", "--json"], env=proc_env)
        self.assertEqual(r_cur.returncode, 0, r_cur.stderr)
        env_sh = env.data_dir / "env.sh"
        self.assertIn(str(binary.resolve()), env_sh.read_text(encoding="utf-8"))

        # 2. Executable stable <data_dir>/bin/agy-companion wins over current_exe()
        stable_bin = env.data_dir / "bin" / "agy-companion"
        stable_bin.parent.mkdir(parents=True, exist_ok=True)
        stable_bin.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        os.chmod(stable_bin, 0o755)
        r_stable = run_cli([str(binary), "integration", "enable", "--json"], env=proc_env)
        self.assertEqual(r_stable.returncode, 0, r_stable.stderr)
        self.assertIn(str(stable_bin), env_sh.read_text(encoding="utf-8"))

        # 3. Explicit --binary wins over stable cache
        explicit_bin = env.root / "custom-bin" / "agy-companion"
        explicit_bin.parent.mkdir(parents=True, exist_ok=True)
        explicit_bin.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        os.chmod(explicit_bin, 0o755)
        r_exp = run_cli(
            [str(binary), "integration", "enable", "--binary", str(explicit_bin), "--json"],
            env=proc_env,
        )
        self.assertEqual(r_exp.returncode, 0, r_exp.stderr)
        self.assertIn(str(explicit_bin), env_sh.read_text(encoding="utf-8"))


class TestTier1RolloutR17LaunchEnvRestore(E2ETestCase):
    TIER = 1
    FEATURE = "R17"

    def test_t1_r17_launch_print_env_emits_jetski_mcp_env_restore_vars(self) -> None:
        """R17: launch --print-env emits CLOUD_CODE_URL, GOOGLE_GEMINI_BASE_URL, and JETSKI_MCP_ENV_RESTORE_*."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        res = run_cli(
            [
                str(binary),
                "launch",
                "--port",
                str(port),
                "--config",
                str(cfg),
                "--app",
                "/opt/antigravity",
                "--print-env",
            ],
            env=env.build_env(),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn(f'CLOUD_CODE_URL="http://127.0.0.1:{port}"', res.stdout)
        self.assertIn(f'GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:{port}"', res.stdout)
        self.assertIn("JETSKI_MCP_ENV_RESTORE_PREFIX=", res.stdout)
        self.assertIn("JETSKI_MCP_ENV_RESTORE_VARS=", res.stdout)
        self.assertIn("/opt/antigravity", res.stdout)
        self.assertNotIn("GOOGLE_API_KEY=", res.stdout)

    def test_t1_r17_launch_with_fake_app_spawns_proxy_and_injects_restore_env(self) -> None:
        """R17: launch with a fake app script spawns daemon and passes restore env vars to child app."""
        if os.name == "nt":
            self.skipTest("POSIX fake app script test")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        dump_path = env.root / "app_env.json"
        fake_app = env.root / "fake_antigravity.sh"
        _write_env_dumper_script(fake_app, dump_path)

        res = run_cli(
            [
                str(binary),
                "launch",
                "--port",
                str(port),
                "--config",
                str(cfg),
                "--app",
                str(fake_app),
            ],
            env=env.build_env(),
        )
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        app_env = _wait_for_json_file(dump_path, timeout=5.0)
        self.assertEqual(app_env.get("CLOUD_CODE_URL"), f"http://127.0.0.1:{port}")
        self.assertEqual(app_env.get("GOOGLE_GEMINI_BASE_URL"), f"http://127.0.0.1:{port}")
        self.assertEqual(app_env.get("GEMINI_API_KEY"), _PLACEHOLDER_KEY)
        self.assertTrue(app_env.get("JETSKI_MCP_ENV_RESTORE_PREFIX"))
        self.assertIn("CLOUD_CODE_URL", app_env.get("JETSKI_MCP_ENV_RESTORE_VARS", ""))
        self.assertIsNotNone(_wait_for_health(port, timeout=3.0))


class TestTier1RolloutR18McpLauncherCloudCode(E2ETestCase):
    TIER = 1
    FEATURE = "R18"

    def test_t1_r18_tools_list_exposes_5_tools_and_snapshot_has_launcher_and_cloudcode(self) -> None:
        """R18: MCP exposes 5 tools including app-only set_launcher_integration and enriched SettingsSnapshot."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(api_key="AIzaSyRolloutR18Mcp1234"))
        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
        )
        tools = {t["name"]: t for t in (mcp.list_tools().get("tools") or [])}
        mcp.assert_clean_stdout()
        self.assertEqual(
            set(tools.keys()),
            {
                "open_settings",
                "get_settings",
                "save_settings",
                "test_connection",
                "set_launcher_integration",
            },
        )
        meta_ui = (tools["set_launcher_integration"].get("_meta") or {}).get("ui") or {}
        self.assertEqual(meta_ui.get("visibility"), ["app"])

        for tool_name in ("open_settings", "get_settings"):
            res = mcp.call_tool(tool_name, {})
            mcp.assert_clean_stdout()
            sc = res.get("structuredContent", {})
            self.assertIn("launcher_integration", sc, sc)
            self.assertIn("cloudcode", sc, sc)
            li = sc["launcher_integration"]
            self.assertIn("supported", li)
            self.assertIn("enabled", li)
            self.assertIn("manual_required", li)
            cc = sc["cloudcode"]
            self.assertIn("mode", cc)
            self.assertIn("configured_upstream", cc)
            self.assertIn("resolved_upstream", cc)

    def test_t1_r18_set_launcher_integration_toggles_rc_hook_and_returns_snapshot(self) -> None:
        """R18: set_launcher_integration enables and disables RC hook in isolated HOME and updates snapshot."""
        if os.name == "nt":
            self.skipTest("POSIX MCP launcher integration test")
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(api_key="AIzaSyRolloutR18Tog1234"))
        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
        )
        r_en = mcp.call_tool("set_launcher_integration", {"enabled": True})
        mcp.assert_clean_stdout()
        sc_en = r_en.get("structuredContent", {})
        self.assertIs(sc_en["launcher_integration"]["enabled"], True)
        self.assertTrue((env.home_dir / ".zlogin").is_file())
        self.assertTrue((env.data_dir / "env.sh").is_file())

        r_dis = mcp.call_tool("set_launcher_integration", {"enabled": False})
        mcp.assert_clean_stdout()
        sc_dis = r_dis.get("structuredContent", {})
        self.assertIs(sc_dis["launcher_integration"]["enabled"], False)
        self.assertFalse((env.home_dir / ".zlogin").exists())


class TestTier1RolloutR19UiRolloutBundle(E2ETestCase):
    TIER = 1
    FEATURE = "R19"

    def test_t1_r19_ui_bundle_size_matches_mcp_resource_and_preserves_invariants(self) -> None:
        """R19: ui/dist/index.html stays under 500 KiB, matches MCP resource, and preserves core UI invariants."""
        bundle_path = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(bundle_path.is_file(), f"Missing {bundle_path}")
        raw = bundle_path.read_bytes()
        self.assertGreater(len(raw), 0)
        self.assertLess(len(raw), 512_000, f"Bundle exceeded 500 KiB: {len(raw)} bytes")

        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        mcp = self.start_mcp(env, config_path=cfg)
        res = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        contents = res.get("contents") or []
        self.assertGreaterEqual(len(contents), 1)
        mcp_html = contents[0]["text"]
        self.assertEqual(mcp_html, raw.decode("utf-8"))

        main_ts = (self.repo_root / "ui" / "src" / "main.ts").read_text(encoding="utf-8")
        self.assertIn("Antigravity Companion", raw.decode("utf-8"))
        self.assertNotIn("AGY Companion", raw.decode("utf-8"))
        for marker in ("snapshotEpoch", "refreshBtn", "endpointTag", "portChangedSinceSave"):
            self.assertIn(marker, main_ts, f"Missing preserved UI invariant: {marker}")

    def test_t1_r19_ui_includes_launcher_cloudcode_countdown_and_details_contracts(self) -> None:
        """R19: ui/src/main.ts and bundle include Dock integration, CloudCode, countdown, and collapsed details."""
        main_ts = (self.repo_root / "ui" / "src" / "main.ts").read_text(encoding="utf-8")
        bundle_text = (self.repo_root / "ui" / "dist" / "index.html").read_text(encoding="utf-8")
        for token in (
            "set_launcher_integration",
            "launcher_integration",
            "cloudcode",
            "last_catalog_refresh_at_ms",
            "test_connection",
        ):
            self.assertIn(token, main_ts, f"Missing {token} in ui/src/main.ts")
            self.assertIn(token, bundle_text, f"Missing {token} in ui/dist/index.html")
        self.assertIn("details", main_ts.lower())


class TestTier1RolloutR20RunCompanionHardening(E2ETestCase):
    TIER = 1
    FEATURE = "R20"

    def test_t1_r20_run_companion_syntax_and_scrubs_injected_proxy_env_before_exec(self) -> None:
        """R20: run-companion.sh passes bash -n & /bin/bash -n and scrubs injected proxy vars before exec."""
        if os.name == "nt":
            self.skipTest("POSIX launcher script test")
        script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        self.assertTrue(stat.S_IMODE(os.stat(script).st_mode) & 0o111)
        self.assertEqual(run_cli(["bash", "-n", str(script)]).returncode, 0)
        if Path("/bin/bash").is_file():
            self.assertEqual(run_cli(["/bin/bash", "-n", str(script)]).returncode, 0)

        env = self.make_env()
        iso_plugin = env.root / "iso_plugin"
        (iso_plugin / "scripts").mkdir(parents=True, exist_ok=True)
        iso_script = iso_plugin / "scripts" / "run-companion.sh"
        shutil.copy2(script, iso_script)
        os.chmod(iso_script, 0o755)
        shutil.copy2(self.repo_root / "plugin" / "plugin.json", iso_plugin / "plugin.json")

        dump_path = env.root / "child_env.json"
        fake_bin = env.root / "fake_agy_companion.sh"
        _write_env_dumper_script(fake_bin, dump_path)

        proc_env = env.build_env(
            extra={
                "AGY_COMPANION_BIN": str(fake_bin),
                "CLOUD_CODE_URL": "http://127.0.0.1:4141",
                "GOOGLE_GEMINI_BASE_URL": "http://127.0.0.1:4141",
                "GEMINI_API_KEY": _PLACEHOLDER_KEY,
                "GOOGLE_API_KEY": "AIzaSyKeepGoogleKey1234",
                "AGY_COMPANION_HOOK_ACTIVE": "1",
                "JETSKI_MCP_ENV_RESTORE_PREFIX": "AGY_COMPANION_ORIG_",
                "JETSKI_MCP_ENV_RESTORE_VARS": "CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL",
            }
        )
        res = run_cli([str(iso_script), "serve"], env=proc_env)
        self.assertEqual(res.returncode, 0, res.stderr)
        child_env = json.loads(dump_path.read_text(encoding="utf-8"))
        for scrubbed in (
            "CLOUD_CODE_URL",
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
            "AGY_COMPANION_HOOK_ACTIVE",
            "JETSKI_MCP_ENV_RESTORE_PREFIX",
            "JETSKI_MCP_ENV_RESTORE_VARS",
        ):
            self.assertNotIn(scrubbed, child_env, f"{scrubbed} was not scrubbed before exec")
        self.assertEqual(child_env.get("GOOGLE_API_KEY"), "AIzaSyKeepGoogleKey1234")

    def test_t1_r20_run_companion_prefers_xdg_version_matched_cache_over_path(self) -> None:
        """R20: run-companion.sh honors XDG_DATA_HOME and prefers version-matched cache over unversioned PATH."""
        if os.name == "nt":
            self.skipTest("POSIX launcher script test")
        env = self.make_env()
        iso_plugin = env.root / "iso_plugin"
        (iso_plugin / "scripts").mkdir(parents=True, exist_ok=True)
        iso_script = iso_plugin / "scripts" / "run-companion.sh"
        shutil.copy2(self.repo_root / "plugin" / "scripts" / "run-companion.sh", iso_script)
        os.chmod(iso_script, 0o755)
        pj = json.loads((self.repo_root / "plugin" / "plugin.json").read_text(encoding="utf-8"))
        version = str(pj["version"])
        (iso_plugin / "plugin.json").write_text(json.dumps(pj), encoding="utf-8")

        # 1. Version-matched cached binary under XDG_DATA_HOME
        cache_bin_dir = env.data_root / "agy-companion" / "bin"
        cache_bin_dir.mkdir(parents=True, exist_ok=True)
        cache_bin = cache_bin_dir / "agy-companion"
        cache_bin.write_text("#!/bin/sh\necho CACHE_BIN_SELECTED\n", encoding="utf-8")
        os.chmod(cache_bin, 0o755)
        (cache_bin_dir / ".version").write_text(f"{version}\n", encoding="utf-8")

        # 2. Stale binary on PATH
        path_bin_dir = env.root / "stale_path_bin"
        path_bin_dir.mkdir(parents=True, exist_ok=True)
        stale_bin = path_bin_dir / "agy-companion"
        stale_bin.write_text("#!/bin/sh\necho STALE_PATH_SELECTED\n", encoding="utf-8")
        os.chmod(stale_bin, 0o755)

        safe_bin = _build_safe_bin_dir(env)
        proc_env = env.build_env(
            extra={
                "PATH": f"{path_bin_dir}:{safe_bin}",
                "XDG_DATA_HOME": str(env.data_root),
                "ANTIGRAVITY_EXECUTABLE_DATA_DIR": None,
                "AGY_COMPANION_BIN": None,
            }
        )
        res = run_cli([str(iso_script), "serve"], env=proc_env)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), "CACHE_BIN_SELECTED")


class TestTier1RolloutR21EmbeddedPluginParity(E2ETestCase):
    TIER = 1
    FEATURE = "R21"

    def test_t1_r21_install_writes_all_7_plugin_files_with_unix_byte_parity(self) -> None:
        """R21: install embeds all 7 plugin files matching repo plugin/ byte-for-byte on Unix."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        res = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                str(plugins_dir),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ],
            env=env.build_env(),
        )
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        installed = plugins_dir / "agy-companion"
        expected_rel = (
            "plugin.json",
            "README.md",
            "assets/logo.svg",
            "mcp_config.json",
            "sidecars/proxy/sidecar.json",
            "skills/companion/SKILL.md",
            "scripts/run-companion.sh",
        )
        for rel in expected_rel:
            dst = installed / rel
            self.assertTrue(dst.is_file(), f"Missing embedded plugin file: {rel}")
            if os.name != "nt":
                src = self.repo_root / "plugin" / rel
                self.assertEqual(
                    dst.read_bytes(),
                    src.read_bytes(),
                    f"Byte drift between installed and repo plugin/{rel}",
                )
        if os.name != "nt":
            self.assertTrue(stat.S_IMODE(os.stat(installed / "scripts" / "run-companion.sh").st_mode) & 0o111)

    def test_t1_r21_installed_plugin_manifests_valid_and_pass_strict_agy_validate(self) -> None:
        """R21: Installed plugin manifests parse cleanly and pass agy plugin validate --strict when agy is present."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        res = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                str(plugins_dir),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ],
            env=env.build_env(),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        installed = plugins_dir / "agy-companion"
        pj = json.loads((installed / "plugin.json").read_text(encoding="utf-8"))
        self.assertEqual(pj["name"], "agy-companion")
        self.assertIsInstance(json.loads((installed / "mcp_config.json").read_text(encoding="utf-8")), dict)
        self.assertIsInstance(
            json.loads((installed / "sidecars" / "proxy" / "sidecar.json").read_text(encoding="utf-8")),
            dict,
        )
        if shutil.which("agy"):
            val = run_cli(["agy", "plugin", "validate", "--strict", "--json", str(installed)])
            self.assertEqual(val.returncode, 0, f"{val.stdout}\n{val.stderr}")
            self.assertEqual(val.json_stdout().get("findings"), [])


class TestTier1RolloutR22InstallCli(E2ETestCase):
    TIER = 1
    FEATURE = "R22"

    def test_t1_r22_fresh_install_stages_binary_plugin_hook_and_running_proxy(self) -> None:
        """R22: Fresh install --json installs binary, 7-file plugin, env.sh, RC hook, and starts healthy proxy."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        res = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"],
            env=proc_env,
        )
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        payload = res.json_stdout()
        self.assertIs(payload.get("ok"), True, payload)

        stable_bin = env.data_dir / "bin" / ("agy-companion.exe" if os.name == "nt" else "agy-companion")
        self.assertTrue(stable_bin.is_file() and os.access(stable_bin, os.X_OK))
        self.assertTrue((env.data_dir / "bin" / ".version").is_file())
        self.assertTrue((plugins_dir / "agy-companion" / "plugin.json").is_file())

        if os.name != "nt":
            self.assertTrue((env.data_dir / "env.sh").is_file())
            self.assertEqual(stat.S_IMODE(os.lstat(env.data_dir / "env.sh").st_mode), 0o600)
            self.assertTrue((env.home_dir / ".zlogin").is_file())

        # Zero leftover staging or backup directories
        leftovers = [p.name for p in plugins_dir.iterdir() if ".stage." in p.name or ".backup" in p.name]
        self.assertEqual(leftovers, [], f"Leftover staging dirs: {leftovers}")
        health = _wait_for_health(port, timeout=4.0)
        self.assertIsNotNone(health, "install did not start healthy proxy")

    def test_t1_r22_install_flags_and_idempotent_upgrade_leave_no_temp_dirs(self) -> None:
        """R22: --no-launcher-hook --no-start-proxy skips hook/daemon and re-running install upgrades cleanly."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        for attempt in (1, 2):
            res = run_cli(
                [
                    str(binary),
                    "install",
                    "--plugins-dir",
                    str(plugins_dir),
                    "--no-launcher-hook",
                    "--no-start-proxy",
                    "--json",
                ],
                env=proc_env,
            )
            self.assertEqual(res.returncode, 0, f"Attempt {attempt}: {res.stdout}\n{res.stderr}")
            self.assertIs(res.json_stdout().get("ok"), True)

        self.assertFalse((env.home_dir / ".zlogin").exists(), "--no-launcher-hook should not create .zlogin")
        self.assertFalse((env.data_dir / "proxy.pid").exists(), "--no-start-proxy should not start proxy")
        leftovers = [p.name for p in plugins_dir.iterdir() if ".stage." in p.name or ".backup" in p.name]
        self.assertEqual(leftovers, [])


class TestTier1RolloutR23UninstallCli(E2ETestCase):
    TIER = 1
    FEATURE = "R23"

    def test_t1_r23_uninstall_removes_artifacts_and_preserves_config_and_usage(self) -> None:
        """R23: Default uninstall strips RC, deletes env.sh/plugin/binary, stops proxy, and keeps config & usage."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key="AIzaSyRolloutR23Keep123"))
        usage_canary = env.usage_dir / "2026-11.jsonl"
        usage_canary.write_text('{"ts":"2026-11-24T00:00:00Z"}\n', encoding="utf-8")
        gemini_cfg = env.home_dir / ".gemini" / "config" / "config.json"
        gemini_cfg.parent.mkdir(parents=True, exist_ok=True)
        gemini_cfg.write_text('{"keep":"user"}\n', encoding="utf-8")

        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        inst = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"],
            env=proc_env,
        )
        self.assertEqual(inst.returncode, 0, inst.stderr)
        self.assertIsNotNone(_wait_for_health(port, timeout=4.0))

        un = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"],
            env=proc_env,
        )
        self.assertEqual(un.returncode, 0, f"{un.stdout}\n{un.stderr}")
        self.assertFalse((plugins_dir / "agy-companion").exists())
        self.assertFalse((env.data_dir / "env.sh").exists())
        self.assertFalse((env.home_dir / ".zlogin").exists())
        self.assertFalse((env.data_dir / "bin" / "agy-companion").exists())
        self.assertFalse((env.data_dir / "bin" / ".version").exists())
        self.assertFalse((env.data_dir / "proxy.pid").exists(), "proxy.pid must be removed after uninstall")
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            try:
                http_raw_request("127.0.0.1", port, "GET", "/health", timeout=0.3)
            except OSError:
                break
            time.sleep(0.05)
        else:
            self.fail(f"Proxy on port {port} still responded to /health after uninstall")
        # Preserved by default:
        self.assertTrue(cfg.is_file(), "config.json must be preserved without --purge")
        self.assertTrue(usage_canary.is_file(), "usage JSONL must be preserved without --purge")
        self.assertEqual(gemini_cfg.read_text(encoding="utf-8"), '{"keep":"user"}\n')

    def test_t1_r23_uninstall_purge_removes_config_and_usage_keeping_gemini_config(self) -> None:
        """R23: uninstall --purge deletes config.json and usage/ while leaving ~/.gemini/config/config.json intact."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key="AIzaSyRolloutR23Purge12"))
        usage_canary = env.usage_dir / "2026-11.jsonl"
        usage_canary.write_text('{"ts":"2026-11-24T00:00:00Z"}\n', encoding="utf-8")
        gemini_cfg = env.home_dir / ".gemini" / "config" / "config.json"
        gemini_cfg.parent.mkdir(parents=True, exist_ok=True)
        gemini_cfg.write_text('{"keep":"antigravity"}\n', encoding="utf-8")

        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})
        inst = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                str(plugins_dir),
                "--no-start-proxy",
                "--json",
            ],
            env=proc_env,
        )
        self.assertEqual(inst.returncode, 0, inst.stderr)

        un = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--purge", "--json"],
            env=proc_env,
        )
        self.assertEqual(un.returncode, 0, f"{un.stdout}\n{un.stderr}")
        self.assertFalse(cfg.exists(), "--purge must delete config.json")
        self.assertFalse(usage_canary.exists(), "--purge must delete usage files")
        self.assertFalse((plugins_dir / "agy-companion").exists())
        self.assertEqual(gemini_cfg.read_text(encoding="utf-8"), '{"keep":"antigravity"}\n')


class TestTier1RolloutR24WindowsParity(E2ETestCase):
    TIER = 1
    FEATURE = "R24"

    def test_t1_r24_windows_launch_discovery_and_direct_exe_manifests_contract(self) -> None:
        """R24: Windows parity provides Antigravity.exe discovery, direct .exe manifests, and Start Menu .lnk."""
        launch_rs = (self.repo_root / "crates" / "companion-cli" / "src" / "launch.rs").read_text(encoding="utf-8")
        install_rs = (self.repo_root / "crates" / "companion-cli" / "src" / "install.rs").read_text(encoding="utf-8")
        self.assertIn("cfg(windows)", launch_rs)
        self.assertIn("Antigravity.exe", launch_rs)
        self.assertIn("cfg(windows)", install_rs)
        self.assertIn("agy-companion.exe", install_rs)
        self.assertIn("Antigravity (Companion).lnk", install_rs)

        if sys.platform == "win32":
            binary = self.require_binary()
            env = self.make_env()
            port = find_free_port()
            env.write_config(env.make_config_dict(listen_port=port))
            plugins_dir = env.root / "plugins"
            res = run_cli(
                [
                    str(binary),
                    "install",
                    "--plugins-dir",
                    str(plugins_dir),
                    "--no-launcher-hook",
                    "--no-start-proxy",
                    "--json",
                ],
                env=env.build_env(),
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            mcp_cfg = json.loads((plugins_dir / "agy-companion" / "mcp_config.json").read_text(encoding="utf-8"))
            cmd = mcp_cfg["mcpServers"]["hub"]["command"]
            self.assertTrue(cmd.lower().endswith("agy-companion.exe"), cmd)

    def test_t1_r24_windows_zero_setx_and_zero_hkcu_environment_mutations(self) -> None:
        """R24: Windows implementation never executes setx or mutates HKCU Environment registry keys."""
        targets = (
            self.repo_root / "crates" / "companion-cli" / "src" / "install.rs",
            self.repo_root / "crates" / "companion-cli" / "src" / "launch.rs",
            self.repo_root / "crates" / "companion-core" / "src" / "integration.rs",
        )
        for path in targets:
            with self.subTest(file=path.name):
                self.assertTrue(path.is_file(), f"Missing expected source file: {path}")
                code = _strip_rust_comments_and_tests(path.read_text(encoding="utf-8"))
                self.assertIsNone(re.search(r"\bsetx\b", code, re.IGNORECASE), f"setx found in {path}")
                self.assertNotIn("HKCU\\Environment", code, f"HKCU\\Environment found in {path}")
                self.assertNotIn("HKCU\\\\Environment", code, f"HKCU\\\\Environment found in {path}")


class TestTier1RolloutR25RolloutDocs(E2ETestCase):
    TIER = 1
    FEATURE = "R25"

    def test_t1_r25_design_doc_and_readme_hero_install_commands_present(self) -> None:
        """R25: Architecture doc exists and README leads with npx, npm, uvx install and uninstall commands."""
        design_doc = self.repo_root / "docs" / "design" / "dock-launcher-and-install.md"
        self.assertTrue(design_doc.is_file(), f"Missing {design_doc}")
        self.assertGreater(design_doc.stat().st_size, 200)

        docs_readme = (self.repo_root / "docs" / "README.md").read_text(encoding="utf-8")
        self.assertIn("dock-launcher-and-install.md", docs_readme)

        readme = (self.repo_root / "README.md").read_text(encoding="utf-8")
        for expected in (
            "npx -y agy-companion@latest install",
            "npm i -g agy-companion",
            "uvx --refresh agy-companion install",
            "uninstall",
        ):
            self.assertIn(expected, readme, f"README.md missing: {expected}")

    def test_t1_r25_skill_md_within_budget_and_no_gemini_md(self) -> None:
        """R25: plugin SKILL.md stays within 24,000 bytes with valid frontmatter and GEMINI.md is absent."""
        skill_path = self.repo_root / "plugin" / "skills" / "companion" / "SKILL.md"
        self.assertTrue(skill_path.is_file())
        raw = skill_path.read_bytes()
        self.assertLessEqual(len(raw), 24_000, f"SKILL.md too large: {len(raw)} bytes")
        text = raw.decode("utf-8")
        self.assertTrue(text.startswith("---\n") or text.startswith("---\r\n"))
        parts = text.split("---", 2)
        self.assertGreaterEqual(len(parts), 3)
        frontmatter = parts[1]
        m = re.search(r"(?m)^name:\s*([a-z0-9-]+)\s*$", frontmatter)
        self.assertIsNotNone(m)
        assert m is not None
        self.assertEqual(m.group(1), "companion")
        self.assertFalse((self.repo_root / "GEMINI.md").exists())
