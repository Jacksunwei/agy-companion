#!/usr/bin/env python3
"""Opaque-box 4-tier E2E test runner for agy-companion (pure Python 3 stdlib).

Usage:
  python3 e2e/run_e2e.py --tier 1|2|3|4|all [--binary PATH] [--repo-root PATH]
                         [--self-test] [--list] [--filter SUBSTR] [--json]
                         [--fail-fast] [--timeout SECS] [--verbose]

Exit codes:
  0  All selected tests passed (or --list succeeded)
  1  One or more tests failed or errored
  2  Invalid CLI arguments or missing/non-executable binary for --tier run
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import dataclasses
import importlib
import inspect
import json
import math
import os
import time
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import Any

E2E_DIR = Path(__file__).resolve().parent
DEFAULT_REPO_ROOT = E2E_DIR.parent
if str(E2E_DIR) not in sys.path:
    sys.path.insert(0, str(E2E_DIR))

import harness  # noqa: E402


TIER_MODULES: dict[int, tuple[str, ...]] = {
    1: ("test_tier1_features", "test_tier1_rollout"),
    2: ("test_tier2_boundaries", "test_tier2_rollout"),
    3: ("test_tier3_combinations", "test_tier3_rollout"),
    4: ("test_tier4_scenarios", "test_tier4_rollout"),
}
SELF_TEST_MODULE = "test_self"


@dataclasses.dataclass(frozen=True)
class DiscoveredTest:
    """Metadata describing a single unittest TestCase method."""

    name: str
    tier: int
    feature: str
    class_name: str
    module_name: str
    description: str
    test_cls: type[unittest.TestCase]

    @property
    def tier_label(self) -> str:
        return "SELF" if self.tier == 0 else f"T{self.tier}"

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "tier": self.tier,
            "tier_label": self.tier_label,
            "feature": self.feature,
            "class": self.class_name,
            "module": self.module_name,
            "description": self.description,
        }


def _infer_feature(method_name: str, default_feature: str) -> str:
    if default_feature and default_feature != "GENERIC":
        return default_feature
    parts = method_name.split("_")
    if len(parts) >= 3 and parts[2].startswith("f") and parts[2][1:].isdigit():
        return f"F{parts[2][1:]}"
    if method_name.startswith("test_self_"):
        return "SELF"
    if method_name.startswith("test_t3_"):
        return "COMBINATION"
    if method_name.startswith("test_t4_"):
        return "SCENARIO"
    return default_feature or "UNKNOWN"


def discover_tests(
    tiers: list[int],
    *,
    include_self: bool = False,
) -> list[DiscoveredTest]:
    """Import tier modules and collect all TestCase methods in deterministic order."""
    module_names: list[tuple[int, str]] = []
    if include_self:
        module_names.append((0, SELF_TEST_MODULE))
    for tier in tiers:
        mods: str | Sequence[str] | None = TIER_MODULES.get(tier)
        if not mods:
            continue
        if isinstance(mods, str):
            mod_list: Sequence[str] = (mods,)
        else:
            mod_list = mods
        for mod_name in mod_list:
            module_names.append((tier, mod_name))

    discovered: list[DiscoveredTest] = []
    seen_names: set[str] = set()

    for expected_tier, mod_name in module_names:
        mod = importlib.import_module(mod_name)
        classes: list[type[unittest.TestCase]] = []
        for _, obj in inspect.getmembers(mod, inspect.isclass):
            if (
                issubclass(obj, unittest.TestCase)
                and obj is not unittest.TestCase
                and obj is not harness.E2ETestCase
                and obj.__module__ == mod.__name__
            ):
                classes.append(obj)
        classes.sort(key=lambda c: c.__name__)

        for cls in classes:
            cls_tier = getattr(cls, "TIER", expected_tier)
            cls_feature = getattr(cls, "FEATURE", "")
            method_names = sorted(
                m
                for m in dir(cls)
                if m.startswith("test_") and callable(getattr(cls, m))
            )
            for method_name in method_names:
                fn = getattr(cls, method_name)
                doc = (inspect.getdoc(fn) or "").splitlines()
                desc = doc[0].strip() if doc else ""
                feature = getattr(fn, "_e2e_feature", "") or _infer_feature(method_name, cls_feature)
                if method_name in seen_names:
                    raise ValueError(f"Duplicate E2E test method name detected: {method_name}")
                seen_names.add(method_name)
                discovered.append(
                    DiscoveredTest(
                        name=method_name,
                        tier=cls_tier,
                        feature=feature,
                        class_name=cls.__name__,
                        module_name=mod_name,
                        description=desc,
                        test_cls=cls,
                    )
                )

    discovered.sort(key=lambda t: (t.tier, t.name))
    return discovered


def filter_tests(tests: list[DiscoveredTest], substr: str | None) -> list[DiscoveredTest]:
    if not substr:
        return list(tests)
    needle = substr.lower()
    return [
        t
        for t in tests
        if needle in t.name.lower()
        or needle in t.feature.lower()
        or needle in t.class_name.lower()
        or needle in t.module_name.lower()
        or needle in t.tier_label.lower()
        or needle in t.description.lower()
    ]


def build_counts(tests: list[DiscoveredTest]) -> tuple[dict[str, int], dict[str, dict[str, int]]]:
    by_tier: dict[str, int] = {}
    by_feature: dict[str, dict[str, int]] = {}
    for t in tests:
        by_tier[t.tier_label] = by_tier.get(t.tier_label, 0) + 1
        tier_feat = by_feature.setdefault(t.tier_label, {})
        tier_feat[t.feature] = tier_feat.get(t.feature, 0) + 1
    return by_tier, by_feature


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="run_e2e.py",
        description="Run opaque-box 4-tier E2E tests for agy-companion (pure Python 3 stdlib).",
    )
    parser.add_argument(
        "--tier",
        choices=("1", "2", "3", "4", "all"),
        default=None,
        help="Select E2E tier(s) to run (1, 2, 3, 4, or all).",
    )
    parser.add_argument(
        "--binary",
        type=str,
        default=None,
        help="Path to agy-companion executable (overrides AGY_COMPANION_E2E_BIN and default).",
    )
    parser.add_argument(
        "--repo-root",
        type=str,
        default=str(DEFAULT_REPO_ROOT),
        help="Repository root directory (default: parent of e2e/).",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run pure-Python harness self-tests (no agy-companion binary required).",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="List matching tests without executing them or requiring agy-companion.",
    )
    parser.add_argument(
        "--filter",
        type=str,
        default=None,
        help="Case-insensitive substring filter on test name, feature, class, or tier.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON output for --list or test execution summary.",
    )
    parser.add_argument(
        "-x",
        "--fail-fast",
        action="store_true",
        help="Stop immediately on first failure or error.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=30.0,
        help="Default per-operation timeout in seconds (default: 30.0).",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Verbose test output.",
    )
    args = parser.parse_args(argv)
    if args.tier is None and not args.self_test and not args.list:
        parser.error("Must specify one of --tier {1,2,3,4,all}, --self-test, or --list.")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a finite number > 0.")
    return args


def _selected_tiers(args: argparse.Namespace) -> tuple[list[int], bool]:
    if args.self_test:
        if args.tier is None:
            return [], True
        tiers = [1, 2, 3, 4] if args.tier == "all" else [int(args.tier)]
        return tiers, True
    if args.tier is None:
        # --list without --tier defaults to all 4 tiers.
        return [1, 2, 3, 4], False
    if args.tier == "all":
        return [1, 2, 3, 4], False
    return [int(args.tier)], False


class _RecordingTestResult(unittest.TestResult):
    def __init__(self, *, failfast: bool = False) -> None:
        super().__init__()
        self.failfast = failfast
        self.records: list[dict[str, Any]] = []
        self._start_times: dict[str, float] = {}

    def startTest(self, test: unittest.TestCase) -> None:  # noqa: N802
        super().startTest(test)
        self._start_times[test.id()] = time.monotonic()

    def stopTest(self, test: unittest.TestCase) -> None:  # noqa: N802
        super().stopTest(test)
        self._start_times.pop(test.id(), None)

    def _elapsed(self, test: unittest.TestCase) -> float:
        started = self._start_times.get(test.id(), time.monotonic())
        return round(time.monotonic() - started, 4)

    def addSuccess(self, test: unittest.TestCase) -> None:  # noqa: N802
        super().addSuccess(test)
        self.records.append({"test": test.id(), "status": "pass", "duration_s": self._elapsed(test)})

    def addFailure(self, test: unittest.TestCase, err: Any) -> None:  # noqa: N802
        super().addFailure(test, err)
        self.records.append(
            {
                "test": test.id(),
                "status": "fail",
                "duration_s": self._elapsed(test),
                "detail": self._exc_info_to_string(err, test),
            }
        )

    def addError(self, test: unittest.TestCase, err: Any) -> None:  # noqa: N802
        super().addError(test, err)
        self.records.append(
            {
                "test": test.id(),
                "status": "error",
                "duration_s": self._elapsed(test),
                "detail": self._exc_info_to_string(err, test),
            }
        )

    def addSubTest(self, test: unittest.TestCase, subtest: unittest.TestCase, err: Any) -> None:  # noqa: N802
        super().addSubTest(test, subtest, err)
        if err is not None:
            status = "fail" if issubclass(err[0], test.failureException) else "error"
            self.records.append(
                {
                    "test": subtest.id(),
                    "status": status,
                    "duration_s": self._elapsed(test),
                    "detail": self._exc_info_to_string(err, test),
                }
            )

    def addSkip(self, test: unittest.TestCase, reason: str) -> None:  # noqa: N802
        super().addSkip(test, reason)
        self.records.append(
            {
                "test": test.id(),
                "status": "skip",
                "duration_s": self._elapsed(test),
                "detail": reason,
            }
        )


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    repo_root = Path(args.repo_root).expanduser().resolve()
    tiers, include_self = _selected_tiers(args)

    tests = filter_tests(discover_tests(tiers, include_self=include_self), args.filter)

    if args.list:
        by_tier, by_feature = build_counts(tests)
        if args.json:
            payload = {
                "total": len(tests),
                "by_tier": by_tier,
                "by_feature": by_feature,
                "tests": [t.to_dict() for t in tests],
            }
            print(json.dumps(payload, indent=2))
        else:
            for t in tests:
                print(f"{t.tier_label}\t{t.feature}\t{t.name}")
        return 0

    binary_path: Path | None = None
    if tiers:
        binary_path = harness.resolve_binary(args.binary, repo_root)
        if not (binary_path.is_file() and os.access(binary_path, os.X_OK)):
            default_bin = repo_root / "target" / "debug" / "agy-companion"
            env_bin = os.environ.get("AGY_COMPANION_E2E_BIN", "(unset)")
            print(
                "ERROR: agy-companion binary not found or not executable.\n"
                f"  Resolved path: {binary_path}\n"
                "  Searched locations (in precedence order):\n"
                f"    1. --binary CLI flag:             {args.binary or '(not provided)'}\n"
                f"    2. AGY_COMPANION_E2E_BIN env var: {env_bin}\n"
                f"    3. Default cargo debug binary:    {default_bin}\n"
                "Hint: build with `cargo build --workspace` or pass `--binary <path>`.",
                file=sys.stderr,
            )
            return 2

    harness.set_runtime_config(
        harness.TestRuntimeConfig(
            binary_path=binary_path,
            repo_root=repo_root,
            default_timeout=args.timeout,
            verbose=args.verbose,
        )
    )

    if not tests:
        print("ERROR: No tests matched selection/filter.", file=sys.stderr)
        return 2

    suite = unittest.TestSuite()
    for t in tests:
        suite.addTest(t.test_cls(t.name))

    start_ts = time.monotonic()
    if args.json:
        result = _RecordingTestResult(failfast=args.fail_fast)
        suite.run(result)
        duration = round(time.monotonic() - start_ts, 4)
        ok = result.wasSuccessful()
        summary = {
            "status": "pass" if ok else "fail",
            "total": result.testsRun,
            "passed": sum(1 for r in result.records if r["status"] == "pass"),
            "failed": len(result.failures),
            "errors": len(result.errors),
            "skipped": len(result.skipped),
            "duration_s": duration,
            "results": result.records,
        }
        print(json.dumps(summary, indent=2))
        return 0 if ok else 1

    verbosity = 2 if args.verbose else 1
    runner = unittest.TextTestRunner(
        stream=sys.stdout if args.verbose else sys.stderr,
        verbosity=verbosity,
        failfast=args.fail_fast,
    )
    res = runner.run(suite)
    return 0 if res.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
