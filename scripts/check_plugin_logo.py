#!/usr/bin/env python3
"""Strict fail-closed Expat SVG and plugin manifest validator for agy-companion."""

from __future__ import annotations

import json
import os
import pathlib
import re
import stat
import sys
import xml.parsers.expat as expat

MAX_LOGO_BYTES = 1_048_576
MAX_SVG_DEPTH = 64
MAX_SVG_ELEMENTS = 10_000
SVG_NS = "http://www.w3.org/2000/svg"

ALLOWED_TAGS = frozenset({
    "svg", "g", "defs", "title", "desc", "clipPath", "filter", "feGaussianBlur",
    "linearGradient", "radialGradient", "stop",
    "path", "rect", "circle", "ellipse", "line", "polyline", "polygon",
})
TEXT_ONLY_TAGS = frozenset({"title", "desc"})
ALLOWED_ATTRS = frozenset({
    "id", "width", "height", "viewBox", "preserveAspectRatio",
    "x", "y", "x1", "y1", "x2", "y2", "cx", "cy", "r", "rx", "ry",
    "d", "points", "gradientUnits", "clipPathUnits", "filterUnits",
    "offset", "stdDeviation",
    "fill", "fill-opacity", "fill-rule",
    "stroke", "stroke-opacity", "stroke-width", "stroke-linecap", "stroke-linejoin",
    "opacity", "stop-color", "stop-opacity", "clip-path", "filter",
    "role", "aria-label", "aria-hidden",
})
URL_ATTRS = frozenset({"fill", "stroke", "clip-path", "filter"})

LOCAL_URL_RE = re.compile(r"^url\(#([A-Za-z_][A-Za-z0-9_.-]*)\)$")
ID_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]*$")
CTRL_CHAR_RE = re.compile(r"[\x00-\x1f\x7f]")
FORBIDDEN_TOKEN_RE = re.compile(r"(?:\\|/\*|\*/|//|@import|expression\s*\(|:)", re.IGNORECASE)


class _SvgAbort(Exception):
    pass


