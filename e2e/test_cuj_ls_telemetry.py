"""Opaque-box CUJ E2E tests for per-window Antigravity LS telemetry (`--suite cuj`).

Exercises real multi-process behavior across `serve` (proxy + Sidecar Web UI + LsCollector),
`MockLsServer` (Connect HTTP/1.1 streaming + unary JSON), `MockGeminiServer`, and `usage.db` v2:
1. Multi-frame initial snapshot zero-cold-backfill + live Vertex Claude + BYOM proxy
   `responseId` deduplication into 1 reconciled SQLite row with exact token normalization.
2. Subagent rollup into focused root Monitor + `CHECKPOINT` compaction cold-start fold +
   `SIDE_QUESTION` immunity + `RevertToStep` reset without duplicating pre-revert rows.
3. Proxy restart/takeover Monitor survival + standalone CLI fail-open without LS env vars.
"""

from __future__ import annotations

import datetime
import sqlite3
import sys
import time
from typing import Any

from harness import (
    E2ETestCase,
    find_free_port,
    http_raw_request,
    open_usage_db_readonly,
    usage_files,
)
from mock_gemini import SseStreamAction, build_sse_events
from mock_ls import MockLsServer


def _utc_now_rfc3339(offset_secs: int = 0) -> str:
    dt = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=offset_secs)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def _sample_ls_model_configs() -> list[dict[str, Any]]:
    return [
        {
            "label": "Gemini 4 Hydrogen (Medium)",
            "modelOrAlias": {"model": "MODEL_PLACEHOLDER_M335"},
            "provider": "MODEL_PROVIDER_GOOGLE",
            "modelUrl": "gemini-api://gemini-4-hydrogen",
            "modelId": "companion/gemini-4-hydrogen-medium",
        },
        {
            "label": "Claude Opus 5.5 (High)",
            "modelOrAlias": {"model": "MODEL_PLACEHOLDER_M501"},
            "provider": "MODEL_PROVIDER_ANTHROPIC",
            "modelId": "claude-opus-5-5-high",
        },
        {
            "label": "Gemini 4 Neon (High)",
            "modelOrAlias": {"model": "MODEL_PLACEHOLDER_M467"},
            "provider": "MODEL_PROVIDER_GOOGLE",
            "modelId": "gemini-4-neon-high",
        },
    ]


def _query_usage_rows_v2(usage_dir: Any) -> list[dict[str, Any]]:
    dbs = usage_files(usage_dir)
    if not dbs:
        return []
    rows: list[dict[str, Any]] = []
    for db_path in dbs:
        conn: sqlite3.Connection | None = None
        try:
            conn = open_usage_db_readonly(db_path)
            conn.row_factory = sqlite3.Row
            cur = conn.execute(
                "SELECT timestamp_utc, request_id, provider, model, input_tokens, output_tokens, "
                "cached_tokens, reasoning_tokens, latency_ms, source, response_id, conversation_id, "
                "root_conversation_id, parent_conversation_id, cache_write_tokens, ttft_ms, "
                "model_display_name, telemetry_source, cascade_step_index "
                "FROM usage_events ORDER BY id ASC"
            )
            rows.extend(dict(row) for row in cur.fetchall())
        except (sqlite3.OperationalError, sqlite3.DatabaseError, OSError):
            return []
        finally:
            if conn is not None:
                conn.close()
    return rows


def _poll_usage_rows_v2(
    usage_dir: Any,
    predicate: Any,
    *,
    timeout: float = 6.0,
    interval: float = 0.05,
) -> list[dict[str, Any]]:
    deadline = time.monotonic() + timeout
    last: list[dict[str, Any]] = []
    while True:
        last = _query_usage_rows_v2(usage_dir)
        if predicate(last):
            return last
        if time.monotonic() >= deadline:
            return last
        time.sleep(interval)


