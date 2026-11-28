"""Opt-in upgrade reminder & install-channel opaque-box CUJs (`--suite cuj`).

Exercises real CLI + Sidecar Web UI update lifecycle contracts strictly over
loopback (zero live internet calls):
1. `install --channel npx|uvx` persistence + `doctor --json` offline `update_status`.
2. Sidecar `/_api/check_update`, `/_api/get_update_status`, and `/_api/get_settings`
   resolving newer versions from loopback mock npm and PyPI registries.
"""

from __future__ import annotations

import json
from typing import Any

from harness import E2ETestCase, run_cli
from mock_gemini import CapturedRequest, JsonAction


class TestCujUpgrade(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "UPGRADE"

    def test_cuj_install_channel_persistence_and_doctor_offline_update_status(self) -> None:
        """`install --channel npx|uvx` persists channel and `doctor --json` reports offline `update_status`."""
        env = self.make_env()
        env.write_config(env.make_config_dict())
        binary = self.require_binary()
        proc_env = env.build_env()

        for channel in ("npx", "uvx"):
            install_res = run_cli(
                [str(binary), "install", "--no-launcher-hook", "--channel", channel],
                env=proc_env,
            )
            self.assertEqual(
                install_res.returncode,
                0,
                f"install --channel {channel} failed: {install_res.stderr}",
            )

            doctor_res = run_cli(
                [str(binary), "doctor", "--json", "--config", str(env.config_path)],
                env=proc_env,
            )
            self.assertIn(
                doctor_res.returncode,
                (0, 1),
                f"doctor --json unexpected exit {doctor_res.returncode}: {doctor_res.stderr}",
            )
            payload: dict[str, Any] = json.loads(doctor_res.stdout)
            checks = payload.get("checks")
            self.assertIsInstance(checks, list, f"doctor --json missing checks: {payload}")
            update_check = next(
                (c for c in checks if isinstance(c, dict) and c.get("name") == "update_status"),
                None,
            )
            self.assertIsNotNone(
                update_check,
                f"doctor --json missing update_status check: {checks}",
            )
            assert update_check is not None
            self.assertEqual(update_check.get("critical"), False)
            self.assertIn(f"channel: {channel}", str(update_check.get("message", "")))

    def test_cuj_sidecar_update_check_and_status_from_loopback_registries(self) -> None:
        """Sidecar `/_api/check_update`, `/_api/get_update_status`, and `/_api/get_settings` reflect loopback mock npm/PyPI registries."""
        mock = self.start_mock_gemini()

        def _registry_handler(req: CapturedRequest) -> JsonAction:
            if req.path == "/npm/latest":
                return JsonAction(200, {"version": "99.0.0"})
            if req.path == "/pypi/json":
                return JsonAction(200, {"info": {"version": "99.1.0"}})
            return JsonAction(404, {"error": f"unexpected path {req.path}"})

        mock.set_handler(_registry_handler)
        binary = self.require_binary()

        cases = (
            ("npx", "99.0.0", "npx -y agy-companion@latest install"),
            ("uvx", "99.1.0", "uvx --refresh agy-companion install"),
        )
        for channel, expected_latest, expected_cmd in cases:
            env = self.make_env()
            env.write_config(env.make_config_dict())
            install_res = run_cli(
                [str(binary), "install", "--no-launcher-hook", "--channel", channel],
                env=env.build_env(),
            )
            self.assertEqual(
                install_res.returncode,
                0,
                f"install --channel {channel} failed: {install_res.stderr}",
            )

            extra_env: dict[str, str | None] = {
                "AGY_COMPANION_NO_UPDATE_CHECK": "0",
                "CI": "0",
                "AGY_COMPANION_TEST_NPM_REGISTRY_URL": f"{mock.base_url}/npm/latest",
                "AGY_COMPANION_TEST_PYPI_REGISTRY_URL": f"{mock.base_url}/pypi/json",
            }
            sidecar = self.start_sidecar_ui(env, extra_env=extra_env)

            checked = sidecar.call_tool("check_update", {})["structuredContent"]
            self.assertEqual(checked["state"], "available", f"unexpected check_update: {checked}")
            self.assertEqual(checked["channel"], channel)
            self.assertEqual(checked["latest_version"], expected_latest)
            self.assertEqual(checked["fallback_command"], expected_cmd)
            self.assertIsNotNone(checked.get("checked_at_ms"))

            status = sidecar.call_tool("get_update_status", {})["structuredContent"]
            self.assertEqual(status["state"], "available")
            self.assertEqual(status["channel"], channel)
            self.assertEqual(status["latest_version"], expected_latest)

            settings = sidecar.call_tool("get_settings", {})["structuredContent"]
            snap_update = settings.get("update")
            self.assertIsInstance(snap_update, dict, f"get_settings missing update: {settings}")
            self.assertEqual(snap_update["state"], "available")
            self.assertEqual(snap_update["channel"], channel)
            self.assertEqual(snap_update["latest_version"], expected_latest)
            sidecar.close()
