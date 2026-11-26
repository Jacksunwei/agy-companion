#!/usr/bin/env python3
"""Fast in-process unit tests for scripts/check_plugin_logo.py."""

from __future__ import annotations

import json
import os
import pathlib
import sys
import tempfile
import unittest

SCRIPTS_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = SCRIPTS_DIR.parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

from check_plugin_logo import (  # noqa: E402
    MAX_LOGO_BYTES,
    MAX_SVG_DEPTH,
    validate_plugin_dir,
    validate_svg_bytes,
)

MINIMAL_SVG = (
    b'<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">'
    b'<title>Logo</title><desc>Static icon</desc>'
    b'<defs><linearGradient id="g"><stop offset="0%" stop-color="#fff"/></linearGradient></defs>'
    b'<rect x="0" y="0" width="16" height="16" fill="url(#g)"/>'
    b'</svg>'
)


def _write_valid_plugin_tree(root: pathlib.Path, *, logo_rel: str = "assets/logo.svg", logo_bytes: bytes = MINIMAL_SVG) -> None:
    plugin = root / "plugin"
    (plugin / "assets").mkdir(parents=True, exist_ok=True)
    (plugin / "sidecars" / "proxy").mkdir(parents=True, exist_ok=True)
    (plugin / "plugin.json").write_text(
        json.dumps({"name": "agy-companion", "version": "0.1.0", "logo": logo_rel}),
        encoding="utf-8",
    )
    (plugin / "mcp_config.json").write_text(
        json.dumps({"mcpServers": {"hub": {"command": "bash", "args": []}}}),
        encoding="utf-8",
    )
    (plugin / "sidecars" / "proxy" / "sidecar.json").write_text(
        json.dumps({"command": "bash", "args": ["serve"]}),
        encoding="utf-8",
    )
    if logo_bytes is not None:
        (plugin / "assets" / "logo.svg").write_bytes(logo_bytes)