class TestCujLsTelemetry(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "LS_TELEMETRY"

    def _start_mock_ls(self, **kwargs: Any) -> MockLsServer:
        return self.stack.enter_context(MockLsServer(**kwargs))

    def test_cuj_ls_multi_frame_boot_zero_backfill_and_vertex_byom_dedup(self) -> None:
        """Multi-frame initial snapshot triggers zero cold backfill; live Vertex Claude + BYOM proxy reconcile cleanly in SQLite v2."""
        env = self.make_env()
        proxy_port = find_free_port()
        mock_gemini = self.start_mock_gemini(expected_api_key="test-gemini-key-ls-001")
        mock_gemini.enqueue(
            SseStreamAction(
                chunks=[
                    build_sse_events(
                        [
                            {
                                "responseId": "resp-byom-dedup-001",
                                "candidates": [{"content": {"parts": [{"text": "hello byom"}]}}],
                                "usageMetadata": {
                                    "promptTokenCount": 35_129,
                                    "cachedContentTokenCount": 32_952,
                                    "candidatesTokenCount": 283,
                                    "thoughtsTokenCount": 52,
                                },
                            }
                        ]
                    )
                ]
            )
        )

        env.write_config(
            env.make_config_dict(
                listen_port=proxy_port,
                base_url=mock_gemini.base_url,
                default_model="gemini-4-hydrogen",
            )
        )
        env.write_gemini_auth("test-gemini-key-ls-001")

        # Seed 3 separate initial snapshot frames of historical cascades (simulating live 12x100 framing).
        historical_frames = [
            {
                "updates": {
                    f"hist-old-{idx:04d}": {
                        "trajectoryId": f"traj-hist-{idx}",
                        "stepCount": 42,
                        "createdTime": "2026-01-01T00:00:00Z",
                        "lastModifiedTime": "2026-01-01T00:10:00Z",
                    }
                }
            }
            for idx in range(3)
        ]
        csrf_secret = "e2e-secret-csrf-ls-telemetry-9988"
        mock_ls = self._start_mock_ls(
            csrf_token=csrf_secret,
            model_configs=_sample_ls_model_configs(),
            initial_snapshot_frames=historical_frames,
            initial_frame_delay_s=0.015,
        )

        sidecar = self.start_sidecar_ui(
            env,
            extra_env={
                "ANTIGRAVITY_LS_ADDRESS": mock_ls.address,
                "ANTIGRAVITY_CSRF_TOKEN": csrf_secret,
            },
        )
        self.assertTrue(
            mock_ls.wait_for_stream_connected(timeout=5.0),
            "sidecar LsCollector must connect to JetboxSubscribeToSummaries",
        )
        # Allow the 300ms idle settle window to elapse after the 3 initial snapshot frames.
        time.sleep(0.50)

        # Assert ZERO cold-backfill GetCascadeTrajectorySteps calls for historical cascades.
        boot_step_rpcs = mock_ls.captured_rpcs("GetCascadeTrajectorySteps")
        self.assertEqual(
            boot_step_rpcs,
            [],
            f"historical boot snapshot frames must trigger 0 Steps calls, got: {boot_step_rpcs}",
        )

        # Wait for the sidecar's proxy listener to finish binding before issuing a BYOM request.
        health_deadline = time.monotonic() + 5.0
        while time.monotonic() < health_deadline:
            try:
                h_resp = http_raw_request("127.0.0.1", proxy_port, "GET", "/health", timeout=0.5)
                if h_resp.status == 200:
                    break
            except OSError:
                pass
            time.sleep(0.02)

        # 1. Send a real BYOM proxy request so proxy writes row with response_id = "resp-byom-dedup-001".
        proxy_resp = http_raw_request(
            "127.0.0.1",
            proxy_port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}',
        )
        self.assertEqual(proxy_resp.status, 200, f"proxy BYOM failed: {proxy_resp.text}")
        proxy_rows = _poll_usage_rows_v2(
            env.usage_dir,
            lambda rows: any(r["response_id"] == "resp-byom-dedup-001" for r in rows),
        )
        self.assertEqual(len(proxy_rows), 1)
        self.assertEqual(proxy_rows[0]["telemetry_source"], "proxy")

        # 2. Push live LS cascade with BOTH a built-in Vertex Claude step and the same BYOM Gemini step.
        live_cid = "live-conv-a1262862"
        t0 = _utc_now_rfc3339(-2)
        t1 = _utc_now_rfc3339(-1)
        t2 = _utc_now_rfc3339(0)
        mock_ls.set_steps(
            live_cid,
            [
                {
                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                    "status": "CORTEX_STEP_STATUS_DONE",
                    "metadata": {
                        "createdAt": t0,
                        "viewableAt": t1,
                        "finishedGeneratingAt": t1,
                        "completedAt": t1,
                        "generatorModel": "MODEL_PLACEHOLDER_M501",
                        "modelUsage": {
                            "model": "MODEL_PLACEHOLDER_M501",
                            "inputTokens": "1000",
                            "cacheReadTokens": "4000",
                            "outputTokens": "250",
                            "responseOutputTokens": "200",
                            "thinkingOutputTokens": "50",
                            "apiProvider": "API_PROVIDER_ANTHROPIC_VERTEX",
                            "responseId": "resp-vertex-claude-001",
                            "responseHeader": {"sessionID": "builtin-vertex-sess"},
                        },
                    },
                },
                {
                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                    "status": "CORTEX_STEP_STATUS_DONE",
                    "metadata": {
                        "createdAt": t1,
                        "viewableAt": t2,
                        "finishedGeneratingAt": t2,
                        "completedAt": t2,
                        "generatorModel": "MODEL_PLACEHOLDER_M335",
                        "modelUsage": {
                            "model": "MODEL_PLACEHOLDER_M335",
                            "inputTokens": "2177",
                            "cacheReadTokens": "32952",
                            "outputTokens": "335",
                            "responseOutputTokens": "283",
                            "thinkingOutputTokens": "52",
                            "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                            "responseId": "resp-byom-dedup-001",
                            "responseHeader": {"responseId": "resp-byom-dedup-001"},
                        },
                    },
                },
            ],
        )
        mock_ls.set_generator_metadata(
            live_cid,
            [
                {
                    "chatModel": {
                        "chatStartMetadata": {
                            "contextWindowMetadata": {"maxContextTokens": "512000"}
                        }
                    }
                }
            ],
        )
        mock_ls.push_summary_delta(
            {
                live_cid: {
                    "trajectoryId": "traj-live-1",
                    "stepCount": 2,
                    "createdTime": t0,
                    "lastModifiedTime": t2,
                }
            }
        )

        # Wait for SQLite v2 to hold both rows with the BYOM row reconciled (2 rows total, never 3!).
        rows = _poll_usage_rows_v2(
            env.usage_dir,
            lambda rs: (
                len(rs) == 2
                and any(
                    r["response_id"] == "resp-byom-dedup-001"
                    and r["telemetry_source"] == "reconciled"
                    for r in rs
                )
                and any(
                    r["response_id"] == "resp-vertex-claude-001"
                    and r["telemetry_source"] == "ls"
                    for r in rs
                )
            ),
        )
        self.assertEqual(len(rows), 2, f"expected 2 deduped rows in usage.db, got: {rows}")

        by_resp = {r["response_id"]: r for r in rows}
        byom_row = by_resp["resp-byom-dedup-001"]
        self.assertEqual(byom_row["telemetry_source"], "reconciled")
        self.assertEqual(byom_row["provider"], "gemini")
        self.assertEqual(byom_row["source"], "proxy")
        self.assertEqual(byom_row["model"], "gemini-4-hydrogen")
        self.assertEqual(byom_row["conversation_id"], live_cid)
        self.assertEqual(byom_row["input_tokens"], 35_129)
        self.assertEqual(byom_row["cached_tokens"], 32_952)
        self.assertEqual(byom_row["output_tokens"], 283)
        self.assertEqual(byom_row["reasoning_tokens"], 52)

        claude_row = by_resp["resp-vertex-claude-001"]
        self.assertEqual(claude_row["telemetry_source"], "ls")
        self.assertEqual(claude_row["provider"], "vertex_claude")
        self.assertEqual(claude_row["source"], "builtin")
        self.assertEqual(claude_row["model"], "claude-opus-5-5")
        self.assertEqual(claude_row["input_tokens"], 5_000)
        self.assertEqual(claude_row["cached_tokens"], 4_000)
        self.assertEqual(claude_row["output_tokens"], 200)
        self.assertEqual(claude_row["reasoning_tokens"], 50)

        # Verify Sidecar UI `/_api/get_settings` reflects ls_telemetry_active == True & both calls.
        snap = sidecar.call_tool("get_settings", {"conversation_id": live_cid})["structuredContent"]
        self.assertTrue(snap.get("ls_telemetry_active"), f"expected ls_telemetry_active=true: {snap}")
        stats = snap.get("stats") or {}
        self.assertEqual(stats.get("usage", {}).get("requests"), 2)
        self.assertEqual(stats.get("max_context_tokens"), 512_000)
        models_seen = {c["model"] for c in stats.get("recent_calls", [])}
        self.assertEqual(models_seen, {"claude-opus-5-5", "gemini-4-hydrogen"})

        gen_rpcs = mock_ls.captured_rpcs("GetCascadeTrajectoryGeneratorMetadata")
        self.assertTrue(gen_rpcs, "expected GetCascadeTrajectoryGeneratorMetadata RPC")
        self.assertEqual({r.body.get("cascadeId") for r in gen_rpcs}, {live_cid})
        self.assertTrue(
            all(r.body.get("includeMessages") is False for r in gen_rpcs),
            f"expected includeMessages=False on all generator metadata RPCs: {gen_rpcs}",
        )

        # Verify historical cascades were STILL never fetched.
        fetched_cids = {
            r.body.get("cascadeId") for r in mock_ls.captured_rpcs("GetCascadeTrajectorySteps")
        }
        self.assertEqual(fetched_cids, {live_cid})

        # S2 hygiene: CSRF token never leaks into stderr.
        self.assertNotIn(csrf_secret, sidecar.stderr_text)

    def test_cuj_ls_subagent_rollup_checkpoint_side_question_and_revert(self) -> None:
        """Root Monitor rolls up subagents, classifies post-CHECKPOINT as cold_start, ignores SIDE_QUESTION, and resets cleanly on RevertToStep."""
        env = self.make_env()
        proxy_port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=proxy_port))

        csrf_secret = "e2e-secret-csrf-ls-subagent-7766"
        mock_ls = self._start_mock_ls(
            csrf_token=csrf_secret,
            model_configs=_sample_ls_model_configs(),
            initial_snapshot_frames=[{"updates": {}}],
        )
        sidecar = self.start_sidecar_ui(
            env,
            extra_env={
                "ANTIGRAVITY_LS_ADDRESS": mock_ls.address,
                "ANTIGRAVITY_CSRF_TOKEN": csrf_secret,
            },
        )
        self.assertTrue(mock_ls.wait_for_stream_connected(timeout=5.0))
        time.sleep(0.45)

        root_cid = "root-conv-100001"
        sub_cid = "sub-conv-100002"
        sideq_cid = "sideq-conv-100003"
        t0 = _utc_now_rfc3339(-4)
        t1 = _utc_now_rfc3339(-3)
        t2 = _utc_now_rfc3339(-2)
        t3 = _utc_now_rfc3339(-1)
        t4 = _utc_now_rfc3339(0)

        root_step0 = {
            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
            "status": "CORTEX_STEP_STATUS_DONE",
            "metadata": {
                "createdAt": t0,
                "finishedGeneratingAt": t1,
                "generatorModel": "MODEL_PLACEHOLDER_M467",
                "modelUsage": {
                    "inputTokens": "1000",
                    "cacheReadTokens": "0",
                    "outputTokens": "100",
                    "responseOutputTokens": "100",
                    "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
                    "responseId": "resp-root-step0",
                    "responseHeader": {"sessionID": "sess-root"},
                },
            },
        }
        checkpoint_step1 = {
            "type": "CORTEX_STEP_TYPE_CHECKPOINT",
            "status": "CORTEX_STEP_STATUS_DONE",
            "metadata": {
                "createdAt": t1,
                "finishedGeneratingAt": t2,
                "generatorModel": "MODEL_PLACEHOLDER_M467",
                "modelUsage": {
                    "inputTokens": "400",
                    "cacheReadTokens": "0",
                    "outputTokens": "50",
                    "responseOutputTokens": "50",
                    "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
                    "responseId": "resp-root-cp1",
                    "responseHeader": {"sessionID": "sess-root"},
                },
            },
        }
        post_cp_step2 = {
            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
            "status": "CORTEX_STEP_STATUS_DONE",
            "metadata": {
                "createdAt": t2,
                "finishedGeneratingAt": t3,
                "generatorModel": "MODEL_PLACEHOLDER_M467",
                "modelUsage": {
                    "inputTokens": "900",
                    "cacheReadTokens": "0",
                    "outputTokens": "80",
                    "responseOutputTokens": "80",
                    "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
                    "responseId": "resp-root-step2",
                    "responseHeader": {"sessionID": "sess-root"},
                },
            },
        }
        sub_step0 = {
            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
            "status": "CORTEX_STEP_STATUS_DONE",
            "metadata": {
                "createdAt": t3,
                "finishedGeneratingAt": t4,
                "generatorModel": "MODEL_PLACEHOLDER_M467",
                "modelUsage": {
                    "inputTokens": "700",
                    "cacheReadTokens": "0",
                    "outputTokens": "60",
                    "responseOutputTokens": "60",
                    "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
                    "responseId": "resp-sub-step0",
                    "responseHeader": {"sessionID": "sess-sub"},
                },
            },
        }

        mock_ls.set_steps(root_cid, [root_step0, checkpoint_step1, post_cp_step2])
        mock_ls.set_steps(sub_cid, [sub_step0])
        mock_ls.set_steps(sideq_cid, [root_step0])
        mock_ls.set_generator_metadata(
            root_cid,
            [
                {
                    "chatModel": {
                        "chatStartMetadata": {
                            "contextWindowMetadata": {"maxContextTokens": "512000"}
                        }
                    }
                }
            ],
        )
        mock_ls.set_generator_metadata(
            sub_cid,
            [
                {
                    "chatModel": {
                        "chatStartMetadata": {
                            "contextWindowMetadata": {"maxContextTokens": "200000"}
                        }
                    }
                }
            ],
        )

        mock_ls.push_summary_delta(
            {
                root_cid: {
                    "trajectoryId": "traj-root-v1",
                    "stepCount": 3,
                    "createdTime": t0,
                    "lastModifiedTime": t3,
                },
                sub_cid: {
                    "trajectoryId": "traj-sub-v1",
                    "stepCount": 1,
                    "createdTime": t3,
                    "lastModifiedTime": t4,
                    "trajectoryMetadata": {
                        "parentConversationId": root_cid,
                        "rootConversationId": root_cid,
                        "nestingDepth": 1,
                    },
                },
                sideq_cid: {
                    "trajectoryId": "traj-sideq-v1",
                    "trajectoryType": "CORTEX_TRAJECTORY_TYPE_SIDE_QUESTION",
                    "stepCount": 1,
                    "createdTime": t3,
                    "lastModifiedTime": t4,
                },
            }
        )

        # Wait for all 4 rows (3 root + 1 subagent) in usage.db.
        rows = _poll_usage_rows_v2(env.usage_dir, lambda rs: len(rs) >= 4)
        self.assertEqual(len(rows), 4, f"expected 4 rows (no SIDE_QUESTION dup), got: {rows}")

        # Verify SIDE_QUESTION was never fetched.
        fetched_cids = {
            r.body.get("cascadeId") for r in mock_ls.captured_rpcs("GetCascadeTrajectorySteps")
        }
        self.assertNotIn(sideq_cid, fetched_cids)

        # Focused root Monitor rolls up root + subagent, with ZERO cache_break!
        snap = sidecar.call_tool("get_settings", {"conversation_id": root_cid})["structuredContent"]
        self.assertTrue(snap.get("ls_telemetry_active"))
        stats = snap["stats"]
        self.assertEqual(stats["usage"]["requests"], 4)
        self.assertEqual(stats["max_context_tokens"], 512_000)
        gen_cids = {
            r.body.get("cascadeId")
            for r in mock_ls.captured_rpcs("GetCascadeTrajectoryGeneratorMetadata")
        }
        self.assertEqual(gen_cids, {root_cid})
        recent = stats["recent_calls"]
        self.assertEqual(len(recent), 4)
        self.assertTrue(
            all(not call["cache_break"] for call in recent),
            f"post-CHECKPOINT and subagent turns must not trigger cache_break: {recent}",
        )

        # Now trigger RevertToStep on root (new trajectoryId, shrunk stepCount 3 -> 2, new post-revert turn).
        t5 = _utc_now_rfc3339(1)
        post_revert_step1 = {
            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
            "status": "CORTEX_STEP_STATUS_DONE",
            "metadata": {
                "createdAt": t4,
                "finishedGeneratingAt": t5,
                "generatorModel": "MODEL_PLACEHOLDER_M467",
                "modelUsage": {
                    "inputTokens": "1100",
                    "cacheReadTokens": "500",
                    "outputTokens": "95",
                    "responseOutputTokens": "95",
                    "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
                    "responseId": "resp-root-post-revert",
                    "responseHeader": {"sessionID": "sess-root-v2"},
                },
            },
        }
        mock_ls.set_steps(root_cid, [root_step0, post_revert_step1])
        mock_ls.set_generator_metadata(
            root_cid,
            [
                {
                    "chatModel": {
                        "chatStartMetadata": {
                            "contextWindowMetadata": {"maxContextTokens": "256000"}
                        }
                    }
                }
            ],
        )
        mock_ls.push_summary_delta(
            {
                root_cid: {
                    "trajectoryId": "traj-root-v2",
                    "stepCount": 2,
                    "createdTime": t0,
                    "lastModifiedTime": t5,
                }
            }
        )

        # Wait for the new post-revert row in usage.db (5 rows total; root_step0 NOT duplicated!).
        rows_after = _poll_usage_rows_v2(
            env.usage_dir,
            lambda rs: any(r["response_id"] == "resp-root-post-revert" for r in rs),
            timeout=6.0,
        )
        self.assertEqual(
            len(rows_after),
            5,
            f"RevertToStep must add only the new post-revert row without duplicating step 0: {rows_after}",
        )

        # Monitor snapshot for root now shows the reverted timeline (2 root calls + 1 subagent = 3).
        snap_after = sidecar.call_tool("get_settings", {"conversation_id": root_cid})[
            "structuredContent"
        ]
        self.assertEqual(snap_after["stats"]["usage"]["requests"], 3)
        self.assertEqual(snap_after["stats"]["max_context_tokens"], 256_000)

    def test_cuj_ls_survives_proxy_restart_and_cli_fails_open_without_ls(self) -> None:
        """LS Monitor survives proxy restart/takeover with zero loss, and standalone sidecar fails open without LS env vars."""
        env = self.make_env()
        proxy_port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=proxy_port))

        # Start primary proxy daemon owning proxy_port (no LS env vars, like detached login-shell daemon).
        primary_proxy = self.start_proxy(env, port=proxy_port)

        csrf_secret = "e2e-secret-csrf-ls-takeover-5544"
        mock_ls = self._start_mock_ls(
            csrf_token=csrf_secret,
            model_configs=_sample_ls_model_configs(),
            initial_snapshot_frames=[{"updates": {}}],
        )
        sidecar = self.start_sidecar_ui(
            env,
            extra_env={
                "ANTIGRAVITY_LS_ADDRESS": mock_ls.address,
                "ANTIGRAVITY_CSRF_TOKEN": csrf_secret,
            },
        )
        self.assertTrue(mock_ls.wait_for_stream_connected(timeout=5.0))
        time.sleep(0.45)

        conv_id = "takeover-conv-887766"
        t0 = _utc_now_rfc3339(-1)
        t1 = _utc_now_rfc3339(0)
        mock_ls.set_steps(
            conv_id,
            [
                {
                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                    "status": "CORTEX_STEP_STATUS_DONE",
                    "metadata": {
                        "createdAt": t0,
                        "finishedGeneratingAt": t1,
                        "generatorModel": "MODEL_PLACEHOLDER_M501",
                        "modelUsage": {
                            "inputTokens": "2000",
                            "cacheReadTokens": "1000",
                            "outputTokens": "150",
                            "responseOutputTokens": "150",
                            "apiProvider": "API_PROVIDER_ANTHROPIC_VERTEX",
                            "responseId": "resp-takeover-001",
                            "responseHeader": {"sessionID": "sess-takeover"},
                        },
                    },
                }
            ],
        )
        mock_ls.push_summary_delta(
            {
                conv_id: {
                    "trajectoryId": "traj-takeover-1",
                    "stepCount": 1,
                    "createdTime": t0,
                    "lastModifiedTime": t1,
                }
            }
        )
        _poll_usage_rows_v2(
            env.usage_dir,
            lambda rs: any(r["response_id"] == "resp-takeover-001" for r in rs),
        )

        before = sidecar.call_tool("get_settings", {"conversation_id": conv_id})[
            "structuredContent"
        ]
        self.assertTrue(before["ls_telemetry_active"])
        self.assertEqual(before["stats"]["usage"]["requests"], 1)
        self.assertIsNone(before["stats"]["max_context_tokens"])
        if sys.platform in ("darwin", "linux"):
            self.assertIsNotNone(before["stats"]["proxy_rss_bytes"])

        # Stop primary proxy; sidecar remains alive & Monitor preserves LS telemetry during/after takeover.
        primary_proxy.stop()
        during_or_after = sidecar.call_tool("get_settings", {"conversation_id": conv_id})[
            "structuredContent"
        ]
        self.assertTrue(during_or_after["ls_telemetry_active"])
        self.assertEqual(during_or_after["stats"]["usage"]["requests"], 1)

        # Standalone sidecar without LS env vars fails open cleanly (ls_telemetry_active == False).
        env_no_ls = self.make_env()
        no_ls_port = find_free_port()
        env_no_ls.write_config(env_no_ls.make_config_dict(listen_port=no_ls_port))
        sidecar_no_ls = self.start_sidecar_ui(env_no_ls)
        snap_no_ls = sidecar_no_ls.call_tool("get_settings", {"conversation_id": conv_id})[
            "structuredContent"
        ]
        self.assertFalse(snap_no_ls["ls_telemetry_active"])

    def test_cuj_ls_resolves_placeholder_and_codex_provider(self) -> None:
        """LS telemetry maps uncatalogued M998 via GeneratorMetadata, M530 via static fallback, and companion/gpt-* to openai-codex."""
        env = self.make_env()
        proxy_port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=proxy_port))

        csrf_secret = "e2e-secret-csrf-ls-placeholder-codex-1122"
        configs = list(_sample_ls_model_configs()) + [
            {
                "label": "GPT-6.2 Sol (Codex) (Medium)",
                "modelOrAlias": {"model": "MODEL_PLACEHOLDER_M356"},
                "provider": "MODEL_PROVIDER_GOOGLE",
                "modelUrl": "gemini-api://gpt-6.2-sol",
                "modelId": "companion/gpt-6.2-sol-medium",
            }
        ]
        mock_ls = self._start_mock_ls(
            csrf_token=csrf_secret,
            model_configs=configs,
            initial_snapshot_frames=[{"updates": {}}],
        )
        sidecar = self.start_sidecar_ui(
            env,
            extra_env={
                "ANTIGRAVITY_LS_ADDRESS": mock_ls.address,
                "ANTIGRAVITY_CSRF_TOKEN": csrf_secret,
            },
        )
        self.assertTrue(mock_ls.wait_for_stream_connected(timeout=5.0))
        time.sleep(0.45)

        conv_id = "conv-placeholder-codex-99"
        t0 = _utc_now_rfc3339(-3)
        t1 = _utc_now_rfc3339(-2)
        t2 = _utc_now_rfc3339(-1)
        t3 = _utc_now_rfc3339(0)
        mock_ls.set_steps(
            conv_id,
            [
                {
                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                    "status": "CORTEX_STEP_STATUS_DONE",
                    "metadata": {
                        "createdAt": t0,
                        "finishedGeneratingAt": t1,
                        "generatorModel": "MODEL_PLACEHOLDER_M356",
                        "modelUsage": {
                            "model": "MODEL_PLACEHOLDER_M356",
                            "inputTokens": "500",
                            "cacheReadTokens": "200",
                            "outputTokens": "50",
                            "responseOutputTokens": "50",
                            "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                            "responseId": "resp-ls-codex-m356",
                            "responseHeader": {"responseId": "resp-ls-codex-m356"},
                        },
                    },
                },
                {
                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                    "status": "CORTEX_STEP_STATUS_DONE",
                    "metadata": {
                        "createdAt": t1,
                        "finishedGeneratingAt": t2,
                        "generatorModel": "MODEL_PLACEHOLDER_M530",
                        "modelUsage": {
                            "model": "MODEL_PLACEHOLDER_M530",
                            "inputTokens": "1000",
                            "cacheReadTokens": "800",
                            "outputTokens": "80",
                            "responseOutputTokens": "80",
                            "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                            "responseId": "resp-ls-m530",
                            "responseHeader": {"sessionID": "sess-m530"},
                        },
                    },
                },
                {
                    "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                    "status": "CORTEX_STEP_STATUS_DONE",
                    "metadata": {
                        "createdAt": t2,
                        "finishedGeneratingAt": t3,
                        "generatorModel": "MODEL_PLACEHOLDER_M998",
                        "modelUsage": {
                            "model": "MODEL_PLACEHOLDER_M998",
                            "inputTokens": "400",
                            "cacheReadTokens": "100",
                            "outputTokens": "40",
                            "responseOutputTokens": "40",
                            "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                            "responseId": "resp-ls-m998",
                            "responseHeader": {"sessionID": "sess-m998"},
                        },
                    },
                },
            ],
        )
        mock_ls.set_generator_metadata(
            conv_id,
            [
                {
                    "stepIndices": [2],
                    "chatModel": {
                        "model": "MODEL_PLACEHOLDER_M998",
                        "responseModel": "gemini-5-nova-tiered-high",
                        "modelDisplayName": "Gemini 5 Nova (High)",
                        "chatStartMetadata": {
                            "contextWindowMetadata": {"maxContextTokens": "512000"}
                        },
                    },
                }
            ],
        )
        mock_ls.push_summary_delta(
            {
                conv_id: {
                    "trajectoryId": "traj-placeholder-codex-1",
                    "stepCount": 3,
                    "createdTime": t0,
                    "lastModifiedTime": t3,
                }
            }
        )

        rows = _poll_usage_rows_v2(
            env.usage_dir,
            lambda rs: len(rs) == 3,
            timeout=6.0,
        )
        self.assertEqual(len(rows), 3, f"expected 3 rows in usage.db, got: {rows}")
        by_resp = {r["response_id"]: r for r in rows}

        self.assertEqual(by_resp["resp-ls-codex-m356"]["provider"], "openai-codex")
        self.assertEqual(by_resp["resp-ls-codex-m356"]["source"], "proxy")
        self.assertEqual(by_resp["resp-ls-codex-m356"]["model"], "gpt-6.2-sol")

        self.assertEqual(by_resp["resp-ls-m530"]["provider"], "antigravity")
        self.assertEqual(by_resp["resp-ls-m530"]["source"], "builtin")
        self.assertEqual(by_resp["resp-ls-m530"]["model"], "gemini-4-hydrogen")

        self.assertEqual(by_resp["resp-ls-m998"]["provider"], "antigravity")
        self.assertEqual(by_resp["resp-ls-m998"]["source"], "builtin")
        self.assertEqual(by_resp["resp-ls-m998"]["model"], "gemini-5-nova")
        self.assertEqual(by_resp["resp-ls-m998"]["model_display_name"], "Gemini 5 Nova (High)")

        snap = sidecar.call_tool("get_settings", {"conversation_id": conv_id})["structuredContent"]
        by_model = {m["model"]: m for m in snap["stats"]["by_model"]}
        self.assertEqual(by_model["gpt-6.2-sol"]["provider"], "openai-codex")
        self.assertEqual(by_model["gemini-4-hydrogen"]["provider"], "antigravity")
        self.assertEqual(by_model["gemini-5-nova"]["model_display_name"], "Gemini 5 Nova (High)")