def validate_svg_bytes(raw: bytes) -> list[str]:
    """Validate raw SVG bytes with a strict fail-closed Expat allowlist."""
    if not raw:
        return ["plugin/plugin.json: SVG logo is empty"]
    if len(raw) > MAX_LOGO_BYTES:
        return [f"plugin/plugin.json: SVG logo exceeds {MAX_LOGO_BYTES} bytes ({len(raw)})"]
    try:
        raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        return [f"plugin/plugin.json: SVG logo is not valid UTF-8 ({exc})"]

    stack: list[str] = []
    defined_ids: set[str] = set()
    url_refs: list[tuple[str, str]] = []
    seen_root = False
    elem_count = 0

    def abort(msg: str) -> None:
        raise _SvgAbort(msg)

    def on_xml_decl(version: str | None, encoding: str | None, _standalone: int) -> None:
        if version not in (None, "1.0"):
            abort(f"SVG logo XML version must be 1.0, got {version!r}")
        if encoding is not None and encoding.lower() != "utf-8":
            abort(f"SVG logo XML encoding must be UTF-8, got {encoding!r}")

    def on_start_ns(_prefix: str | None, uri: str) -> None:
        if uri != SVG_NS:
            abort(f"SVG logo declares forbidden XML namespace {uri!r}")

    def on_start_elem(name: str, attrs: dict[str, str]) -> None:
        nonlocal seen_root, elem_count
        elem_ns, local = name.rsplit(" ", 1) if " " in name else ("", name)
        if not seen_root:
            seen_root = True
            if (elem_ns, local) != (SVG_NS, "svg"):
                abort(f"SVG logo root must be {{{SVG_NS}}}svg, got {{{elem_ns}}}{local}")
        elif elem_ns != SVG_NS:
            abort(f"SVG logo element <{local}> uses forbidden namespace {elem_ns!r}")
        elif stack and stack[-1] in TEXT_ONLY_TAGS:
            abort(f"SVG logo element <{stack[-1]}> must not contain child elements")

        if local not in ALLOWED_TAGS:
            abort(f"SVG logo contains disallowed element <{local}>")
        elem_count += 1
        if elem_count > MAX_SVG_ELEMENTS:
            abort(f"SVG logo exceeds maximum element count ({MAX_SVG_ELEMENTS})")
        if len(stack) + 1 > MAX_SVG_DEPTH:
            abort(f"SVG logo exceeds maximum nesting depth ({MAX_SVG_DEPTH})")

        in_defs = "defs" in stack or local == "defs"
        for attr_name, val in attrs.items():
            attr_ns, attr = attr_name.rsplit(" ", 1) if " " in attr_name else ("", attr_name)
            if attr_ns != "":
                abort(f"SVG logo element <{local}> contains namespaced attribute '{attr_ns}:{attr}'")
            if attr.lower().startswith("on"):
                abort(f"SVG logo contains forbidden event handler attribute '{attr}'")
            if attr not in ALLOWED_ATTRS:
                abort(f"SVG logo element <{local}> contains disallowed attribute '{attr}'")
            if CTRL_CHAR_RE.search(val) or FORBIDDEN_TOKEN_RE.search(val):
                abort(f"SVG logo attribute '{attr}' contains forbidden control/URI/CSS token")
            if attr == "id":
                if not ID_RE.fullmatch(val):
                    abort(f"SVG logo has invalid id {val!r}")
                if in_defs:
                    defined_ids.add(val)
            elif "(" in val or ")" in val or "url(" in val.lower():
                if attr not in URL_ATTRS or in_defs:
                    abort(f"SVG logo attribute '{attr}' does not permit functional notation here")
                match = LOCAL_URL_RE.fullmatch(val.strip())
                if not match:
                    abort(f"SVG logo attribute '{attr}' contains non-local url() reference")
                url_refs.append((attr, match.group(1)))
        stack.append(local)

    def on_char_data(data: str) -> None:
        if data.strip(" \t\r\n") and (not stack or stack[-1] not in TEXT_ONLY_TAGS):
            abort(f"SVG logo element <{stack[-1] if stack else 'document'}> must not contain text")

    parser = expat.ParserCreate(namespace_separator=" ")
    parser.XmlDeclHandler = on_xml_decl
    for handler in (
        "StartDoctypeDeclHandler", "EntityDeclHandler", "UnparsedEntityDeclHandler",
        "NotationDeclHandler", "AttlistDeclHandler", "ElementDeclHandler",
        "ExternalEntityRefHandler", "StartCdataSectionHandler",
    ):
        setattr(parser, handler, lambda *_a, h=handler: abort(f"SVG logo forbids {h}"))
    parser.ProcessingInstructionHandler = lambda t, _d: abort(f"SVG logo forbids XML PI <?{t}?>")
    parser.StartNamespaceDeclHandler = on_start_ns
    parser.StartElementHandler = on_start_elem
    parser.EndElementHandler = lambda _n: stack.pop()
    parser.CharacterDataHandler = on_char_data

    try:
        parser.Parse(raw, True)
    except _SvgAbort as exc:
        return [f"plugin/plugin.json: {exc}"]
    except expat.ExpatError as exc:
        return [f"plugin/plugin.json: SVG logo is not valid XML ({exc})"]

    if not seen_root:
        return ["plugin/plugin.json: SVG logo has no root element"]
    missing = [f"{a}=url(#{t})" for a, t in url_refs if t not in defined_ids]
    if missing:
        return [f"plugin/plugin.json: SVG logo references undefined local id(s): {', '.join(missing)}"]
    return []