class TestCheckPluginLogo(unittest.TestCase):
    def test_real_repo_plugin_and_logo_pass(self) -> None:
        self.assertEqual(validate_plugin_dir(REPO_ROOT), [])
        raw = (REPO_ROOT / "plugin" / "assets" / "logo.svg").read_bytes()
        self.assertEqual(validate_svg_bytes(raw), [])

    def test_minimal_valid_svg_passes(self) -> None:
        self.assertEqual(validate_svg_bytes(MINIMAL_SVG), [])

    def test_rejects_banned_elements_and_attributes(self) -> None:
        cases: list[tuple[str, bytes]] = [
            ("use_element", b'<svg xmlns="http://www.w3.org/2000/svg"><defs><rect id="a" width="1" height="1"/></defs><use href="#a"/></svg>'),
            ("href_attr", b'<svg xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="a" href="#b"/></defs></svg>'),
            ("xlink_href", b'<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><rect width="1" height="1" xlink:href="#a"/></svg>'),
            ("style_element", b'<svg xmlns="http://www.w3.org/2000/svg"><style>rect{fill:red}</style></svg>'),
            ("style_attr", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" style="fill:red"/></svg>'),
            ("onload_attr", b'<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>'),
            ("onclick_attr", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" onclick="alert(1)"/></svg>'),
            ("script_element", b'<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>'),
            ("foreignObject", b'<svg xmlns="http://www.w3.org/2000/svg"><foreignObject/></svg>'),
            ("image_element", b'<svg xmlns="http://www.w3.org/2000/svg"><image width="1" height="1"/></svg>'),
            ("anchor_element", b'<svg xmlns="http://www.w3.org/2000/svg"><a/></svg>'),
            ("animate_element", b'<svg xmlns="http://www.w3.org/2000/svg"><animate/></svg>'),
            ("set_element", b'<svg xmlns="http://www.w3.org/2000/svg"><set/></svg>'),
            ("doctype_xxe", b'<!DOCTYPE svg [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><svg xmlns="http://www.w3.org/2000/svg"/>'),
            ("processing_instruction", b'<?xml-stylesheet type="text/css" href="evil.css"?><svg xmlns="http://www.w3.org/2000/svg"/>'),
            ("cdata_section", b'<svg xmlns="http://www.w3.org/2000/svg"><title><![CDATA[hi]]></title></svg>'),
            ("unknown_element", b'<svg xmlns="http://www.w3.org/2000/svg"><customTag/></svg>'),
            ("unknown_attribute", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" data-x="1"/></svg>'),
            ("non_svg_namespace", b'<svg xmlns="http://www.w3.org/1999/xhtml"/>'),
            ("extra_namespace", b'<svg xmlns="http://www.w3.org/2000/svg" xmlns:x="https://example.com"/>'),
            ("external_url", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" fill="url(https://evil.example/x.svg#a)"/></svg>'),
            ("javascript_url", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" fill="url(javascript:alert(1))"/></svg>'),
            ("data_uri_scheme", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" fill="data:image/svg+xml,foo"/></svg>'),
            ("self_ref_outside_defs", b'<svg xmlns="http://www.w3.org/2000/svg"><g id="a" filter="url(#a)"/></svg>'),
            ("url_inside_defs", b'<svg xmlns="http://www.w3.org/2000/svg"><defs><clipPath id="a" clip-path="url(#a)"/></defs></svg>'),
            ("undefined_local_ref", b'<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1" fill="url(#missing)"/></svg>'),
            ("text_in_non_text_tag", b'<svg xmlns="http://www.w3.org/2000/svg"><g>hello</g></svg>'),
            ("child_in_title", b'<svg xmlns="http://www.w3.org/2000/svg"><title><g/></title></svg>'),
            ("invalid_utf8", b'\xff\xfe<svg/>'),
            ("malformed_xml", b'<svg xmlns="http://www.w3.org/2000/svg"><rect></svg>'),
        ]
        for label, payload in cases:
            with self.subTest(case=label):
                errs = validate_svg_bytes(payload)
                self.assertTrue(errs, f"expected rejection for {label}")

    def test_rejects_depth_and_size_bounds(self) -> None:
        deep = (
            b'<svg xmlns="http://www.w3.org/2000/svg">'
            + b"<g>" * MAX_SVG_DEPTH
            + b"</g>" * MAX_SVG_DEPTH
            + b"</svg>"
        )
        self.assertTrue(validate_svg_bytes(deep))
        oversized = b'<svg xmlns="http://www.w3.org/2000/svg">' + b" " * MAX_LOGO_BYTES + b"</svg>"
        self.assertTrue(validate_svg_bytes(oversized))

    def test_rejects_symlink_traversal_and_oversized_files(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            outside = root / "outside.svg"
            outside.write_bytes(MINIMAL_SVG)

            # Traversal, absolute path, backslash, dot segment
            for bad_path in ("../outside.svg", "/etc/passwd", r"assets\logo.svg", "assets/./logo.svg", "assets//logo.svg"):
                case_root = root / f"bad_{abs(hash(bad_path))}"
                _write_valid_plugin_tree(case_root, logo_rel=bad_path)
                self.assertTrue(validate_plugin_dir(case_root), f"expected failure for {bad_path!r}")

            # Symlink logo file
            sym_root = root / "sym_file"
            _write_valid_plugin_tree(sym_root)
            logo_file = sym_root / "plugin" / "assets" / "logo.svg"
            logo_file.unlink()
            os.symlink(outside, logo_file)
            self.assertTrue(validate_plugin_dir(sym_root))

            # Symlink intermediate directory (assets)
            sym_dir_root = root / "sym_dir"
            _write_valid_plugin_tree(sym_dir_root)
            assets_dir = sym_dir_root / "plugin" / "assets"
            (assets_dir / "logo.svg").unlink()
            assets_dir.rmdir()
            real_assets = root / "real_assets"
            real_assets.mkdir()
            (real_assets / "logo.svg").write_bytes(MINIMAL_SVG)
            os.symlink(real_assets, assets_dir)
            self.assertTrue(validate_plugin_dir(sym_dir_root))

            # Symlink intermediate manifest directory (plugin/sidecars and plugin/sidecars/proxy)
            sym_sidecars_root = root / "sym_sidecars"
            _write_valid_plugin_tree(sym_sidecars_root)
            sidecars_dir = sym_sidecars_root / "plugin" / "sidecars"
            (sidecars_dir / "proxy" / "sidecar.json").unlink()
            (sidecars_dir / "proxy").rmdir()
            sidecars_dir.rmdir()
            real_sidecars = root / "real_sidecars"
            (real_sidecars / "proxy").mkdir(parents=True)
            (real_sidecars / "proxy" / "sidecar.json").write_text(
                json.dumps({"command": "bash", "args": ["serve"]}),
                encoding="utf-8",
            )
            os.symlink(real_sidecars, sidecars_dir)
            self.assertTrue(validate_plugin_dir(sym_sidecars_root))

            sym_proxy_root = root / "sym_proxy"
            _write_valid_plugin_tree(sym_proxy_root)
            proxy_dir = sym_proxy_root / "plugin" / "sidecars" / "proxy"
            (proxy_dir / "sidecar.json").unlink()
            proxy_dir.rmdir()
            os.symlink(real_sidecars / "proxy", proxy_dir)
            self.assertTrue(validate_plugin_dir(sym_proxy_root))

            # Oversized and empty logo files
            big_root = root / "big"
            _write_valid_plugin_tree(big_root, logo_bytes=b" " * (MAX_LOGO_BYTES + 1))
            self.assertTrue(validate_plugin_dir(big_root))

            empty_root = root / "empty"
            _write_valid_plugin_tree(empty_root, logo_bytes=b"")
            self.assertTrue(validate_plugin_dir(empty_root))

            # Malformed manifest JSON
            bad_json_root = root / "bad_json"
            _write_valid_plugin_tree(bad_json_root)
            (bad_json_root / "plugin" / "mcp_config.json").write_text("{invalid", encoding="utf-8")
            self.assertTrue(validate_plugin_dir(bad_json_root))


if __name__ == "__main__":
    unittest.main()
