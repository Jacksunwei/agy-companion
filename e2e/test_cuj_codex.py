"""OpenAI Codex provider end-to-end opaque-box CUJ (`--suite cuj`).

Exercises the real compiled `agy-companion serve` binary (proxy + Sidecar Web UI
sharing one `CodexAuthManager`) against hermetic loopback mock OAuth, Codex
Responses SSE, and CloudCode servers:
1. Initial `/_api/get_settings` shows `codex.authenticated == False`, 4 curated
   GPT-6.x `supported_codex_models`, `codex_login_pending == False`, and Day-0
   `fetchAvailableModels` omits Codex slots.
2. `POST /_api/start_codex_login` binds an ephemeral loopback callback listener,
   returns `authorize_url` + `callback_listening == True`, and completes via the
   real loopback `/auth/callback?code=...&state=...` redirect, persisting `0600`
   `auth/codex.json` inside `0700` `auth/` without leaking tokens.
3. `POST /_api/test_connection` (`{"provider": "codex"}`) verifies live upstream
   connectivity against the mock Codex Responses server.
4. Zero-restart hot-reload injects Codex `M351..=M364` slots into
   `fetchAvailableModels`, translates `POST /v1beta/models/gpt-6.2-sol:streamGenerateContent`
   into Gemini SSE (`thought: true` + text + `usageMetadata`), and records
   `provider == "openai-codex"` in `/api/stats` & `/_api/get_usage`.
5. `POST /_api/disconnect_codex` clears `auth/codex.json`, resets
   `codex.authenticated == False`, and removes Codex slots on the next fetch.
"""

from __future__ import annotations

import base64
import json
import os
import stat
import urllib.parse
from typing import Any

from harness import (
    E2ETestCase,
    find_free_port,
    http_raw_request,
    poll_stats,
    poll_usage_events,
)
from mock_gemini import CapturedRequest, JsonAction, SseStreamAction


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _make_test_jwt(payload: dict[str, Any]) -> str:
    header = _b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode("utf-8"))
    body = _b64url(json.dumps(payload).encode("utf-8"))
    return f"{header}.{body}.e2e-sig"


