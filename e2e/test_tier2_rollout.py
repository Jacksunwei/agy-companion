"""Tier 2 Rollout: Boundary, Corner-Case, Negative & S1–S8 Security Tests (`R01` .. `R25`).

Complements Tier 1 happy-path tests with adversarial inputs, boundary limits,
fail-closed security invariants, and non-mutation guards across all 25 rollout
features (`R01`–`R25`).
"""

from __future__ import annotations

import gzip
import json
import os
import re
import shlex
import shutil
import socket
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
    run_cli,
)
from mcp_client import McpStdioClient
from mock_gemini import CapturedRequest, JsonAction, MockGeminiServer, RawBytesAction, SseStreamAction


_PLACEHOLDER_KEY = "agy-companion-local"
_PLACEHOLDER_SLOT_RE = re.compile(r"^MODEL_PLACEHOLDER_M(\d+)$")
_S5_FORBIDDEN_SINKS_RE = re.compile(
    r"innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML"
    r"|localStorage|sessionStorage|document\.cookie|new\s+Function|XMLHttpRequest"
    r"|(?<![A-Za-z0-9_$])(?:eval|alert|confirm|prompt|fetch)\s*\("
)
_S5_EXTERNAL_TAGS_RE = re.compile(
    r"<script[^>]*\ssrc\s*=|<link[^>]*\shref\s*=|<img[^>]*\ssrc\s*=|<iframe|@import",
    re.IGNORECASE,
)


def _read_rust_module_source(module_base: Path, *, include_tests: bool = False) -> str:
    single_file = module_base.with_suffix(".rs")
    if single_file.is_file():
        files = [single_file]
    elif module_base.is_dir():
        files = [
            candidate
            for candidate in sorted(module_base.rglob("*.rs"))
            if candidate.is_file()
            and (
                include_tests
                or (
                    candidate.name != "tests.rs"
                    and "tests" not in candidate.relative_to(module_base).parts
                )
            )
        ]
    else:
        files = []
    if not files:
        raise AssertionError(f"Missing expected Rust module: {module_base}")
    return "\n".join(path.read_text(encoding="utf-8") for path in files)