def validate_plugin_dir(repo_root: pathlib.Path) -> list[str]:
    """Validate plugin JSON manifests and bundled logo path, file type, size, and SVG content."""
    manifests = (
        "plugin/plugin.json",
        "plugin/sidecars/proxy/sidecar.json",
    )
    errors: list[str] = []
    plugin_dir = repo_root / "plugin"
    if plugin_dir.is_symlink() or not plugin_dir.is_dir():
        return ["plugin/plugin.json: plugin directory is missing or a symlink"]
    plugin_resolved = plugin_dir.resolve()

    parsed: dict[str, dict] = {}
    for rel in manifests:
        cur = plugin_dir
        symlink_hit = False
        for part in pathlib.PurePosixPath(rel).parts[1:]:
            cur = cur / part
            if cur.is_symlink():
                errors.append(f"{rel}: must not traverse or point to a symlink ({cur.relative_to(repo_root)})")
                symlink_hit = True
                break
        if symlink_hit:
            continue
        try:
            st = os.lstat(cur)
        except OSError:
            errors.append(f"missing or non-regular manifest {rel}")
            continue
        if not stat.S_ISREG(st.st_mode):
            errors.append(f"missing or non-regular manifest {rel}")
            continue
        try:
            cur.resolve().relative_to(plugin_resolved)
        except Exception:
            errors.append(f"{rel}: escapes plugin root")
            continue
        try:
            data = json.loads(cur.read_text(encoding="utf-8"))
        except Exception as exc:
            errors.append(f"{rel}: invalid JSON ({exc})")
            continue
        if not isinstance(data, dict):
            errors.append(f"{rel}: top-level JSON must be an object")
        else:
            parsed[rel] = data

    sidecar_manifest = parsed.get("plugin/sidecars/proxy/sidecar.json")
    if isinstance(sidecar_manifest, dict):
        if not isinstance(sidecar_manifest.get("command"), str) or not sidecar_manifest["command"].strip():
            errors.append("plugin/sidecars/proxy/sidecar.json: 'command' must be a non-empty string")
        args = sidecar_manifest.get("args")
        if not isinstance(args, list) or not all(isinstance(item, str) for item in args):
            errors.append("plugin/sidecars/proxy/sidecar.json: 'args' must be a list of strings")

    plugin_manifest = parsed.get("plugin/plugin.json")
    if not isinstance(plugin_manifest, dict):
        return errors

    for field in ("name", "version"):
        val = plugin_manifest.get(field)
        if not isinstance(val, str) or not val.strip() or val != val.strip():
            errors.append(f"plugin/plugin.json: '{field}' must be a non-empty trimmed string")

    logo = plugin_manifest.get("logo")
    if not isinstance(logo, str) or not logo.strip() or logo != logo.strip():
        return errors + ["plugin/plugin.json: 'logo' must be a non-empty trimmed string"]
    if "\x00" in logo or logo.startswith("/") or "\\" in logo:
        return errors + [f"plugin/plugin.json: invalid 'logo' path {logo!r}"]

    raw_parts = logo.split("/")
    logo_rel = pathlib.PurePosixPath(logo)
    if any(seg in {"", ".", ".."} for seg in raw_parts) or logo_rel.is_absolute() or ".." in logo_rel.parts:
        return errors + [f"plugin/plugin.json: 'logo' must be a canonical relative path: {logo!r}"]
    if logo_rel.suffix.lower() not in {".svg", ".png", ".jpg", ".jpeg", ".webp"}:
        return errors + [f"plugin/plugin.json: unsupported 'logo' extension {logo_rel.suffix!r}"]

    cur = plugin_dir
    for part in logo_rel.parts:
        cur = cur / part
        if cur.is_symlink():
            return errors + [f"plugin/plugin.json: 'logo' must not traverse or point to a symlink: {logo!r}"]

    try:
        st = os.lstat(cur)
    except OSError:
        return errors + [f"plugin/plugin.json: 'logo' target is missing: {logo!r}"]
    if not stat.S_ISREG(st.st_mode):
        return errors + [f"plugin/plugin.json: 'logo' target is not a regular file: {logo!r}"]

    try:
        cur.resolve().relative_to(plugin_resolved)
    except Exception:
        return errors + [f"plugin/plugin.json: 'logo' escapes plugin root: {logo!r}"]

    if st.st_size == 0:
        return errors + [f"plugin/plugin.json: 'logo' file is empty: {logo!r}"]
    if st.st_size > MAX_LOGO_BYTES:
        return errors + [f"plugin/plugin.json: 'logo' file exceeds {MAX_LOGO_BYTES} bytes ({st.st_size}): {logo!r}"]

    raw = cur.read_bytes()
    if len(raw) == 0 or len(raw) > MAX_LOGO_BYTES:
        return errors + [f"plugin/plugin.json: 'logo' read size out of bounds ({len(raw)}): {logo!r}"]
    if logo_rel.suffix.lower() == ".svg":
        errors.extend(validate_svg_bytes(raw))
    return errors


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    repo_root = pathlib.Path(args[0]) if args else pathlib.Path(__file__).resolve().parent.parent
    errors = validate_plugin_dir(repo_root)
    if errors:
        for err in errors:
            print(err, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