def _sample_catalog() -> dict[str, Any]:
    return {
        "models": {
            "gemini-4-carbon-high": {
                "model": "MODEL_PLACEHOLDER_M427",
                "displayName": "Gemini 4 Carbon (High)",
                "thinkingLevel": 3,
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
        "groupedModels": {},
    }


class TestCujCodex(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "CODEX"

    def _sidecar_post(
        self,
        port: int,
        token: str,
        tool: str,
        payload: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        resp = http_raw_request(
            "127.0.0.1",
            port,
            "POST",
            f"/_api/{tool}",
            headers={
                "Content-Type": "application/json",
                "X-Sidecar-Token": token,
            },
            body=json.dumps(payload or {}).encode("utf-8"),
            timeout=10.0,
        )
        self.assertEqual(
            resp.status,
            200,
            f"/_api/{tool} returned HTTP {resp.status}: {resp.text}",
        )
        data = resp.json()
        self.assertIsInstance(data, dict)
        return data

    def _fetch_catalog(self, proxy_port: int, bearer: str) -> dict[str, Any]:
        resp = http_raw_request(
            "127.0.0.1",
            proxy_port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {bearer}",
            },
            body=b"{}",
            timeout=10.0,
        )
        self.assertEqual(resp.status, 200, f"fetchAvailableModels failed: {resp.text}")
        data = resp.json()
        self.assertIsInstance(data, dict)
        return data

    def test_cuj_codex_oauth_hot_reload_streaming_usage_and_disconnect(self) -> None:
        """Full Codex lifecycle via real `agy-companion serve` + Sidecar Web UI: OAuth loopback callback, test_connection, hot-reload M351+ slots, Responses SSE translation, usage telemetry, and disconnect."""
        self.require_binary()
        env = self.make_env()

        access_jwt = _make_test_jwt(
            {
                "exp": 4102444800,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct_e2e_codex_9876",
                    "chatgpt_plan_type": "pro",
                },
                "https://api.openai.com/profile": {
                    "email": "jack@example.com",
                },
            }
        )
        refresh_token = "rt_e2e_codex_secret_refresh_01"

        codex_sse_bytes = (
            'data: {"type":"response.reasoning_summary_text.delta","delta":"Plan step 1"}\n\n'
            'data: {"type":"response.output_text.delta","delta":"Hello from Codex!"}\n\n'
            'data: {"type":"response.completed","response":{"id":"resp_codex_e2e_1","status":"completed","usage":{"input_tokens":1500,"input_tokens_details":{"cached_tokens":900},"output_tokens":240,"output_tokens_details":{"reasoning_tokens":60}}}}\n\n'
        ).encode("utf-8")

        mock = self.start_mock_gemini()

        def handler(req: CapturedRequest) -> Any:
            if req.path.startswith("/oauth/token"):
                return JsonAction(
                    200,
                    {
                        "access_token": access_jwt,
                        "refresh_token": refresh_token,
                        "expires_in": 3600,
                    },
                )
            if req.path.startswith("/responses"):
                if (
                    req.header("authorization") == f"Bearer {access_jwt}"
                    and req.header("chatgpt-account-id") == "acct_e2e_codex_9876"
                ):
                    return SseStreamAction([codex_sse_bytes], status=200)
                return JsonAction(401, {"error": {"message": "unauthorized"}})
            if ":fetchAvailableModels" in req.path:
                return JsonAction(200, _sample_catalog())
            return JsonAction(404, {"error": f"unexpected path {req.path}"})

        mock.set_handler(handler)

        proxy_port = find_free_port()
        sidecar_port = find_free_port()
        while sidecar_port == proxy_port:
            sidecar_port = find_free_port()
        token = "e2e-codex-sidecar-token"

        cfg = env.write_config(
            env.make_config_dict(
                listen_port=proxy_port,
                cloudcode_upstream_url=mock.base_url,
                codex_base_url=mock.base_url,
            )
        )
        self.start_proxy(
            env,
            config_path=cfg,
            port=proxy_port,
            extra_env={
                "ANTIGRAVITY_SIDECAR_WEB_PORT": str(sidecar_port),
                "ANTIGRAVITY_SIDECAR_UI_TOKEN": token,
                "AGY_COMPANION_CODEX_OAUTH_ISSUER": mock.base_url,
                "AGY_COMPANION_CODEX_CALLBACK_PORT": "0",
            },
        )

        # 1. Initial Sidecar UI snapshot: signed out, 4 GPT-6.x models, not pending.
        snap0 = self._sidecar_post(sidecar_port, token, "get_settings")
        self.assertFalse(snap0["config"]["providers"]["codex"]["authenticated"])
        self.assertFalse(snap0["codex_login_pending"])
        self.assertEqual(
            [m["id"] for m in snap0["supported_codex_models"]],
            ["gpt-6.2-astra", "gpt-6.2-sol", "gpt-6.1-terra", "gpt-6-luna"],
        )

        # Day-0 catalog has no Codex models while signed out.
        cat0 = self._fetch_catalog(proxy_port, "codex-e2e-0")
        self.assertFalse(any("gpt-6" in k for k in cat0.get("models", {})))

        # 2. Start OAuth login via Sidecar API and complete through the real loopback callback listener.
        started = self._sidecar_post(sidecar_port, token, "start_codex_login")
        self.assertTrue(started["callback_listening"])
        self.assertFalse(started["manual_fallback"])
        auth_url = started["authorize_url"]
        parsed_auth = urllib.parse.urlparse(auth_url)
        params = urllib.parse.parse_qs(parsed_auth.query)
        redirect_uri = params["redirect_uri"][0]
        state_val = params["state"][0]
        cb_url = urllib.parse.urlparse(redirect_uri)
        self.assertIsNotNone(cb_url.port)

        snap_pending = self._sidecar_post(sidecar_port, token, "get_settings")
        self.assertTrue(snap_pending["codex_login_pending"])

        cb_resp = http_raw_request(
            "127.0.0.1",
            int(cb_url.port),
            "GET",
            f"{cb_url.path}?code=e2e-auth-code-1&state={urllib.parse.quote(state_val)}",
            timeout=10.0,
        )
        self.assertEqual(cb_resp.status, 200, f"callback failed: {cb_resp.text}")
        self.assertIn("Signed in", cb_resp.text)

        # Verify auth/codex.json persisted with strict Unix permissions and redacted in get_settings.
        codex_auth_path = env.codex_auth_path_for(cfg)
        self.assertTrue(codex_auth_path.is_file(), "auth/codex.json must exist after OAuth callback")
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(codex_auth_path.parent).st_mode), 0o700)
            self.assertEqual(stat.S_IMODE(os.lstat(codex_auth_path).st_mode), 0o600)

        snap_authed = self._sidecar_post(sidecar_port, token, "get_settings")
        raw_authed = json.dumps(snap_authed)
        self.assertNotIn(access_jwt, raw_authed)
        self.assertNotIn(refresh_token, raw_authed)
        self.assertNotIn("acct_e2e_codex_9876", raw_authed)
        self.assertFalse(snap_authed["codex_login_pending"])
        codex_cfg = snap_authed["config"]["providers"]["codex"]
        self.assertTrue(codex_cfg["authenticated"])
        self.assertEqual(codex_cfg["plan_type"], "pro")
        self.assertEqual(codex_cfg["account_hint"], "jack@example.com")

        # 3. Provider-aware test_connection verifies live Codex Responses connectivity.
        test_res = self._sidecar_post(
            sidecar_port, token, "test_connection", {"provider": "codex"}
        )
        self.assertTrue(test_res["ok"], f"Codex test_connection failed: {test_res}")
        self.assertNotIn(access_jwt, test_res["message"])

        # 4. Hot-reload: fetchAvailableModels injects M351..=M364 Codex slots with zero restart.
        cat1 = self._fetch_catalog(proxy_port, "codex-e2e-1")
        codex_entries = {
            k: v for k, v in cat1.get("models", {}).items() if "gpt-6" in k
        }
        self.assertGreaterEqual(len(codex_entries), 8, f"Expected >= 8 default Codex slots: {codex_entries.keys()}")
        codex_placeholders = {v.get("model") for v in codex_entries.values()}
        self.assertIn("MODEL_PLACEHOLDER_M351", codex_placeholders)
        for entry in codex_entries.values():
            self.assertIs(entry.get("supportsThoughtCirculation"), False)

        # 5. Streaming Gemini -> Codex Responses SSE translation + per-session stats + SQLite usage.
        conv_id = "conv-codex-e2e-42"
        stream_body = json.dumps(
            {
                "systemInstruction": {"parts": [{"text": f"Conversation ID: {conv_id}\nBe concise."}]},
                "contents": [{"role": "user", "parts": [{"text": "Say hello"}]}],
                "generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}},
            }
        ).encode("utf-8")
        stream_resp = http_raw_request(
            "127.0.0.1",
            proxy_port,
            "POST",
            "/v1beta/models/gpt-6.2-sol:streamGenerateContent?alt=sse",
            headers={"Content-Type": "application/json"},
            body=stream_body,
            timeout=10.0,
        )
        self.assertEqual(stream_resp.status, 200, f"Codex stream failed: {stream_resp.text}")
        self.assertIn("Plan step 1", stream_resp.text)
        self.assertIn('"thought":true', stream_resp.text.replace(" ", ""))
        self.assertIn("Hello from Codex!", stream_resp.text)
        self.assertIn("usageMetadata", stream_resp.text)

        # Verify outbound Codex request carries prompt_cache_key + session_id + developer SI placement.
        codex_reqs = [r for r in mock.requests if r.path.startswith("/responses")]
        self.assertGreaterEqual(len(codex_reqs), 2, f"Expected health check + stream /responses requests, got {len(codex_reqs)}")
        stream_codex_req = codex_reqs[-1]
        self.assertEqual(stream_codex_req.header("session_id"), conv_id)
        stream_codex_json = json.loads(stream_codex_req.body.decode("utf-8"))
        self.assertEqual(stream_codex_json["prompt_cache_key"], conv_id)
        self.assertEqual(stream_codex_json["instructions"], "You are a helpful coding assistant.")
        self.assertEqual(stream_codex_json["input"][0]["role"], "developer")
        self.assertIn("Be concise.", stream_codex_json["input"][0]["content"][0]["text"])
        self.assertEqual(stream_codex_json["input"][1]["role"], "user")

        # 5b. Follow-up multi-turn tool-call continuation with functionResponse inside role="model".
        tool_call_id = "call_12658be792e746a4a55dc9fd30d2e2f7"
        followup_body = json.dumps(
            {
                "systemInstruction": {"parts": [{"text": f"Conversation ID: {conv_id}\nBe concise."}]},
                "contents": [
                    {"role": "user", "parts": [{"text": "Read Cargo.toml"}]},
                    {
                        "role": "model",
                        "parts": [
                            {
                                "functionCall": {
                                    "id": tool_call_id,
                                    "name": "read_file",
                                    "args": {"path": "Cargo.toml"},
                                }
                            }
                        ],
                    },
                    {
                        "role": "model",
                        "parts": [
                            {
                                "functionResponse": {
                                    "id": tool_call_id,
                                    "name": "read_file",
                                    "response": {"content": "[workspace]\n"},
                                }
                            }
                        ],
                    },
                ],
                "generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}},
            }
        ).encode("utf-8")
        followup_resp = http_raw_request(
            "127.0.0.1",
            proxy_port,
            "POST",
            "/v1beta/models/gpt-6.2-sol:streamGenerateContent?alt=sse",
            headers={"Content-Type": "application/json"},
            body=followup_body,
            timeout=10.0,
        )
        self.assertEqual(followup_resp.status, 200, f"Codex follow-up stream failed: {followup_resp.text}")
        self.assertIn("Hello from Codex!", followup_resp.text)

        codex_reqs_after = [r for r in mock.requests if r.path.startswith("/responses")]
        self.assertGreaterEqual(len(codex_reqs_after), 3)
        followup_codex_json = json.loads(codex_reqs_after[-1].body.decode("utf-8"))
        followup_input = followup_codex_json["input"]
        self.assertEqual(
            [item.get("type") for item in followup_input],
            ["message", "message", "function_call", "function_call_output"],
            f"Expected developer + user + function_call + function_call_output, got: {followup_input}",
        )
        self.assertEqual(followup_input[2]["call_id"], tool_call_id)
        self.assertEqual(followup_input[2]["name"], "read_file")
        self.assertEqual(followup_input[3]["call_id"], tool_call_id)
        self.assertIn("[workspace]", followup_input[3]["output"])

        # Verify SQLite usage & per-session /api/stats & Sidecar /_api/get_usage record provider="openai-codex".
        _files, events = poll_usage_events(env.usage_dir, min_count=2, timeout=5.0, interval=0.02)
        codex_events = [e for e in events if e["provider"] == "openai-codex"]
        self.assertEqual(len(codex_events), 2, f"Expected 2 openai-codex usage events, got {events}")
        self.assertEqual(codex_events[0]["source"], "proxy")
        self.assertEqual(codex_events[0]["model"], "gpt-6.2-sol")
        self.assertEqual(int(codex_events[0]["input_tokens"]), 1500)
        self.assertEqual(int(codex_events[0]["cached_tokens"]), 900)
        self.assertEqual(int(codex_events[0]["output_tokens"]), 240)
        self.assertEqual(int(codex_events[0]["reasoning_tokens"]), 60)

        stats_resp = http_raw_request(
            "127.0.0.1",
            proxy_port,
            "GET",
            f"/api/stats?conversation_id={conv_id}",
            timeout=5.0,
        )
        self.assertEqual(stats_resp.status, 200)
        stats_json = stats_resp.json()
        self.assertEqual(stats_json["usage"]["requests"], 2)
        self.assertEqual(stats_json["by_model"][0]["provider"], "openai-codex")
        self.assertEqual(stats_json["by_model"][0]["model"], "gpt-6.2-sol")

        usage_7d = self._sidecar_post(sidecar_port, token, "get_usage", {"range": "7d"})
        self.assertEqual(usage_7d["totals"]["requests"], 2)
        self.assertEqual(usage_7d["by_provider"][0]["provider"], "openai-codex")

        # 6. Disconnect via Sidecar API clears auth/codex.json and removes Codex slots on hot-reload.
        snap_disc = self._sidecar_post(sidecar_port, token, "disconnect_codex")
        self.assertFalse(snap_disc["config"]["providers"]["codex"]["authenticated"])
        self.assertFalse(codex_auth_path.exists(), "auth/codex.json must be deleted on disconnect")

        cat2 = self._fetch_catalog(proxy_port, "codex-e2e-2")
        self.assertFalse(any("gpt-6" in k for k in cat2.get("models", {})))
