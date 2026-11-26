#!/usr/bin/env python3
"""Opaque-box intent-named parallel E2E test runner for agy-companion (pure Python 3 stdlib).

Usage:
  python3 -B e2e/run_e2e.py --suite cuj|boundary|shims|ui-smoke|all
                            [--workers N] [--binary PATH] [--repo-root PATH]
                            [--self-test] [--list] [--filter SUBSTR] [--json]
                            [-x|--fail-fast] [--timeout SECS] [-v|--verbose]

Deprecated alias:
  --tier 1|2|3|4|all  (1/3/4 -> cuj, 2 -> boundary, all -> all)

Exit codes:
  0  All selected tests passed (or --list succeeded / empty optional suite)
  1  One or more tests failed or errored
  2  Invalid CLI arguments, zero filter matches, or missing/non-executable binary
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import concurrent.futures
import dataclasses
import importlib
import inspect
import json
import math
import os
import time
import traceback
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import Any

E2E_DIR = Path(__file__).resolve().parent
DEFAULT_REPO_ROOT = E2E_DIR.parent
if str(E2E_DIR) not in sys.path:
    sys.path.insert(0, str(E2E_DIR))

import harness  # noqa: E402


SUITE_MODULES: dict[str, tuple[str, ...]] = {
    "cuj": ("test_cuj_core", "test_cuj_ux_v2"),
    "boundary": ("test_boundary_security",),
    "shims": ("test_shims_packaging",),
    "ui-smoke": ("test_ui_smoke",),
}
ALL_DEFAULT_SUITES: tuple[str, ...] = ("cuj", "boundary", "shims")
SUITE_ORDER: dict[str, int] = {
    "self": 0,
    "cuj": 1,
    "boundary": 2,
    "shims": 3,
    "ui-smoke": 4,
}
TIER_TO_SUITES: dict[str, tuple[str, ...]] = {
    "1": ("cuj",),
    "2": ("boundary",),
    "3": ("cuj",),
    "4": ("cuj",),
    "all": ALL_DEFAULT_SUITES,
}
SELF_TEST_MODULE = "test_self"
DEFAULT_WORKERS = min(4, os.cpu_count() or 1)


@dataclasses.dataclass(frozen=True)
class DiscoveredTest:
    """Metadata describing a single unittest TestCase method (pure picklable strings)."""

    name: str
    suite: str
    feature: str
    class_name: str
    module_name: str
    description: str

    @property
    def suite_label(self) -> str:
        return self.suite.upper()

    @property
    def test_id(self) -> str:
        return f"{self.module_name}.{self.class_name}.{self.name}"

    @property
    def task_key(self) -> tuple[str, str, str]:
        return (self.module_name, self.class_name, self.name)

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "suite": self.suite,
            "suite_label": self.suite_label,
            "feature": self.feature,
            "class": self.class_name,
            "module": self.module_name,
            "description": self.description,
        }


def _infer_feature(method_name: str, default_feature: str, suite: str) -> str:
    if default_feature and default_feature != "GENERIC":
        return default_feature
    if method_name.startswith("test_self_"):
        return "SELF"
    return suite.upper() or "UNKNOWN"


def discover_tests(
    suites: Sequence[str],
    *,
    include_self: bool = False,
) -> list[DiscoveredTest]:
    """Import suite modules and collect all TestCase methods in deterministic order."""
    module_entries: list[tuple[str, str]] = []
    seen_modules: set[str] = set()
    if include_self:
        module_entries.append(("self", SELF_TEST_MODULE))
        seen_modules.add(SELF_TEST_MODULE)
    for suite in suites:
        for mod_name in SUITE_MODULES.get(suite, ()):
            if mod_name not in seen_modules:
                module_entries.append((suite, mod_name))
                seen_modules.add(mod_name)

    discovered: list[DiscoveredTest] = []
    seen_names: set[str] = set()

    for expected_suite, mod_name in module_entries:
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
            cls_suite = getattr(cls, "SUITE", "") or expected_suite
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
                feature = getattr(fn, "_e2e_feature", "") or _infer_feature(
                    method_name, cls_feature, cls_suite
                )
                if method_name in seen_names:
                    raise ValueError(f"Duplicate E2E test method name detected: {method_name}")
                seen_names.add(method_name)
                discovered.append(
                    DiscoveredTest(
                        name=method_name,
                        suite=cls_suite,
                        feature=feature,
                        class_name=cls.__name__,
                        module_name=mod_name,
                        description=desc,
                    )
                )

    discovered.sort(
        key=lambda t: (SUITE_ORDER.get(t.suite, 99), t.module_name, t.class_name, t.name)
    )
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
        or needle in t.suite.lower()
        or needle in t.suite_label.lower()
        or needle in t.description.lower()
    ]


def build_counts(tests: list[DiscoveredTest]) -> tuple[dict[str, int], dict[str, dict[str, int]]]:
    by_suite: dict[str, int] = {}
    by_feature: dict[str, dict[str, int]] = {}
    for t in tests:
        by_suite[t.suite] = by_suite.get(t.suite, 0) + 1
        suite_feat = by_feature.setdefault(t.suite, {})
        suite_feat[t.feature] = suite_feat.get(t.feature, 0) + 1
    return by_suite, by_feature


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="run_e2e.py",
        description="Run opaque-box intent-named E2E tests for agy-companion (pure Python 3 stdlib).",
    )
    selector = parser.add_mutually_exclusive_group()
    selector.add_argument(
        "--suite",
        choices=("cuj", "boundary", "shims", "ui-smoke", "all"),
        default=None,
        help="Select intent-named E2E suite (default: all = cuj + boundary + shims).",
    )
    selector.add_argument(
        "--tier",
        choices=("1", "2", "3", "4", "all"),
        default=None,
        help="Deprecated alias for --suite (1/3/4 -> cuj, 2 -> boundary, all -> all).",
    )
    parser.add_argument(
        "-j",
        "--workers",
        type=int,
        default=DEFAULT_WORKERS,
        help=f"Parallel worker process count (default: {DEFAULT_WORKERS}).",
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
        help="Run pure-Python harness self-test guardrails (no agy-companion binary required).",
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
        help="Case-insensitive substring filter on test name, feature, class, module, or suite.",
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
    if args.workers < 1:
        parser.error("--workers must be >= 1.")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a finite number > 0.")
    return args


def _selected_suites(args: argparse.Namespace) -> tuple[tuple[str, ...], bool]:
    if args.tier is not None:
        mapped = TIER_TO_SUITES[args.tier]
        mapped_str = ",".join(mapped) if args.tier != "all" else "all"
        print(
            f"NOTE: --tier {args.tier} is deprecated; mapped to --suite {mapped_str}.",
            file=sys.stderr,
        )
        return mapped, bool(args.self_test)

    if args.suite is not None:
        suites = ALL_DEFAULT_SUITES if args.suite == "all" else (args.suite,)
        return suites, bool(args.self_test)

    if args.self_test:
        return (), True

    return ALL_DEFAULT_SUITES, False


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


def _worker_init(
    e2e_dir_str: str,
    binary_path_str: str | None,
    repo_root_str: str,
    default_timeout: float,
    verbose: bool,
) -> None:
    """Initialize spawned ProcessPoolExecutor worker with zero-bytecode and runtime config."""
    sys.dont_write_bytecode = True
    if e2e_dir_str not in sys.path:
        sys.path.insert(0, e2e_dir_str)
    import harness as worker_harness

    worker_harness.set_runtime_config(
        worker_harness.TestRuntimeConfig(
            binary_path=Path(binary_path_str) if binary_path_str else None,
            repo_root=Path(repo_root_str),
            default_timeout=default_timeout,
            verbose=verbose,
        )
    )


def _run_single_test(task: tuple[str, str, str]) -> dict[str, Any]:
    """Execute one TestCase method and return a picklable outcome dictionary."""
    module_name, class_name, method_name = task
    test_id = f"{module_name}.{class_name}.{method_name}"
    start_ts = time.monotonic()
    result = _RecordingTestResult(failfast=False)
    try:
        mod = importlib.import_module(module_name)
        cls = getattr(mod, class_name)
        case = cls(method_name)
        case.run(result)
    except Exception:
        tb = traceback.format_exc()
        duration = round(time.monotonic() - start_ts, 4)
        return {
            "test": test_id,
            "module": module_name,
            "class": class_name,
            "name": method_name,
            "status": "error",
            "duration_s": duration,
            "records": [{"test": test_id, "status": "error", "duration_s": duration, "detail": tb}],
            "detail": tb,
        }

    duration = round(time.monotonic() - start_ts, 4)
    if result.errors:
        status = "error"
    elif result.failures:
        status = "fail"
    elif result.skipped:
        status = "skip"
    else:
        status = "pass"

    details = [r["detail"] for r in result.records if r.get("detail") and r["status"] in ("fail", "error")]
    if not details and status == "skip":
        details = [r["detail"] for r in result.records if r.get("detail")]
    return {
        "test": test_id,
        "module": module_name,
        "class": class_name,
        "name": method_name,
        "status": status,
        "duration_s": duration,
        "records": result.records,
        "detail": "\n".join(details),
    }


def _print_text_summary(
    outcomes: list[dict[str, Any]],
    *,
    duration_s: float,
    workers: int,
    verbose: bool,
) -> None:
    status_tags = {"pass": "PASS", "fail": "FAIL", "error": "ERROR", "skip": "SKIP"}
    short_chars = {"pass": ".", "fail": "F", "error": "E", "skip": "s"}
    stream = sys.stdout if verbose else sys.stderr

    if verbose:
        for outcome in outcomes:
            tag = status_tags.get(outcome["status"], outcome["status"].upper())
            detail = outcome.get("detail", "")
            suffix = f" ({detail})" if outcome["status"] == "skip" and detail else ""
            print(
                f"[{tag}] ({outcome['duration_s']:.2f}s) {outcome['test']}{suffix}",
                file=stream,
            )
    else:
        line = "".join(short_chars.get(o["status"], "?") for o in outcomes)
        print(line, file=stream)

    failures = [o for o in outcomes if o["status"] in ("fail", "error")]
    for outcome in failures:
        print("=" * 70, file=stream)
        print(f"{outcome['status'].upper()}: {outcome['test']}", file=stream)
        print("-" * 70, file=stream)
        print(outcome.get("detail", "").rstrip(), file=stream)

    passed = sum(1 for o in outcomes if o["status"] == "pass")
    failed = sum(1 for o in outcomes if o["status"] == "fail")
    errors = sum(1 for o in outcomes if o["status"] == "error")
    skipped = sum(1 for o in outcomes if o["status"] == "skip")
    ok = failed == 0 and errors == 0
    verdict = "OK" if ok else "FAILED"
    parts = [f"passed={passed}"]
    if failed:
        parts.append(f"failed={failed}")
    if errors:
        parts.append(f"errors={errors}")
    if skipped:
        parts.append(f"skipped={skipped}")
    print(
        f"Ran {len(outcomes)} tests in {duration_s:.2f}s (workers={workers}) — {verdict} ({', '.join(parts)})",
        file=stream,
    )


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    repo_root = Path(args.repo_root).expanduser().resolve()
    suites, include_self = _selected_suites(args)

    tests = filter_tests(discover_tests(suites, include_self=include_self), args.filter)

    if args.list:
        by_suite, by_feature = build_counts(tests)
        if args.json:
            payload = {
                "total": len(tests),
                "by_suite": by_suite,
                "by_feature": by_feature,
                "tests": [t.to_dict() for t in tests],
            }
            print(json.dumps(payload, indent=2))
        else:
            for t in tests:
                print(f"{t.suite_label}\t{t.feature}\t{t.name}")
        return 0

    if not tests:
        all_selected_empty = (
            args.filter is None
            and not include_self
            and bool(suites)
            and all(not SUITE_MODULES.get(s) for s in suites)
        )
        if all_selected_empty:
            if args.json:
                print(
                    json.dumps(
                        {
                            "status": "pass",
                            "total": 0,
                            "passed": 0,
                            "failed": 0,
                            "errors": 0,
                            "skipped": 0,
                            "duration_s": 0.0,
                            "workers": 1,
                            "results": [],
                        },
                        indent=2,
                    )
                )
            else:
                print(
                    f"No modules registered yet for suite(s): {', '.join(suites)} (0 tests).",
                    file=sys.stderr,
                )
            return 0
        print("ERROR: No tests matched selection/filter.", file=sys.stderr)
        return 2

    needs_binary = any(t.suite not in ("self", "ui-smoke") for t in tests)
    binary_path: Path | None = None
    if needs_binary:
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
                "Hint: build with `cargo build -p companion-cli` or pass `--binary <path>`.",
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

    effective_workers = min(args.workers, len(tests))
    start_ts = time.monotonic()
    outcomes: list[dict[str, Any]] = []

    if effective_workers == 1:
        for t in tests:
            outcome = _run_single_test(t.task_key)
            outcomes.append(outcome)
            if args.fail_fast and outcome["status"] in ("fail", "error"):
                break
    else:
        initargs = (
            str(E2E_DIR),
            str(binary_path) if binary_path is not None else None,
            str(repo_root),
            float(args.timeout),
            bool(args.verbose),
        )
        outcomes_by_idx: dict[int, dict[str, Any]] = {}
        executor = concurrent.futures.ProcessPoolExecutor(
            max_workers=effective_workers,
            initializer=_worker_init,
            initargs=initargs,
        )
        try:
            future_to_idx: dict[concurrent.futures.Future[dict[str, Any]], int] = {
                executor.submit(_run_single_test, t.task_key): idx
                for idx, t in enumerate(tests)
            }
            for fut in concurrent.futures.as_completed(future_to_idx):
                idx = future_to_idx[fut]
                t = tests[idx]
                try:
                    outcome = fut.result()
                except Exception:
                    tb = traceback.format_exc()
                    outcome = {
                        "test": t.test_id,
                        "module": t.module_name,
                        "class": t.class_name,
                        "name": t.name,
                        "status": "error",
                        "duration_s": 0.0,
                        "records": [
                            {
                                "test": t.test_id,
                                "status": "error",
                                "duration_s": 0.0,
                                "detail": tb,
                            }
                        ],
                        "detail": tb,
                    }
                outcomes_by_idx[idx] = outcome
                if args.fail_fast and outcome["status"] in ("fail", "error"):
                    for pending in future_to_idx:
                        pending.cancel()
                    break
        finally:
            executor.shutdown(wait=True, cancel_futures=True)

        outcomes = [outcomes_by_idx[i] for i in sorted(outcomes_by_idx)]

    duration = round(time.monotonic() - start_ts, 4)
    passed = sum(1 for o in outcomes if o["status"] == "pass")
    failed = sum(1 for o in outcomes if o["status"] == "fail")
    errors = sum(1 for o in outcomes if o["status"] == "error")
    skipped = sum(1 for o in outcomes if o["status"] == "skip")
    ok = failed == 0 and errors == 0

    if args.json:
        flat_records: list[dict[str, Any]] = []
        for outcome in outcomes:
            flat_records.extend(outcome.get("records") or [])
        summary = {
            "status": "pass" if ok else "fail",
            "total": len(outcomes),
            "passed": passed,
            "failed": failed,
            "errors": errors,
            "skipped": skipped,
            "duration_s": duration,
            "workers": effective_workers,
            "results": flat_records,
        }
        print(json.dumps(summary, indent=2))
        return 0 if ok else 1

    _print_text_summary(
        outcomes,
        duration_s=duration,
        workers=effective_workers,
        verbose=args.verbose,
    )
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
