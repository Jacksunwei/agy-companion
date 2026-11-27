"""Shared stdlib-only SemVer parser and PEP 440 helper for agy-companion scripts.

Supports:
  - Final releases:    MAJOR.MINOR.PATCH        (e.g. 0.1.0, 0.2.0)
  - Dev prereleases:   MAJOR.MINOR.PATCH-dev.N  (e.g. 0.2.0-dev.0, 0.2.0-dev.1)

Numeric components reject leading zeros (per SemVer 2.0.0) and strings are
anchored with \\A...\\Z so trailing newlines and extra whitespace fail closed.
"""

from __future__ import annotations

from dataclasses import dataclass
import re
import sys

sys.dont_write_bytecode = True

VERSION_RE = re.compile(
    r"\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-dev\.(0|[1-9][0-9]*))?\Z",
    re.ASCII,
)


@dataclass(frozen=True)
class ParsedVersion:
    major: int
    minor: int
    patch: int
    dev: int | None = None

    @property
    def is_dev(self) -> bool:
        return self.dev is not None

    @property
    def is_final(self) -> bool:
        return self.dev is None

    @property
    def sort_key(self) -> tuple[int, int, int, bool, int]:
        return (self.major, self.minor, self.patch, self.dev is None, self.dev or 0)

    def __str__(self) -> str:
        base = f"{self.major}.{self.minor}.{self.patch}"
        if self.dev is not None:
            return f"{base}-dev.{self.dev}"
        return base

    def to_pep440(self) -> str:
        base = f"{self.major}.{self.minor}.{self.patch}"
        if self.dev is not None:
            return f"{base}.dev{self.dev}"
        return base

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, ParsedVersion):
            return NotImplemented
        return self.sort_key < other.sort_key

    def __le__(self, other: object) -> bool:
        if not isinstance(other, ParsedVersion):
            return NotImplemented
        return self.sort_key <= other.sort_key

    def __gt__(self, other: object) -> bool:
        if not isinstance(other, ParsedVersion):
            return NotImplemented
        return self.sort_key > other.sort_key

    def __ge__(self, other: object) -> bool:
        if not isinstance(other, ParsedVersion):
            return NotImplemented
        return self.sort_key >= other.sort_key


def parse_version(version: str) -> ParsedVersion:
    """Parse MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-dev.N, failing closed otherwise."""
    if not isinstance(version, str):
        raise ValueError(
            f"invalid SemVer version {version!r} "
            "(expected MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-dev.N)"
        )
    match = VERSION_RE.fullmatch(version)
    if not match:
        raise ValueError(
            f"invalid SemVer version {version!r} "
            "(expected MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-dev.N)"
        )
    dev_str = match.group(4)
    return ParsedVersion(
        major=int(match.group(1)),
        minor=int(match.group(2)),
        patch=int(match.group(3)),
        dev=int(dev_str) if dev_str is not None else None,
    )


def version_sort_key(version: str) -> tuple[int, int, int, bool, int]:
    return parse_version(version).sort_key


def is_dev_version(version: str) -> bool:
    return parse_version(version).is_dev


def is_final_version(version: str) -> bool:
    return parse_version(version).is_final


def to_pep440(version: str) -> str:
    return parse_version(version).to_pep440()