def _sample_catalog(used_slots: list[int] | None = None) -> dict[str, Any]:
    models: dict[str, Any] = {
        "gemini-4-carbon-high": {
            "model": "MODEL_PLACEHOLDER_M427",
            "displayName": "Gemini 4 Carbon (High)",
            "thinkingLevel": 3,
        }
    }
    for slot in used_slots or []:
        models[f"upstream-slot-{slot}"] = {
            "model": f"MODEL_PLACEHOLDER_M{slot}",
            "displayName": f"Upstream Slot {slot}",
        }
    return {
        "models": models,
        "defaultAgentModelId": "gemini-4-carbon-high",
        "agentModelSorts": [
            {
                "displayName": "Recommended",
                "groups": [{"displayName": "Google AI Pro", "modelIds": ["gemini-4-carbon-high"]}],
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
    for tool in (
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
    ):
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


def _strip_ts_comments(source: str) -> str:
    pattern = re.compile(
        r'("(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'|`(?:\\.|[^`\\])*`)'
        r"|//[^\n]*|/\*[\s\S]*?\*/"
    )
    return pattern.sub(lambda m: m.group(1) if m.group(1) is not None else " ", source)


class TestTier2RolloutR01CloudCodeDailyFirstBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R01"

    def test_t2_r01_malformed_json_body_still_defaults_daily_first(self) -> None:
        """R01: Malformed JSON body on fetchAvailableModels fails safely and still tries daily before prod."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R01Bad12",
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
            headers={"Authorization": "Bearer t2-r01-bad-json", "Content-Type": "application/json"},
            body=b"{not-valid-json",
            timeout=5.0,
        )
        self.assertEqual(resp.status, 502, resp.text)
        targets = connect_proxy.wait_for_connects(2, timeout=3.0)
        self.assertGreaterEqual(len(targets), 2, targets)
        self.assertEqual(targets[0], "daily-cloudcode-pa.googleapis.com:443")
        self.assertEqual(targets[1], "cloudcode-pa.googleapis.com:443")

    def test_t2_r01_business_looking_project_names_do_not_bypass_daily_first(self) -> None:
        """R01: Non-empty project names without entitlement or auth_method still route daily first."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R01Proj1",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        for project in ("projects/corp-prod-123", "enterprise-looking-managed-42"):
            with self.subTest(project=project):
                connect_proxy.clear_targets()
                resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1internal:fetchAvailableModels",
                    headers={"Authorization": f"Bearer t2-r01-{project}", "Content-Type": "application/json"},
                    body=json.dumps({"project": project}).encode("utf-8"),
                    timeout=5.0,
                )
                self.assertEqual(resp.status, 502, resp.text)
                targets = connect_proxy.wait_for_connects(2, timeout=3.0)
                self.assertGreaterEqual(len(targets), 2, targets)
                self.assertEqual(targets[0], "daily-cloudcode-pa.googleapis.com:443")


class TestTier2RolloutR02EnterpriseGcpTosBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R02"

    def test_t2_r02_empty_or_null_entitlement_user_tier_does_not_force_prod(self) -> None:
        """R02: Empty or null entitlement.userTier does not falsely trigger prod-first Enterprise routing."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R02Null1",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        for label, payload in (
            ("empty_entitlement", b'{"entitlement":{}}'),
            ("null_user_tier", b'{"entitlement":{"userTier":null}}'),
            ("empty_string_user_tier", b'{"entitlement":{"userTier":""}}'),
        ):
            with self.subTest(case=label):
                connect_proxy.clear_targets()
                resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1internal:fetchAvailableModels",
                    headers={"Authorization": f"Bearer t2-r02-{label}", "Content-Type": "application/json"},
                    body=payload,
                    timeout=5.0,
                )
                self.assertEqual(resp.status, 502, resp.text)
                targets = connect_proxy.wait_for_connects(2, timeout=3.0)
                self.assertGreaterEqual(len(targets), 2, targets)
                self.assertEqual(targets[0], "daily-cloudcode-pa.googleapis.com:443")

    def test_t2_r02_unknown_auth_method_and_absent_ua_default_daily_first(self) -> None:
        """R02: Unknown auth_method token or absent User-Agent falls back safely to daily first."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R02Ua123",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        for label, extra_hdrs in (
            ("unknown_auth", {"User-Agent": "antigravity/2.34.0 auth_method=unknown"}),
            ("missing_ua", {}),
        ):
            with self.subTest(case=label):
                connect_proxy.clear_targets()
                hdrs = {"Authorization": f"Bearer t2-r02-ua-{label}", "Content-Type": "application/json"}
                hdrs.update(extra_hdrs)
                resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1internal:fetchAvailableModels",
                    headers=hdrs,
                    body=b"{}",
                    timeout=5.0,
                )
                self.assertEqual(resp.status, 502, resp.text)
                targets = connect_proxy.wait_for_connects(2, timeout=3.0)
                self.assertGreaterEqual(len(targets), 2, targets)
                self.assertEqual(targets[0], "daily-cloudcode-pa.googleapis.com:443")


class TestTier2RolloutR03SipHashLruHotReloadBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R03"

    def test_t2_r03_lru_evicts_oldest_entry_after_8_distinct_accounts(self) -> None:
        """R03: Inserting 9 distinct accounts evicts the oldest entry from the 8-entry per-account LRU."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()

        def handler(req: CapturedRequest) -> Any:
            if ":fetchAvailableModels" in req.path:
                if req.header("x-goog-user-project") is not None:
                    return JsonAction(status=403, payload={"error": {"message": "strip"}})
                return JsonAction(status=200, payload=_sample_catalog())
            return SseStreamAction(chunks=[b'data: {"ok":true}\n\n'], status=200)

        mock_ccpa.set_handler(handler)
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R03Lru12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # Populate 9 distinct tokens -> token-1 must be evicted when token-9 inserts.
        for idx in range(1, 10):
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1internal:fetchAvailableModels",
                headers={
                    "Authorization": f"Bearer r03-lru-account-{idx}",
                    "x-goog-user-project": f"proj-{idx}",
                    "Content-Type": "application/json",
                },
                body=b"{}",
            )
            self.assertEqual(r.status, 200, r.text)

        mock_ccpa.clear_requests()

        # Token 9 (most recent) must still strip x-goog-user-project.
        r9 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r03-lru-account-9",
                "x-goog-user-project": "proj-9",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(r9.status, 200, r9.text)

        # Token 1 (oldest, evicted) must forward x-goog-user-project again.
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r03-lru-account-1",
                "x-goog-user-project": "proj-1",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(r1.status, 200, r1.text)
        self.assertEqual(len(mock_ccpa.requests), 2)
        self.assertIsNone(mock_ccpa.requests[0].header("x-goog-user-project"))
        self.assertEqual(mock_ccpa.requests[1].header("x-goog-user-project"), "proj-1")

    def test_t2_r03_raw_bearer_tokens_never_leak_in_logs_or_stats(self) -> None:
        """R03: Raw OAuth Bearer tokens never appear in proxy stdout, stderr, or /api/stats (S2)."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R03Sec12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        canary_tok = "r03-canary-oauth-secret-987654"
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": f"Bearer {canary_tok}", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200, resp.text)
        stats_text = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").text
        proxy.stop()
        self.assertNotIn(canary_tok, stats_text)
        self.assertNotIn(canary_tok, proxy.stdout_text)
        self.assertNotIn(canary_tok, proxy.stderr_text)


class TestTier2RolloutR04Enterprise403StripBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R04"

    def test_t2_r04_stream_generate_content_403_never_retries_in_fixed_or_auto_mode(self) -> None:
        """R04: Hard idempotency invariant — streamGenerateContent NEVER retries on 403 or transport failure."""
        # 1. Fixed loopback mock returning 403
        env1 = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=403, payload={"error": {"message": "forbidden stream"}}))
        cfg1 = env1.write_config(
            env1.make_config_dict(
                api_key="AIzaSyRolloutT2R04NoR1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy1 = self.start_proxy(env1, config_path=cfg1)
        r_fixed = http_raw_request(
            "127.0.0.1",
            proxy1.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r04-no-retry-fixed",
                "x-goog-user-project": "proj-no-retry",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(r_fixed.status, 403, r_fixed.text)
        self.assertEqual(len(mock_ccpa.requests), 1, "Generation RPC MUST NOT retry on 403!")
        proxy1.stop()

        # 2. Auto mode + MockConnectProxy returning 502 -> exact 1 CONNECT (zero fallback retry)
        env2 = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg2 = env2.write_config(
            env2.make_config_dict(
                api_key="AIzaSyRolloutT2R04NoR2",
                cloudcode_upstream_url="auto",
            )
        )
        proxy2 = self.start_proxy(
            env2,
            config_path=cfg2,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        r_auto = http_raw_request(
            "127.0.0.1",
            proxy2.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer r04-no-retry-auto",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
            timeout=5.0,
        )
        self.assertEqual(r_auto.status, 502, r_auto.text)
        time.sleep(0.15)
        self.assertEqual(
            len(connect_proxy.connect_targets),
            1,
            f"Generation RPC MUST NOT fallback/retry in auto mode: {connect_proxy.connect_targets!r}",
        )

    def test_t2_r04_metadata_403_retries_at_most_once_when_retry_also_returns_403(self) -> None:
        """R04: Metadata 403 retries strictly once even if retry without x-goog-user-project also returns 403."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=403, payload={"error": {"message": "always 403"}}))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R04Once1",
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
                "Authorization": "Bearer r04-double-403",
                "x-goog-user-project": "proj-double",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(resp.status, 403, resp.text)
        self.assertEqual(len(mock_ccpa.requests), 2)
        self.assertEqual(mock_ccpa.requests[0].header("x-goog-user-project"), "proj-double")
        self.assertIsNone(mock_ccpa.requests[1].header("x-goog-user-project"))


class TestTier2RolloutR05TransportHeaderFidelityBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R05"

    def test_t2_r05_unparseable_json_raw_fallback_preserves_content_encoding(self) -> None:
        """R05: Raw fallback on unparseable/compressed fetchAvailableModels body preserves Content-Encoding."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        gz_non_json = gzip.compress(b"not-json-compressed-upstream-bytes")
        mock_ccpa.enqueue(
            RawBytesAction(
                status=200,
                content_type="application/json",
                body=gz_non_json,
                headers={"Content-Encoding": "gzip"},
            )
        )
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R05Raw12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r05-raw-gz", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200)
        self.assertEqual((resp.header("content-encoding") or "").lower(), "gzip")
        self.assertEqual(resp.body, gz_non_json)

    def test_t2_r05_rejects_path_traversal_and_disallowed_methods_on_v1internal(self) -> None:
        """R05: S3/S7 rejects traversal paths and non-GET/POST methods on /v1internal routes with 0 upstream hits."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R05Meth1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        self.assertEqual(
            http_raw_request("127.0.0.1", proxy.port, "PUT", "/v1internal:fetchAvailableModels").status,
            405,
        )
        self.assertEqual(
            http_raw_request("127.0.0.1", proxy.port, "DELETE", "/v1internal:loadCodeAssist").status,
            405,
        )
        self.assertEqual(
            http_raw_request("127.0.0.1", proxy.port, "POST", "/v1internal/a/../secret", body=b"{}").status,
            400,
        )
        self.assertEqual(len(mock_ccpa.requests), 0)


class TestTier2RolloutR06DeterministicSlotsNeonXHighBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R06"

    def test_t2_r06_exhausted_placeholder_pool_returns_200_without_collision(self) -> None:
        """R06: When upstream occupies all M334..=M365 slots, proxy returns 200 without colliding or failing."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_catalog(list(range(334, 366)))))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R06Full1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r06-full-pool", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200, resp.text)
        models = resp.json()["models"]
        for slot in range(334, 366):
            self.assertEqual(models[f"upstream-slot-{slot}"]["model"], f"MODEL_PLACEHOLDER_M{slot}")
        injected = [mid for mid in models if mid.startswith("companion/")]
        self.assertEqual(injected, [], f"Should not inject when pool exhausted: {injected}")

    def test_t2_r06_partial_slot_collisions_probe_cleanly_without_duplicates(self) -> None:
        """R06: Partial upstream collisions in M334..=M365 probe to unique free slots without overwriting upstream."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        blocked = [334, 335, 336, 337, 340, 350, 360]
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_catalog(blocked)))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R06Part1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r06-partial", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200, resp.text)
        models = resp.json()["models"]
        blocked_enums = {f"MODEL_PLACEHOLDER_M{s}" for s in blocked}
        injected_enums = [d["model"] for mid, d in models.items() if mid.startswith("companion/")]
        self.assertTrue(injected_enums)
        self.assertEqual(len(injected_enums), len(set(injected_enums)))
        self.assertTrue(blocked_enums.isdisjoint(set(injected_enums)))
        for enum_val in injected_enums:
            m = _PLACEHOLDER_SLOT_RE.match(enum_val)
            self.assertIsNotNone(m)
            assert m is not None
            self.assertGreaterEqual(int(m.group(1)), 334)
            self.assertLessEqual(int(m.group(1)), 365)


class TestTier2RolloutR07PlaceholderKeyQuarantineBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R07"

    def test_t2_r07_mcp_save_settings_rejects_placeholder_key_preserving_existing_config(self) -> None:
        """R07: save_settings rejects agy-companion-local (trimmed or padded) and keeps real config key intact."""
        env = self.make_env()
        real_key = "AIzaSyRolloutT2R07Keep1"
        cfg = env.write_config(env.make_config_dict(listen_port=find_free_port(), api_key=real_key))
        mcp = self.start_mcp(env, config_path=cfg)

        for bad_key in (_PLACEHOLDER_KEY, f"  {_PLACEHOLDER_KEY}  "):
            with self.subTest(bad_key=bad_key):
                resp = mcp.save_settings_smart(api_key=bad_key, raw=True)
                mcp.assert_clean_stdout()
                self.assertTrue(
                    McpStdioClient.is_tool_error(resp),
                    f"Expected save_settings to reject placeholder key {bad_key!r}: {resp!r}",
                )

        on_disk = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertEqual(on_disk["providers"]["gemini"]["api_key"], real_key)

    def test_t2_r07_padded_placeholder_in_config_and_header_never_reaches_upstream(self) -> None:
        """R07: Whitespace-padded placeholder key in config and header returns 401 with 0 upstream requests."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key=f"  {_PLACEHOLDER_KEY}  ",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={
                "Content-Type": "application/json",
                "x-goog-api-key": f" {_PLACEHOLDER_KEY} ",
            },
            body=b'{"contents":[{"parts":[{"text":"ping"}]}]}',
        )
        self.assertEqual(resp.status, 401, resp.text)
        self.assertEqual(len(mock_gemini.requests), 0)


class TestTier2RolloutR08Day0UntouchedCatalogBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R08"

    def test_t2_r08_placeholder_key_in_config_still_acts_as_day0_untouched(self) -> None:
        """R08: Config containing placeholder key hides Proxy models just like unconfigured Day-0."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        raw = json.dumps(_sample_catalog(), separators=(",", ":")).encode("utf-8")
        mock_ccpa.enqueue(RawBytesAction(status=200, content_type="application/json", body=raw))
        cfg = env.write_config(
            env.make_config_dict(
                api_key=_PLACEHOLDER_KEY,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r08-ph-day0", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 200, resp.text)
        self.assertEqual(resp.body, raw)
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(stats["cloudcode"]["last_catalog_included_proxy_models"], False)

    def test_t2_r08_clearing_api_key_via_hot_reload_reverts_to_day0_untouched(self) -> None:
        """R08: Clearing a configured real API key via hot-reload reverts fetchAvailableModels to untouched."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=200, payload=_sample_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R08Rev12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r08-revert", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r1.status, 200)
        self.assertIn("companion/gemini-4-hydrogen-low", r1.json()["models"])

        env.write_config(
            env.make_config_dict(api_key=None, cloudcode_upstream_url=mock_ccpa.base_url),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r08-revert", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r2.status, 200)
        self.assertNotIn("companion/gemini-4-hydrogen-low", r2.json()["models"])
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(stats["cloudcode"]["last_catalog_included_proxy_models"], False)


class TestTier2RolloutR09ExtendedApiStatsBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R09"

    def test_t2_r09_stats_rejects_post_and_never_leaks_secrets(self) -> None:
        """R09: POST /api/stats returns 405 and GET /api/stats never contains API keys or Bearer tokens."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_catalog()))
        secret_key = "AIzaSyRolloutT2R09Sec12"
        cfg = env.write_config(
            env.make_config_dict(
                api_key=secret_key,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "POST", "/api/stats", body=b"{}").status, 405)

        bearer = "r09-secret-bearer-token-555"
        http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": f"Bearer {bearer}", "Content-Type": "application/json"},
            body=b"{}",
        )
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats")
        self.assertEqual(resp.status, 200)
        self.assertEqual(resp.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", resp.header("cache-control") or "")
        self.assertNotIn(secret_key, resp.text)
        self.assertNotIn(bearer, resp.text)

    def test_t2_r09_stats_refresh_timestamp_advances_monotonically(self) -> None:
        """R09: Consecutive fetchAvailableModels calls update last_catalog_refresh_at_ms monotonically."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=200, payload=_sample_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT2R09Mono1",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r09-mono", "Content-Type": "application/json"},
            body=b"{}",
        )
        ts1 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()["cloudcode"]["last_catalog_refresh_at_ms"]
        self.assertIsInstance(ts1, int)
        time.sleep(0.05)
        http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer r09-mono", "Content-Type": "application/json"},
            body=b"{}",
        )
        ts2 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()["cloudcode"]["last_catalog_refresh_at_ms"]
        self.assertIsInstance(ts2, int)
        self.assertGreaterEqual(ts2, ts1)


class TestTier2RolloutR10NonGemini34HintBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R10"

    def test_t2_r10_legacy_model_rejected_even_without_api_key_or_upstream(self) -> None:
        """R10: Unsupported gemini-1.5-pro fails fast with 400 and unset hint before touching upstream."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock_gemini.base_url, api_key=None))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-1.5-pro:generateContent",
            headers={"Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 400, resp.text)
        self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", resp.json()["error"]["message"])
        self.assertEqual(len(mock_gemini.requests), 0)

    def test_t2_r10_unsupported_lite_model_400_carries_security_headers(self) -> None:
        """R10: Unsupported gemini-2.5-flash-lite 400 carries nosniff, no-store, and unset hint."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(base_url=mock_gemini.base_url, api_key="AIzaSyRolloutT2R10Sec12")
        )
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-flash-lite:generateContent",
            headers={"Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(resp.status, 400, resp.text)
        self.assertEqual(resp.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", resp.header("cache-control") or "")
        self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", resp.json()["error"]["message"])


class TestTier2RolloutR11HealthProbeAdoptRunningBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R11"

    def test_t2_r11_oversized_health_body_over_64kib_rejected_by_adopt_and_hook_env(self) -> None:
        """R11: Hostile >64 KiB /health response is refused by serve --adopt-running and fails open in hook-env."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        hostile = self.start_mock_gemini()
        big_body = (
            b'{"status":"ok","version":"0.1.0","protocol_version":1,"pad":"'
            + (b"A" * (100 * 1024))
            + b'"}'
        )
        hostile.set_handler(
            lambda _req: RawBytesAction(status=200, content_type="application/json", body=big_body)
        )
        cfg = env.write_config(env.make_config_dict(listen_port=hostile.port))
        _install_dummy_plugin_json(env)

        adopt_res = run_cli(
            [str(binary), "serve", "--port", str(hostile.port), "--config", str(cfg), "--adopt-running"],
            env=env.build_env(),
            timeout=6.0,
        )
        self.assertNotEqual(adopt_res.returncode, 0, "adopt-running must reject >64 KiB /health")

        hook_res = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
            timeout=6.0,
        )
        self.assertEqual(hook_res.returncode, 0, hook_res.stderr)
        self.assertEqual(hook_res.stdout, "", f"hook-env must fail open on >64 KiB /health: {hook_res.stdout!r}")

    def test_t2_r11_non_companion_500_health_rejected_by_adopt_running(self) -> None:
        """R11: serve --adopt-running exits non-zero when target port is held by a non-companion server."""
        binary = self.require_binary()
        env = self.make_env()
        hostile = self.start_mock_gemini()
        hostile.set_handler(lambda _req: JsonAction(status=500, payload={"error": "not companion"}))
        cfg = env.write_config(env.make_config_dict(listen_port=hostile.port))

        res = run_cli(
            [str(binary), "serve", "--port", str(hostile.port), "--config", str(cfg), "--adopt-running"],
            env=env.build_env(),
            timeout=6.0,
        )
        self.assertNotEqual(res.returncode, 0)


class TestTier2RolloutR12ManagedDaemonIdlePidLogBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R12"

    def test_t2_r12_invalid_idle_secs_rejected_and_symlink_pidfile_not_followed(self) -> None:
        """R12: Non-integer --idle-shutdown-secs exits non-zero and symlink proxy.pid never overwrites target."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        bad = run_cli(
            [str(binary), "serve", "--port", str(port), "--config", str(cfg), "--idle-shutdown-secs", "abc"],
            env=env.build_env(),
        )
        self.assertNotEqual(bad.returncode, 0)

        if os.name != "nt":
            target = env.root / "do_not_overwrite.txt"
            target.write_text("CANARY_INTACT\n", encoding="utf-8")
            pid_link = env.data_dir / "proxy.pid"
            os.symlink(target, pid_link)
            proc = subprocess.Popen(
                [str(binary), "serve", "--port", str(port), "--config", str(cfg), "--idle-shutdown-secs", "2"],
                env=env.build_env(),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            try:
                time.sleep(0.6)
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=3.0)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=2.0)
            self.assertEqual(target.read_text(encoding="utf-8"), "CANARY_INTACT\n")

    def test_t2_r12_proxy_log_is_0600_regular_file_without_api_key_leaks(self) -> None:
        """R12: Daemon proxy.log (when written) is regular non-symlink mode 0600 with zero plaintext API keys."""
        if os.name == "nt":
            self.skipTest("POSIX mode 0600 test")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        secret_key = "AIzaSyRolloutT2R12Log12"
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key=secret_key))
        _install_dummy_plugin_json(env)

        res = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), str(port))

        candidates = [env.data_dir / "proxy.log", env.data_dir / "logs" / "proxy.log"]
        found_logs = [p for p in candidates if p.exists() or p.is_symlink()]
        self.assertTrue(found_logs, f"Expected proxy.log in {candidates}")
        for log_path in found_logs:
            st = os.lstat(log_path)
            self.assertFalse(stat.S_ISLNK(st.st_mode), f"proxy.log must not be a symlink: {log_path}")
            self.assertTrue(stat.S_ISREG(st.st_mode))
            self.assertEqual(stat.S_IMODE(st.st_mode), 0o600)
            self.assertNotIn(secret_key, log_path.read_text(encoding="utf-8", errors="replace"))


class TestTier2RolloutR13IntegrationEnvShBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R13"

    def test_t2_r13_ssh_guard_override_secret_preservation_and_3state_scrub(self) -> None:
        """R13: SSH guard skips unless overridden, real API keys stay untouched, and scrub preserves empty vs unset."""
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

        # 1. Remote SSH guard blocks activation when SSH_CONNECTION is set.
        ssh_blocked = run_cli(
            ["/bin/sh", "-c", '. "$1"; printf "%s|%s" "${CLOUD_CODE_URL:-}" "${AGY_COMPANION_HOOK_ACTIVE:-}"', "_", str(env_sh)],
            env=env.build_env(
                extra={
                    "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                    "SSH_CONNECTION": "10.0.0.1 5555 10.0.0.2 22",
                    "AGY_COMPANION_CONFIG": str(cfg),
                }
            ),
        )
        self.assertEqual(ssh_blocked.returncode, 0, ssh_blocked.stderr)
        self.assertEqual(ssh_blocked.stdout, "|")

        # 2. AGY_COMPANION_ALLOW_REMOTE_SSH=1 overrides SSH guard, preserves real GEMINI_API_KEY & GOOGLE_API_KEY,
        # and 3-state scrub restores empty CLOUD_CODE_URL="" vs unset GOOGLE_GEMINI_BASE_URL.
        real_gem = "AIzaSyRolloutT2R13Gem12"
        real_goog = "AIzaSyRolloutT2R13Goog1"
        script = (
            'CLOUD_CODE_URL=""\n'
            'export CLOUD_CODE_URL\n'
            'unset GOOGLE_GEMINI_BASE_URL\n'
            '. "$1"\n'
            'printf "ACT:%s|%s|%s|%s\\n" '
            '"${CLOUD_CODE_URL:-}" "${GEMINI_API_KEY:-}" "${GOOGLE_API_KEY:-}" "${JETSKI_MCP_ENV_RESTORE_VARS:-}"\n'
            'unset ANTIGRAVITY_RESOLVING_ENVIRONMENT\n'
            '. "$1"\n'
            'printf "SCRUB:%s|%s|%s|%s\\n" '
            '"${CLOUD_CODE_URL-<UNSET>}" "${GOOGLE_GEMINI_BASE_URL-<UNSET>}" '
            '"${GEMINI_API_KEY-<UNSET>}" "${GOOGLE_API_KEY-<UNSET>}"\n'
        )
        res = run_cli(
            ["/bin/sh", "-c", script, "_", str(env_sh)],
            env=env.build_env(
                extra={
                    "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                    "SSH_CONNECTION": "10.0.0.1 5555 10.0.0.2 22",
                    "AGY_COMPANION_ALLOW_REMOTE_SSH": "1",
                    "GEMINI_API_KEY": real_gem,
                    "GOOGLE_API_KEY": real_goog,
                    "AGY_COMPANION_CONFIG": str(cfg),
                }
            ),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        lines = res.stdout.splitlines()
        self.assertGreaterEqual(len(lines), 2, res.stdout)
        act = lines[0].removeprefix("ACT:").split("|")
        self.assertEqual(act[0], f"http://127.0.0.1:{port}")
        self.assertEqual(act[1], real_gem)
        self.assertEqual(act[2], real_goog)
        self.assertNotIn("GEMINI_API_KEY", act[3], "Real GEMINI_API_KEY must not enter restore vars")
        self.assertNotIn("GOOGLE_API_KEY", act[3], "GOOGLE_API_KEY must never enter restore vars")
        # 3-state check: CLOUD_CODE_URL restored to "" (empty, NOT <UNSET>), GOOGLE_GEMINI_BASE_URL restored to <UNSET>
        self.assertEqual(lines[1], f"SCRUB:|<UNSET>|{real_gem}|{real_goog}")

    def test_t2_r13_env_sh_rejects_malicious_restore_prefix_and_hostile_port_output(self) -> None:
        """R13: env.sh safely merges valid restore prefix, rejects malicious prefix, and blocks hostile port output."""
        if os.name == "nt":
            self.skipTest("POSIX env.sh test")
        binary = self.require_binary()
        env = self.make_env()
        pwn_marker = env.root / "pwned_marker"

        # 1. Hostile binary outputs must never execute commands or export URLs.
        hostile_bin = env.root / "hostile_bin.sh"
        hostile_bin.write_text('#!/bin/sh\nprintf "%s\\n" "$HOSTILE_OUT"\n', encoding="utf-8")
        os.chmod(hostile_bin, 0o755)

        en = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--binary", str(hostile_bin), "--json"],
            env=env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}),
        )
        self.assertEqual(en.returncode, 0, en.stderr)
        env_sh = env.data_dir / "env.sh"

        for bad_out in (f"4141; touch {pwn_marker}", "0", "99999", "notaport", ""):
            with self.subTest(hostile_out=bad_out):
                r = run_cli(
                    ["/bin/sh", "-c", '. "$1"; printf "%s|%s" "${CLOUD_CODE_URL:-}" "${AGY_COMPANION_HOOK_ACTIVE:-}"', "_", str(env_sh)],
                    env=env.build_env(
                        extra={
                            "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                            "HOSTILE_OUT": bad_out,
                        }
                    ),
                )
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertEqual(r.stdout, "|")
                self.assertFalse(pwn_marker.exists(), f"Command injection occurred for {bad_out!r}!")

        # 2. Valid vs malicious JETSKI_MCP_ENV_RESTORE_PREFIX with valid port output "4545"
        r_valid = run_cli(
            ["/bin/sh", "-c", '. "$1"; printf "%s|%s" "${JETSKI_MCP_ENV_RESTORE_PREFIX:-}" "${JETSKI_MCP_ENV_RESTORE_VARS:-}"', "_", str(env_sh)],
            env=env.build_env(
                extra={
                    "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                    "HOSTILE_OUT": "4545",
                    "JETSKI_MCP_ENV_RESTORE_PREFIX": "CUSTOM_",
                    "JETSKI_MCP_ENV_RESTORE_VARS": "FOO",
                }
            ),
        )
        self.assertEqual(r_valid.returncode, 0, r_valid.stderr)
        pref, merged_vars = r_valid.stdout.split("|", 1)
        self.assertEqual(pref, "CUSTOM_")
        self.assertIn("FOO", merged_vars)
        self.assertIn("CLOUD_CODE_URL", merged_vars)

        r_evil = run_cli(
            ["/bin/sh", "-c", '. "$1"; printf "%s" "${JETSKI_MCP_ENV_RESTORE_PREFIX:-}"', "_", str(env_sh)],
            env=env.build_env(
                extra={
                    "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                    "HOSTILE_OUT": "4545",
                    "JETSKI_MCP_ENV_RESTORE_PREFIX": f"$(touch {pwn_marker})",
                }
            ),
        )
        self.assertEqual(r_evil.returncode, 0, r_evil.stderr)
        self.assertFalse(pwn_marker.exists(), "Malicious restore prefix executed shell command!")
        self.assertRegex(r_evil.stdout, r"^[A-Za-z_][A-Za-z0-9_]*$")


class TestTier2RolloutR14ShellRcHooksSafetyBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R14"

    def test_t2_r14_symlink_and_readonly_rc_manual_required_and_follow_opt_in(self) -> None:
        """R14: Symlink/read-only RC returns manual_required safely; --follow-rc-symlink updates regular target only."""
        if os.name == "nt":
            self.skipTest("POSIX symlink/chmod RC test")
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        # 1. Symlinked .zlogin (Stow/Nix style) -> default does NOT mutate symlink or target.
        dotfiles_target = env.root / "dotfiles" / "zlogin"
        dotfiles_target.parent.mkdir(parents=True, exist_ok=True)
        dotfiles_target.write_text("# dotfiles original\n", encoding="utf-8")
        zlogin = env.home_dir / ".zlogin"
        os.symlink(dotfiles_target, zlogin)

        r_def = run_cli([str(binary), "integration", "enable", "--shell", "zsh", "--json"], env=proc_env)
        self.assertEqual(r_def.returncode, 0, r_def.stderr)
        d_def = r_def.json_stdout()
        self.assertEqual(d_def["state"], "manual_required", d_def)
        self.assertNotIn("manual_required", d_def)
        self.assertTrue(d_def.get("manual_snippet"), d_def)
        self.assertTrue(zlogin.is_symlink())
        self.assertEqual(dotfiles_target.read_text(encoding="utf-8"), "# dotfiles original\n")
        self.assertTrue((env.data_dir / "env.sh").is_file())

        # 2. Explicit --follow-rc-symlink updates writable regular target.
        r_follow = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--follow-rc-symlink", "--json"],
            env=proc_env,
        )
        self.assertEqual(r_follow.returncode, 0, r_follow.stderr)
        self.assertIs(r_follow.json_stdout()["enabled"], True)
        self.assertIn("# >>> agy-companion", dotfiles_target.read_text(encoding="utf-8"))

        # 3. Dangling symlink with --follow-rc-symlink fails closed (non-zero).
        dangling = env.root / "dangling_rc"
        os.symlink(env.root / "nonexistent_target", dangling)
        r_dang = run_cli(
            [
                str(binary),
                "integration",
                "enable",
                "--shell",
                "zsh",
                "--rc-file",
                str(dangling),
                "--follow-rc-symlink",
            ],
            env=proc_env,
        )
        self.assertNotEqual(r_dang.returncode, 0)

        # 4. Read-only regular file (0444) returns manual_required without mutating file.
        ro_rc = env.root / "readonly_rc"
        ro_rc.write_text("# readonly\n", encoding="utf-8")
        os.chmod(ro_rc, 0o444)
        try:
            r_ro = run_cli(
                [str(binary), "integration", "enable", "--shell", "zsh", "--rc-file", str(ro_rc), "--json"],
                env=proc_env,
            )
            self.assertEqual(r_ro.returncode, 0, r_ro.stderr)
            d_ro = r_ro.json_stdout()
            self.assertEqual(d_ro["state"], "manual_required", d_ro)
            self.assertNotIn("manual_required", d_ro)
            self.assertEqual(ro_rc.read_text(encoding="utf-8"), "# readonly\n")
        finally:
            os.chmod(ro_rc, 0o644)

    def test_t2_r14_unbalanced_markers_fail_closed_with_line_numbers(self) -> None:
        """R14: Unbalanced, reversed, or duplicate markers fail closed with line numbers and zero RC mutation."""
        if os.name == "nt":
            self.skipTest("POSIX RC marker parser test")
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env(extra={"SHELL": "/bin/bash"})
        cases = (
            ("missing_end", "export A=1\n# >>> agy-companion (test) >>>\n. /tmp/x\n"),
            ("reversed", "# <<< agy-companion <<<\n# >>> agy-companion (test) >>>\n"),
            (
                "duplicate_begin",
                "# >>> agy-companion (a) >>>\n# >>> agy-companion (b) >>>\n# <<< agy-companion <<<\n",
            ),
        )
        for label, content in cases:
            with self.subTest(case=label):
                rc = env.root / f"bad_{label}.profile"
                rc.write_text(content, encoding="utf-8")
                res = run_cli(
                    [str(binary), "integration", "enable", "--shell", "bash", "--rc-file", str(rc)],
                    env=proc_env,
                )
                self.assertNotEqual(res.returncode, 0, f"Expected non-zero for {label}")
                self.assertRegex(res.stderr + res.stdout, r"\d+", f"Expected line number in error for {label}")
                self.assertEqual(rc.read_text(encoding="utf-8"), content)


class TestTier2RolloutR15HookEnvDaemonBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R15"

    def test_t2_r15_hook_env_fails_open_when_plugin_missing_or_config_corrupt(self) -> None:
        """R15: hook-env prints 0 bytes and exits 0 when plugin.json is absent or config.json is corrupt."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        # 1. Missing plugin.json -> fail open (stdout empty, exit 0)
        r_no_plugin = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(r_no_plugin.returncode, 0)
        self.assertEqual(r_no_plugin.stdout, "")

        # 2. Present plugin.json but corrupt config.json -> fail open (stdout empty, exit 0)
        _install_dummy_plugin_json(env)
        bad_cfg = env.write_config("{corrupt-json", path=env.root / "corrupt.json")
        r_corrupt = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(bad_cfg)}),
        )
        self.assertEqual(r_corrupt.returncode, 0)
        self.assertEqual(r_corrupt.stdout, "")

    def test_t2_r15_hook_env_fails_open_on_slow_non_companion_port(self) -> None:
        """R15: hook-env fails open quickly (stdout empty, exit 0) when port is held by a slow non-companion server."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        slow = self.start_mock_gemini()
        slow.set_handler(lambda _req: JsonAction(status=200, payload={"status": "not-companion"}, delay_s=0.8))
        cfg = env.write_config(env.make_config_dict(listen_port=slow.port))
        _install_dummy_plugin_json(env)

        res = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
            timeout=5.0,
        )
        self.assertEqual(res.returncode, 0)
        self.assertEqual(res.stdout, "")


class TestTier2RolloutR16IntegrationCliBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R16"

    def test_t2_r16_integration_rejects_path_traversal_and_invalid_shell(self) -> None:
        """R16: integration CLI fails closed on .. traversal in --rc-file and unknown --shell values."""
        binary = self.require_binary()
        env = self.make_env()
        r_trav = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--rc-file", str(env.root / "a" / ".." / "evil")],
            env=env.build_env(),
        )
        self.assertNotEqual(r_trav.returncode, 0)
        self.assertFalse((env.root / "evil").exists())

        r_shell = run_cli(
            [str(binary), "integration", "enable", "--shell", "bogusshell"],
            env=env.build_env(),
        )
        self.assertNotEqual(r_shell.returncode, 0)

    def test_t2_r16_integration_disable_when_not_enabled_is_clean_noop(self) -> None:
        """R16: Running integration disable on clean HOME exits 0 with enabled=false and creates no RC files."""
        if os.name == "nt":
            self.skipTest("POSIX integration test")
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli(
            [str(binary), "integration", "disable", "--shell", "zsh", "--json"],
            env=env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIs(res.json_stdout()["enabled"], False)
        self.assertFalse((env.home_dir / ".zlogin").exists())


class TestTier2RolloutR17LaunchEnvRestoreBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R17"

    def test_t2_r17_launch_preserves_existing_real_gemini_and_google_api_keys(self) -> None:
        """R17: launch never overwrites pre-existing real GEMINI_API_KEY or GOOGLE_API_KEY in child app."""
        if os.name == "nt":
            self.skipTest("POSIX fake app script test")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        dump_path = env.root / "preserved_env.json"
        fake_app = env.root / "dump_env.sh"
        _write_env_dumper_script(fake_app, dump_path)

        real_gem = "AIzaSyRolloutT2R17Gem12"
        real_goog = "AIzaSyRolloutT2R17Goog1"
        res = run_cli(
            [str(binary), "launch", "--port", str(port), "--config", str(cfg), "--app", str(fake_app)],
            env=env.build_env(extra={"GEMINI_API_KEY": real_gem, "GOOGLE_API_KEY": real_goog}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        app_env = _wait_for_json_file(dump_path, timeout=5.0)
        self.assertEqual(app_env.get("GEMINI_API_KEY"), real_gem)
        self.assertEqual(app_env.get("GOOGLE_API_KEY"), real_goog)
        restore_vars = app_env.get("JETSKI_MCP_ENV_RESTORE_VARS", "")
        self.assertNotIn("GEMINI_API_KEY", restore_vars)
        self.assertNotIn("GOOGLE_API_KEY", restore_vars)

    def test_t2_r17_launch_missing_app_exits_nonzero(self) -> None:
        """R17: launch with a nonexistent --app path fails cleanly with non-zero exit code."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
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
                str(env.root / "definitely_missing_app"),
            ],
            env=env.build_env(),
        )
        self.assertNotEqual(res.returncode, 0)


class TestTier2RolloutR18McpLauncherCloudCodeBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R18"

    def test_t2_r18_set_launcher_integration_denies_unknown_fields(self) -> None:
        """R18: set_launcher_integration enforces deny_unknown_fields and rejects rc_file or follow_rc_symlink."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
        )
        for bad_args in (
            {"enabled": True, "rc_file": "/etc/passwd"},
            {"enabled": True, "follow_rc_symlink": True},
            {"enabled": True, "shell": "bash"},
        ):
            with self.subTest(args=bad_args):
                resp = mcp.call_tool_raw("set_launcher_integration", bad_args)
                mcp.assert_clean_stdout()
                self.assertTrue(
                    McpStdioClient.is_tool_error(resp),
                    f"Expected deny_unknown_fields rejection for {bad_args!r}: {resp!r}",
                )
        self.assertFalse((env.home_dir / ".zlogin").exists())
        self.assertFalse((env.data_dir / "env.sh").exists())

    def test_t2_r18_mcp_never_follows_symlinked_rc_and_reports_manual_required(self) -> None:
        """R18: MCP set_launcher_integration refuses to mutate symlinked .zlogin and returns manual_required."""
        if os.name == "nt":
            self.skipTest("POSIX symlink MCP test")
        env = self.make_env()
        target = env.root / "nix_store_zlogin"
        target.write_text("# managed by nix\n", encoding="utf-8")
        zlogin = env.home_dir / ".zlogin"
        os.symlink(target, zlogin)

        cfg = env.write_config(env.make_config_dict())
        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
        )
        res = mcp.call_tool("set_launcher_integration", {"enabled": True})
        mcp.assert_clean_stdout()
        li = res.get("structuredContent", {}).get("launcher_integration", {})
        self.assertEqual(li.get("state"), "manual_required", li)
        self.assertNotIn("manual_required", li)
        self.assertTrue(li.get("manual_snippet"), li)
        self.assertTrue(zlogin.is_symlink())
        self.assertEqual(target.read_text(encoding="utf-8"), "# managed by nix\n")


class TestTier2RolloutR19UiRolloutBundleBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R19"

    def test_t2_r19_ui_sources_and_bundle_have_zero_s5_sinks_or_external_tags(self) -> None:
        """R19: S5 security gate confirms zero forbidden sinks or external tags in ui/src/ and ui/dist/index.html."""
        src_files = sorted((self.repo_root / "ui" / "src").glob("*.ts"))
        self.assertTrue(src_files)
        for src in src_files:
            code = _strip_ts_comments(src.read_text(encoding="utf-8"))
            self.assertIsNone(_S5_FORBIDDEN_SINKS_RE.search(code), f"Forbidden S5 sink in {src}")
            self.assertIsNone(_S5_EXTERNAL_TAGS_RE.search(code), f"External tag in {src}")

        bundle = (self.repo_root / "ui" / "dist" / "index.html").read_text(encoding="utf-8")
        self.assertIsNone(_S5_FORBIDDEN_SINKS_RE.search(bundle), "Forbidden S5 sink in ui/dist/index.html")
        self.assertIsNone(_S5_EXTERNAL_TAGS_RE.search(bundle), "External tag in ui/dist/index.html")

    def test_t2_r19_ui_registers_handlers_before_connect_and_is_platform_aware(self) -> None:
        """R19: ui/src/main.ts registers MCP App handlers before connect() and supports non-macOS launch snippets."""
        main_ts = _strip_ts_comments((self.repo_root / "ui" / "src" / "main.ts").read_text(encoding="utf-8"))
        connect_pos = main_ts.find(".connect(")
        self.assertGreater(connect_pos, 0)
        for handler in ("ontoolinput", "ontoolresult", "onhostcontextchanged"):
            pos = main_ts.find(handler)
            self.assertGreater(pos, 0, f"Missing {handler}")
            self.assertLess(pos, connect_pos, f"{handler} must be registered before app.connect()")
        all_ts = "\n".join(
            _strip_ts_comments(p.read_text(encoding="utf-8"))
            for p in sorted((self.repo_root / "ui" / "src").glob("*.ts"))
        )
        self.assertRegex(all_ts, r"(?i)(windows|linux|win32|platform|userAgent|navigator)")


class TestTier2RolloutR20RunCompanionHardeningBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R20"

    def test_t2_r20_run_companion_preserves_real_gemini_api_key_while_scrubbing_proxy_urls(self) -> None:
        """R20: run-companion.sh preserves non-placeholder GEMINI_API_KEY while scrubbing proxy URLs."""
        if os.name == "nt":
            self.skipTest("POSIX launcher script test")
        env = self.make_env()
        iso_plugin = env.root / "iso_plugin"
        (iso_plugin / "scripts").mkdir(parents=True, exist_ok=True)
        iso_script = iso_plugin / "scripts" / "run-companion.sh"
        shutil.copy2(self.repo_root / "plugin" / "scripts" / "run-companion.sh", iso_script)
        os.chmod(iso_script, 0o755)
        shutil.copy2(self.repo_root / "plugin" / "plugin.json", iso_plugin / "plugin.json")

        dump_path = env.root / "child_real_env.json"
        fake_bin = env.root / "fake_bin.sh"
        _write_env_dumper_script(fake_bin, dump_path)

        real_key = "AIzaSyRolloutT2R20Real1"
        res = run_cli(
            [str(iso_script), "serve"],
            env=env.build_env(
                extra={
                    "AGY_COMPANION_BIN": str(fake_bin),
                    "CLOUD_CODE_URL": "http://127.0.0.1:4141",
                    "GOOGLE_GEMINI_BASE_URL": "http://127.0.0.1:4141",
                    "GEMINI_API_KEY": real_key,
                }
            ),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        child_env = json.loads(dump_path.read_text(encoding="utf-8"))
        self.assertEqual(child_env.get("GEMINI_API_KEY"), real_key)
        self.assertNotIn("CLOUD_CODE_URL", child_env)
        self.assertNotIn("GOOGLE_GEMINI_BASE_URL", child_env)

    def test_t2_r20_run_companion_skips_mismatched_cache_version_and_uses_path(self) -> None:
        """R20: Version-mismatched cached binary does not shadow PATH binary."""
        if os.name == "nt":
            self.skipTest("POSIX launcher script test")
        env = self.make_env()
        iso_plugin = env.root / "iso_plugin"
        (iso_plugin / "scripts").mkdir(parents=True, exist_ok=True)
        iso_script = iso_plugin / "scripts" / "run-companion.sh"
        shutil.copy2(self.repo_root / "plugin" / "scripts" / "run-companion.sh", iso_script)
        os.chmod(iso_script, 0o755)
        shutil.copy2(self.repo_root / "plugin" / "plugin.json", iso_plugin / "plugin.json")

        # Stale cache with mismatched .version
        cache_bin_dir = env.data_root / "agy-companion" / "bin"
        cache_bin_dir.mkdir(parents=True, exist_ok=True)
        stale_cache = cache_bin_dir / "agy-companion"
        stale_cache.write_text("#!/bin/sh\necho MISMATCHED_CACHE_BAD\n", encoding="utf-8")
        os.chmod(stale_cache, 0o755)
        (cache_bin_dir / ".version").write_text("0.0.1-stale\n", encoding="utf-8")

        path_bin_dir = env.root / "path_bin"
        path_bin_dir.mkdir(parents=True, exist_ok=True)
        path_bin = path_bin_dir / "agy-companion"
        path_bin.write_text("#!/bin/sh\necho PATH_FALLBACK_OK\n", encoding="utf-8")
        os.chmod(path_bin, 0o755)

        safe_bin = _build_safe_bin_dir(env)
        res = run_cli(
            [str(iso_script), "serve"],
            env=env.build_env(
                extra={
                    "PATH": f"{path_bin_dir}:{safe_bin}",
                    "XDG_DATA_HOME": str(env.data_root),
                    "ANTIGRAVITY_EXECUTABLE_DATA_DIR": None,
                    "AGY_COMPANION_BIN": None,
                }
            ),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), "PATH_FALLBACK_OK")

    def test_t2_r20_run_companion_skips_missing_symlink_and_fifo_cache_version_and_uses_path(self) -> None:
        """R20: Missing, symlinked, or FIFO .version in XDG cache must skip cache and exec PATH without hanging."""
        if os.name == "nt":
            self.skipTest("POSIX launcher script test")
        env = self.make_env()
        iso_plugin = env.root / "iso_plugin"
        (iso_plugin / "scripts").mkdir(parents=True, exist_ok=True)
        iso_script = iso_plugin / "scripts" / "run-companion.sh"
        shutil.copy2(self.repo_root / "plugin" / "scripts" / "run-companion.sh", iso_script)
        os.chmod(iso_script, 0o755)
        shutil.copy2(self.repo_root / "plugin" / "plugin.json", iso_plugin / "plugin.json")
        expected_version = json.loads((iso_plugin / "plugin.json").read_text(encoding="utf-8"))["version"]

        cache_bin_dir = env.data_root / "agy-companion" / "bin"
        cache_bin_dir.mkdir(parents=True, exist_ok=True)
        stale_cache = cache_bin_dir / "agy-companion"
        stale_cache.write_text("#!/bin/sh\necho UNVERSIONED_CACHE_BAD\n", encoding="utf-8")
        os.chmod(stale_cache, 0o755)

        path_bin_dir = env.root / "path_bin"
        path_bin_dir.mkdir(parents=True, exist_ok=True)
        path_bin = path_bin_dir / "agy-companion"
        path_bin.write_text("#!/bin/sh\necho PATH_FALLBACK_OK\n", encoding="utf-8")
        os.chmod(path_bin, 0o755)

        safe_bin = _build_safe_bin_dir(env)
        base_env = env.build_env(
            extra={
                "PATH": f"{path_bin_dir}:{safe_bin}",
                "XDG_DATA_HOME": str(env.data_root),
                "ANTIGRAVITY_EXECUTABLE_DATA_DIR": None,
                "AGY_COMPANION_BIN": None,
            }
        )
        ver_path = cache_bin_dir / ".version"
        real_ver_target = env.root / "real_version.txt"
        real_ver_target.write_text(f"{expected_version}\n", encoding="utf-8")

        for mode in ("missing", "symlink", "fifo"):
            with self.subTest(version_file=mode):
                if ver_path.is_symlink() or ver_path.exists():
                    ver_path.unlink()
                if mode == "symlink":
                    os.symlink(real_ver_target, ver_path)
                elif mode == "fifo":
                    os.mkfifo(ver_path)

                res = run_cli(
                    ["/bin/bash", str(iso_script), "--version"],
                    env=base_env,
                    timeout=5.0,
                )
                self.assertEqual(res.returncode, 0, f"{mode}: stderr={res.stderr}")
                self.assertEqual(
                    res.stdout.strip(),
                    "PATH_FALLBACK_OK",
                    f"{mode}: expected PATH fallback, got stdout={res.stdout!r}",
                )


class TestTier2RolloutR21EmbeddedPluginParityBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R21"

    def test_t2_r21_repo_plugin_dir_contains_exact_7_regular_files(self) -> None:
        """R21: Repository plugin/ bundle contains exact 7 non-symlink regular files with zero stray artifacts."""
        plugin_root = self.repo_root / "plugin"
        actual = sorted(
            p.relative_to(plugin_root).as_posix()
            for p in plugin_root.rglob("*")
            if p.is_file() or p.is_symlink()
        )
        expected = sorted(
            [
                "README.md",
                "assets/logo.svg",
                "mcp_config.json",
                "plugin.json",
                "scripts/run-companion.sh",
                "sidecars/proxy/sidecar.json",
                "skills/companion/SKILL.md",
            ]
        )
        self.assertEqual(actual, expected)
        for rel in expected:
            self.assertFalse((plugin_root / rel).is_symlink(), f"plugin/{rel} must not be a symlink")

    def test_t2_r21_install_rejects_traversal_in_plugins_dir(self) -> None:
        """R21: S6 rejects .. traversal in --plugins-dir before creating any plugin files."""
        binary = self.require_binary()
        env = self.make_env()
        bad_dir = str(env.root / "sub" / ".." / "escaped_plugins")
        res = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                bad_dir,
                "--no-launcher-hook",
                "--no-start-proxy",
            ],
            env=env.build_env(),
        )
        self.assertNotEqual(res.returncode, 0)
        self.assertFalse((env.root / "escaped_plugins" / "agy-companion").exists())


class TestTier2RolloutR22InstallCliBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R22"

    def test_t2_r22_dev_symlink_guard_and_unrelated_local_bin_preservation(self) -> None:
        """R22: install refuses symlinked dev plugin without --force and never overwrites unrelated ~/.local/bin file."""
        if os.name == "nt":
            self.skipTest("POSIX symlink install guard test")
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))

        plugins_dir = env.root / "plugins"
        plugins_dir.mkdir(parents=True, exist_ok=True)
        dev_target = env.root / "dev_checkout_plugin"
        dev_target.mkdir(parents=True, exist_ok=True)
        plugin_link = plugins_dir / "agy-companion"
        os.symlink(dev_target, plugin_link)

        # Unrelated user file at ~/.local/bin/agy-companion must be preserved.
        local_bin = env.home_dir / ".local" / "bin" / "agy-companion"
        local_bin.parent.mkdir(parents=True, exist_ok=True)
        local_bin.write_text("#!/bin/sh\necho UNRELATED_USER_TOOL\n", encoding="utf-8")
        os.chmod(local_bin, 0o755)

        # 1. Refused without --force
        r_refuse = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                str(plugins_dir),
                "--no-launcher-hook",
                "--no-start-proxy",
            ],
            env=env.build_env(),
        )
        self.assertNotEqual(r_refuse.returncode, 0, "Expected dev-symlink guard refusal without --force")
        self.assertTrue(plugin_link.is_symlink(), "Symlink must remain intact after refusal")

        # 2. Succeeds with --force while leaving unrelated ~/.local/bin/agy-companion intact
        r_force = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                str(plugins_dir),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--force",
                "--json",
            ],
            env=env.build_env(),
        )
        self.assertEqual(r_force.returncode, 0, f"{r_force.stdout}\n{r_force.stderr}")
        self.assertFalse(plugin_link.is_symlink())
        self.assertTrue((plugin_link / "plugin.json").is_file())
        self.assertIn("UNRELATED_USER_TOOL", local_bin.read_text(encoding="utf-8"))

    def test_t2_r22_fresh_install_port_24141_collision_falls_back_to_24142_through_24150(self) -> None:
        """R22: Fresh install with non-companion listener on 24141 auto-falls back to 24142..=24150 and writes 0600 config."""
        binary = self.require_binary()
        probe = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            probe.bind(("127.0.0.1", 24141))
        except OSError:
            self.skipTest("Host port 24141 is already occupied; skipping 24141 collision fallback test safely")
        finally:
            probe.close()

        blocker = MockGeminiServer(port=24141)
        blocker.set_handler(lambda _req: JsonAction(status=500, payload={"error": "occupied"}))
        try:
            self.stack.enter_context(blocker)
        except OSError:
            self.skipTest("Host port 24141 is already occupied; skipping 24141 collision fallback test safely")

        env = self.make_env()
        # Ensure fresh install with NO pre-existing config.json
        self.assertFalse(env.config_path.exists())
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
            env=env.build_env(set_config_env=False),
        )
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        self.assertTrue(env.config_path.is_file(), "Fresh install should persist fallback proxy.port to config.json")
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(env.config_path).st_mode), 0o600)
        written = json.loads(env.config_path.read_text(encoding="utf-8"))
        chosen_port = written["proxy"]["port"]
        self.assertGreaterEqual(chosen_port, 24142)
        self.assertLessEqual(chosen_port, 24150)


class TestTier2RolloutR23UninstallCliBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R23"

    def test_t2_r23_dry_run_foreign_plugin_and_symlink_plugin_protection(self) -> None:
        """R23: --dry-run mutates nothing, foreign plugin dirs are untouched, and symlink plugin unlinks symlink only."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
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

        # 1. --dry-run leaves everything intact
        dry = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--dry-run", "--json"],
            env=proc_env,
        )
        self.assertEqual(dry.returncode, 0, dry.stderr)
        self.assertIs(dry.json_stdout().get("dry_run"), True)
        self.assertTrue((plugins_dir / "agy-companion" / "plugin.json").is_file())
        if os.name != "nt":
            self.assertTrue((env.data_dir / "env.sh").is_file())
            self.assertTrue((env.home_dir / ".zlogin").is_file())

        # 2. Foreign plugin dir (mismatched plugin.json name) is NOT deleted
        foreign_plugins = env.root / "foreign_plugins"
        foreign_dir = foreign_plugins / "agy-companion"
        foreign_dir.mkdir(parents=True, exist_ok=True)
        (foreign_dir / "plugin.json").write_text('{"name":"unrelated-plugin","version":"1.0.0"}\n', encoding="utf-8")
        sentinel = foreign_dir / "important.txt"
        sentinel.write_text("KEEP\n", encoding="utf-8")
        r_for = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(foreign_plugins), "--json"],
            env=proc_env,
        )
        self.assertEqual(r_for.returncode, 0, r_for.stderr)
        self.assertTrue(sentinel.is_file(), "Foreign plugin dir must NOT be deleted")

        # 3. Symlinked plugin dir unlinks symlink ONLY, preserving target directory contents
        if os.name != "nt":
            sym_plugins = env.root / "sym_plugins"
            sym_plugins.mkdir(parents=True, exist_ok=True)
            target_dir = env.root / "real_dev_plugin"
            target_dir.mkdir(parents=True, exist_ok=True)
            target_marker = target_dir / "dev_file.txt"
            target_marker.write_text("DEV_SAFE\n", encoding="utf-8")
            sym_link = sym_plugins / "agy-companion"
            os.symlink(target_dir, sym_link)
            r_sym = run_cli(
                [str(binary), "uninstall", "--plugins-dir", str(sym_plugins), "--json"],
                env=proc_env,
            )
            self.assertEqual(r_sym.returncode, 0, r_sym.stderr)
            self.assertFalse(sym_link.is_symlink() or sym_link.exists())
            self.assertTrue(target_marker.is_file(), "Symlink target directory must NOT be deleted")

    def test_t2_r23_uninstall_refuses_to_kill_unrelated_pid_or_delete_unrelated_local_bin(self) -> None:
        """R23: 4-layer PID verification refuses to kill an unrelated process and preserves unrelated ~/.local/bin file."""
        if os.name == "nt":
            self.skipTest("POSIX PID guard test")
        binary = self.require_binary()
        env = self.make_env()
        plugins_dir = env.root / "plugins"
        plugins_dir.mkdir(parents=True, exist_ok=True)

        sleeper = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            start_new_session=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            pid_file = env.data_dir / "proxy.pid"
            pid_file.write_text(
                json.dumps({"pid": sleeper.pid, "port": find_free_port(), "version": "0.1.0"}),
                encoding="utf-8",
            )
            local_bin = env.home_dir / ".local" / "bin" / "agy-companion"
            local_bin.parent.mkdir(parents=True, exist_ok=True)
            local_bin.write_text("#!/bin/sh\necho KEEP_ME\n", encoding="utf-8")
            os.chmod(local_bin, 0o755)

            res = run_cli(
                [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"],
                env=env.build_env(),
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            self.assertIsNone(sleeper.poll(), "Unrelated Python process was killed by uninstall!")
            self.assertTrue(local_bin.is_file(), "Unrelated ~/.local/bin/agy-companion was deleted!")
        finally:
            if sleeper.poll() is None:
                sleeper.kill()
                sleeper.wait(timeout=2.0)


class TestTier2RolloutR24WindowsParityBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R24"

    def test_t2_r24_windows_integration_unsupported_and_manifests_avoid_bash(self) -> None:
        """R24: Windows integration reports state=unsupported and Windows manifests do not shell out to bash."""
        integ_rs = _read_rust_module_source(self.repo_root / "crates" / "companion-core" / "src" / "integration")
        install_rs = _read_rust_module_source(self.repo_root / "crates" / "companion-cli" / "src" / "install")
        self.assertRegex(integ_rs, r"cfg\(\s*(?:windows|not\s*\(\s*unix\s*\))\s*\)")
        self.assertRegex(integ_rs, r"IntegrationState::Unsupported|IntegrationStatus::unsupported\(")
        self.assertIn("agy-companion.exe", install_rs)

    def test_t2_r24_windows_shortcut_hides_console_window_and_cleans_up_on_uninstall(self) -> None:
        """R24: Windows Start Menu shortcut hides console window and uninstall includes shortcut removal."""
        install_rs = _read_rust_module_source(self.repo_root / "crates" / "companion-cli" / "src" / "install")
        self.assertRegex(install_rs, r"CREATE_NO_WINDOW|0x0800_?0000|WindowStyle")
        self.assertGreaterEqual(
            install_rs.count("Antigravity (Companion).lnk"),
            1,
            "Expected Start Menu shortcut filename in install module",
        )


class TestTier2RolloutR25RolloutDocsBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "R25"

    def test_t2_r25_no_curl_pipe_install_sh_or_install_ps1_files_or_advertisements(self) -> None:
        """R25: User-approved PR 3 scope — scripts/install.sh & install.ps1 do NOT exist and are NOT advertised."""
        self.assertFalse((self.repo_root / "scripts" / "install.sh").exists())
        self.assertFalse((self.repo_root / "scripts" / "install.ps1").exists())

        strict_docs = (
            self.repo_root / "README.md",
            self.repo_root / "plugin" / "README.md",
        )
        for doc in strict_docs:
            with self.subTest(doc=str(doc.relative_to(self.repo_root))):
                self.assertTrue(doc.is_file(), f"Missing {doc}")
                text = doc.read_text(encoding="utf-8")
                self.assertNotIn("releases/latest/download/install.sh", text)
                self.assertNotIn("releases/latest/download/install.ps1", text)
                self.assertNotIn("scripts/install.ps1", text)
                self.assertIsNone(re.search(r"curl[^\n]*install\.sh", text, re.IGNORECASE))
                self.assertIsNone(re.search(r"irm[^\n]*install\.ps1", text, re.IGNORECASE))

        design_doc = self.repo_root / "docs" / "design" / "dock-launcher-and-install.md"
        with self.subTest(doc=design_doc.name):
            self.assertTrue(design_doc.is_file(), f"Missing {design_doc}")
            design_text = design_doc.read_text(encoding="utf-8")
            self.assertNotIn("releases/latest/download/install.sh", design_text)
            self.assertNotIn("releases/latest/download/install.ps1", design_text)
            self.assertIsNone(re.search(r"curl[^\n]*install\.sh", design_text, re.IGNORECASE))
            self.assertIsNone(re.search(r"irm[^\n]*install\.ps1", design_text, re.IGNORECASE))

    def test_t2_r25_rollout_docs_contain_zero_real_google_api_keys(self) -> None:
        """R25: S2 secret hygiene — rollout documentation files contain zero 39-char Google API keys."""
        secret_re = re.compile(r"AIza[0-9A-Za-z_-]{35}")
        docs_to_check = (
            self.repo_root / "README.md",
            self.repo_root / "AGENTS.md",
            self.repo_root / "docs" / "README.md",
            self.repo_root / "docs" / "design" / "dock-launcher-and-install.md",
            self.repo_root / "plugin" / "README.md",
            self.repo_root / "plugin" / "skills" / "companion" / "SKILL.md",
        )
        for doc in docs_to_check:
            with self.subTest(doc=str(doc.relative_to(self.repo_root))):
                self.assertTrue(doc.is_file(), f"Missing {doc}")
                self.assertIsNone(secret_re.search(doc.read_text(encoding="utf-8")))
