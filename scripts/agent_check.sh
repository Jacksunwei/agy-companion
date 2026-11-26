#!/usr/bin/env bash
set -euo pipefail
unset CDPATH

SCRIPT_DIR="$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
if ! command -v python3 >/dev/null 2>&1; then
  printf 'FAIL scripts/agent_check.sh: python3 not found on PATH\n' >&2
  exit 1
fi

exec python3 -B "${SCRIPT_DIR}/check.py" --profile all "$@"
