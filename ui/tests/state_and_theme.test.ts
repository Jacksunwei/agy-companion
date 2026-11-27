import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import {
  CATALOG_POLL_INTERVAL_MS,
  CODEX_PENDING_POLL_MS,
  HEALTHY_POLL_MS,
  UNHEALTHY_BACKOFF_MS,
  chooseInitialTab,
  createCatalogRefreshReminder,
  formatCatalogRefreshWaitHint,
  needsSettingsAttention,
  nextPollDelayMs,
  reconcileCatalogRefreshReminder,
  shouldApplyRefresh,
  toggleEnabledModel,
} from "../src/state.ts";
import { formatCatalogRefreshCopy } from "../src/dom.ts";
import {
  CURATED_MODEL_CATALOG,
  parseSettingsSnapshot,
  type SettingsSnapshot,
} from "../src/types.ts";

function loadSnapshot(name: string): SettingsSnapshot {
  const url = new URL(`./fixtures/${name}`, import.meta.url);
  const parsed = parseSettingsSnapshot(JSON.parse(fs.readFileSync(url, "utf8")));
  assert.ok(parsed !== null);
  return parsed;
}

function parseHexRgb(hex: string): [number, number, number] {
  const clean = hex.trim().replace(/^#/, "");
  assert.equal(clean.length, 6, `Expected 6-digit hex color, got ${hex}`);
  return [
    Number.parseInt(clean.slice(0, 2), 16),
    Number.parseInt(clean.slice(2, 4), 16),
    Number.parseInt(clean.slice(4, 6), 16),
  ];
}

function relativeLuminance(hex: string): number {
  const [r, g, b] = parseHexRgb(hex).map((c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrastRatio(hexA: string, hexB: string): number {
  const l1 = relativeLuminance(hexA);
  const l2 = relativeLuminance(hexB);
  const [hi, lo] = l1 >= l2 ? [l1, l2] : [l2, l1];
  return (hi + 0.05) / (lo + 0.05);
}

function extractCssHexTokens(block: string): Map<string, string> {
  const map = new Map<string, string>();
  const re = /(--[a-z0-9-]+)\s*:\s*[^;]*?(#[0-9a-fA-F]{6})\b/g;
  for (const m of block.matchAll(re)) {
    map.set(m[1], m[2].toLowerCase());
  }
  return map;
}

function listFilesRecursive(dir: string): string[] {
  const files: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...listFilesRecursive(full));
    } else if (entry.isFile()) {
      files.push(full);
    }
  }
  return files;
}

describe("ui/src/state.ts pure state helpers", () => {
  it("chooseInitialTab lands on monitor when hybrid_active or api_key_configured, else settings", () => {
    const day0 = loadSnapshot("settings_snapshot_day0_offline.json");
    assert.equal(chooseInitialTab(day0), "settings");

    // Day-0 hybrid_active without API key -> monitor
    const hybridNoKey: SettingsSnapshot = {
      ...day0,
      launcher_integration: day0.launcher_integration
        ? { ...day0.launcher_integration, state: "active", enabled: true, hybrid_active: true }
        : null,
    };
    assert.equal(chooseInitialTab(hybridNoKey), "monitor");

    // Configured key without hybrid_active -> monitor
    const keyNotHybrid: SettingsSnapshot = {
      ...day0,
      config: {
        ...day0.config,
        providers: {
          ...day0.config.providers,
          gemini: { ...day0.config.providers.gemini, api_key_configured: true },
        },
      },
    };
    assert.equal(chooseInitialTab(keyNotHybrid), "monitor");

    // Codex-only authenticated without Gemini key or hybrid_active -> monitor
    const codexOnly: SettingsSnapshot = {
      ...day0,
      config: {
        ...day0.config,
        providers: {
          ...day0.config.providers,
          codex: { ...day0.config.providers.codex, authenticated: true },
        },
      },
    };
    assert.equal(chooseInitialTab(codexOnly), "monitor");

    // Both active -> monitor
    const active = loadSnapshot("settings_snapshot_active.json");
    assert.equal(chooseInitialTab(active), "monitor");

    // launcher_integration null without key -> settings
    const noIntegration: SettingsSnapshot = { ...day0, launcher_integration: null };
    assert.equal(chooseInitialTab(noIntegration), "settings");

    // ls_telemetry_active without hybrid_active or BYOM key -> monitor
    const lsOnly: SettingsSnapshot = {
      ...day0,
      ls_telemetry_active: true,
      launcher_integration: null,
    };
    assert.equal(chooseInitialTab(lsOnly), "monitor");
  });

  it("shouldApplyRefresh guards against stale snapshotEpoch and in-flight writes", () => {
    assert.equal(shouldApplyRefresh(1, 1, false), true);
    assert.equal(shouldApplyRefresh(1, 2, false), false);
    assert.equal(shouldApplyRefresh(2, 2, true), false);
    assert.equal(shouldApplyRefresh(1, 2, true), false);
  });

  it("nextPollDelayMs uses CODEX_PENDING_POLL_MS (2s) while pending, HEALTHY_POLL_MS when healthy, and clamps backoff when unhealthy", () => {
    assert.equal(HEALTHY_POLL_MS, 10_000);
    assert.equal(CODEX_PENDING_POLL_MS, 2_000);
    assert.deepEqual(UNHEALTHY_BACKOFF_MS, [3_000, 5_000, 10_000]);

    assert.equal(nextPollDelayMs(true, 0), 10_000);
    assert.equal(nextPollDelayMs(true, 0, true), 2_000);
    assert.equal(nextPollDelayMs(false, 2, true), 2_000);
    assert.equal(nextPollDelayMs(false, 0), 3_000);
    assert.equal(nextPollDelayMs(false, 1), 3_000);
    assert.equal(nextPollDelayMs(false, 2), 5_000);
    assert.equal(nextPollDelayMs(false, 3), 10_000);
    assert.equal(nextPollDelayMs(false, 99), 10_000);
    // Unhealthy RPC streak before snapshot flips unreachable
    assert.equal(nextPollDelayMs(true, 1), 3_000);
  });

  it("toggleEnabledModel preserves catalog ordering, supports [] pause, preserves extras, and does not mutate input", () => {
    const input = ["gemini-4-hydrogen", "gemini-4-carbon"];
    const snapshotBefore = [...input];

    // Adding a model sorts curated items in catalog order
    const added = toggleEnabledModel(input, "gemini-4-neon");
    assert.deepEqual(added, ["gemini-4-carbon", "gemini-4-neon", "gemini-4-hydrogen"]);
    assert.deepEqual(input, snapshotBefore);

    // Toggling off the last remaining model produces [] (0-model pause)
    const paused = toggleEnabledModel(["gemini-4-carbon"], "gemini-4-carbon");
    assert.deepEqual(paused, []);

    // Toggling on from [] restores that model
    const restored = toggleEnabledModel([], "gemini-3.8-flash");
    assert.deepEqual(restored, ["gemini-3.8-flash"]);

    // Extra/custom persisted IDs stay after curated catalog entries
    const withExtra = toggleEnabledModel(["custom-model", "gemini-4-hydrogen"], "gemini-4-carbon");
    assert.deepEqual(withExtra, ["gemini-4-carbon", "gemini-4-hydrogen", "custom-model"]);
    assert.deepEqual(toggleEnabledModel(withExtra, "custom-model"), [
      "gemini-4-carbon",
      "gemini-4-hydrogen",
    ]);

    // Catalog invariant check
    assert.equal(CURATED_MODEL_CATALOG[0].id, "gemini-4-carbon");
  });

  it("needsSettingsAttention flags attention across Unix, Windows, and unsupported states", () => {
    assert.equal(needsSettingsAttention(null), false);

    const active = loadSnapshot("settings_snapshot_active.json");
    assert.equal(needsSettingsAttention(active), false);

    // Windows with enabled: false + hybrid_active: true + configured key -> no attention
    const winHealthy: SettingsSnapshot = {
      ...active,
      platform: "windows",
      launcher_integration: active.launcher_integration
        ? { ...active.launcher_integration, enabled: false, hybrid_active: true }
        : null,
    };
    assert.equal(needsSettingsAttention(winHealthy), false);

    // Linux state: "unsupported" with hybrid_active: true + configured key -> no attention
    const linuxUnsupported: SettingsSnapshot = {
      ...active,
      platform: "linux",
      launcher_integration: active.launcher_integration
        ? {
            ...active.launcher_integration,
            state: "unsupported",
            enabled: false,
            hybrid_active: true,
          }
        : null,
    };
    assert.equal(needsSettingsAttention(linuxUnsupported), false);

    // Missing Gemini key AND Codex signed out (even when hybrid_active: true) -> needs attention
    const noKey: SettingsSnapshot = {
      ...active,
      config: {
        ...active.config,
        providers: {
          ...active.config.providers,
          gemini: { ...active.config.providers.gemini, api_key_configured: false },
        },
      },
    };
    assert.equal(needsSettingsAttention(noKey), true);

    // Codex-only user (no Gemini key, codex.authenticated: true, hybrid_active: true) -> zero attention dot!
    const codexOnlyHealthy: SettingsSnapshot = {
      ...noKey,
      config: {
        ...noKey.config,
        providers: {
          ...noKey.config.providers,
          codex: { ...noKey.config.providers.codex, authenticated: true },
        },
      },
    };
    assert.equal(needsSettingsAttention(codexOnlyHealthy), false);

    // Unix disabled -> needs attention
    const unixDisabled: SettingsSnapshot = {
      ...active,
      launcher_integration: active.launcher_integration
        ? { ...active.launcher_integration, state: "disabled", enabled: false, hybrid_active: true }
        : null,
    };
    assert.equal(needsSettingsAttention(unixDisabled), true);

    // manual_required -> needs attention
    const manual: SettingsSnapshot = {
      ...active,
      launcher_integration: active.launcher_integration
        ? { ...active.launcher_integration, state: "manual_required", hybrid_active: true }
        : null,
    };
    assert.equal(needsSettingsAttention(manual), true);

    // !hybrid_active -> needs attention
    const notHybrid: SettingsSnapshot = {
      ...active,
      launcher_integration: active.launcher_integration
        ? { ...active.launcher_integration, hybrid_active: false }
        : null,
    };
    assert.equal(needsSettingsAttention(notHybrid), true);

    // launcher_integration null -> needs attention
    assert.equal(needsSettingsAttention({ ...active, launcher_integration: null }), true);

    // ls_telemetry_active WITHOUT any BYOM provider configured:
    // does NOT nag for !hybrid_active, unixDisabled, or null integration, but DOES flag manual_required
    const lsNoByomNotHybrid: SettingsSnapshot = {
      ...noKey,
      ls_telemetry_active: true,
      launcher_integration: noKey.launcher_integration
        ? { ...noKey.launcher_integration, state: "enabled", enabled: true, hybrid_active: false }
        : null,
    };
    assert.equal(needsSettingsAttention(lsNoByomNotHybrid), false);
    assert.equal(
      needsSettingsAttention({
        ...lsNoByomNotHybrid,
        launcher_integration: lsNoByomNotHybrid.launcher_integration
          ? { ...lsNoByomNotHybrid.launcher_integration, state: "disabled", enabled: false }
          : null,
      }),
      false,
    );
    assert.equal(
      needsSettingsAttention({ ...lsNoByomNotHybrid, launcher_integration: null }),
      false,
    );
    assert.equal(
      needsSettingsAttention({
        ...lsNoByomNotHybrid,
        launcher_integration: lsNoByomNotHybrid.launcher_integration
          ? { ...lsNoByomNotHybrid.launcher_integration, state: "manual_required" }
          : null,
      }),
      true,
    );

    // ls_telemetry_active WITH Gemini key OR Codex auth configured still flags !hybrid_active / unixDisabled
    assert.equal(
      needsSettingsAttention({
        ...notHybrid,
        ls_telemetry_active: true,
      }),
      true,
    );
    assert.equal(
      needsSettingsAttention({
        ...unixDisabled,
        ls_telemetry_active: true,
      }),
      true,
    );
    assert.equal(
      needsSettingsAttention({
        ...lsNoByomNotHybrid,
        config: {
          ...lsNoByomNotHybrid.config,
          providers: {
            ...lsNoByomNotHybrid.config.providers,
            codex: { ...lsNoByomNotHybrid.config.providers.codex, authenticated: true },
          },
        },
      }),
      true,
    );
    assert.equal(
      needsSettingsAttention({
        ...active,
        ls_telemetry_active: true,
      }),
      false,
    );
  });

  it("manages catalogRefreshReminder lifecycle, auto-clear reconciliation, wait hints, and platform copy", () => {
    assert.equal(CATALOG_POLL_INTERVAL_MS, 300_000);
    const active = loadSnapshot("settings_snapshot_active.json");
    const offline = loadSnapshot("settings_snapshot_day0_offline.json");

    // Only activates when proxy is reachable AND hybrid_active === true
    assert.equal(createCatalogRefreshReminder(null, offline, 1_747_300_000_000), null);
    assert.equal(
      createCatalogRefreshReminder(
        active,
        {
          ...active,
          proxy: { ...active.proxy, reachable: false },
        },
        1_747_300_000_000,
      ),
      null,
    );
    assert.equal(
      createCatalogRefreshReminder(
        active,
        {
          ...active,
          launcher_integration: active.launcher_integration
            ? { ...active.launcher_integration, hybrid_active: false }
            : null,
        },
        1_747_300_000_000,
      ),
      null,
    );

    const rem = createCatalogRefreshReminder(null, active, 1_747_300_050_000);
    assert.deepEqual(rem, {
      baselineCatalogRefreshAtMs: 1_747_299_990_000,
      changedAtMs: 1_747_300_050_000,
    });

    // Falls back to prevSnap timestamp when nextSnap omits it
    const remFromPrev = createCatalogRefreshReminder(
      active,
      { ...active, last_catalog_refresh_at_ms: null },
      1_747_300_050_000,
    );
    assert.deepEqual(remFromPrev, {
      baselineCatalogRefreshAtMs: 1_747_299_990_000,
      changedAtMs: 1_747_300_050_000,
    });

    // Reconcile preserves reminder when timestamp is unchanged or older
    assert.equal(reconcileCatalogRefreshReminder(null, active), null);
    assert.deepEqual(reconcileCatalogRefreshReminder(rem, active), rem);
    assert.deepEqual(
      reconcileCatalogRefreshReminder(rem, {
        ...active,
        last_catalog_refresh_at_ms: 1_747_299_980_000,
      }),
      rem,
    );

    // Auto-clears as soon as a newer last_catalog_refresh_at_ms arrives
    assert.equal(
      reconcileCatalogRefreshReminder(rem, {
        ...active,
        last_catalog_refresh_at_ms: 1_747_299_990_001,
      }),
      null,
    );

    // Null baseline auto-clears on any non-null timestamp (clock-skew safe)
    const nullBaselineRem = createCatalogRefreshReminder(
      null,
      { ...active, last_catalog_refresh_at_ms: null },
      1_747_300_050_000,
    );
    assert.deepEqual(nullBaselineRem, {
      baselineCatalogRefreshAtMs: null,
      changedAtMs: 1_747_300_050_000,
    });
    assert.deepEqual(
      reconcileCatalogRefreshReminder(nullBaselineRem, {
        ...active,
        last_catalog_refresh_at_ms: null,
      }),
      nullBaselineRem,
    );
    assert.equal(
      reconcileCatalogRefreshReminder(nullBaselineRem, {
        ...active,
        last_catalog_refresh_at_ms: 1_747_300_000_000,
      }),
      null,
    );

    // Clears if proxy becomes unreachable or hybrid_active drops
    assert.equal(
      reconcileCatalogRefreshReminder(rem, {
        ...active,
        proxy: { ...active.proxy, reachable: false },
      }),
      null,
    );
    assert.equal(
      reconcileCatalogRefreshReminder(rem, {
        ...active,
        launcher_integration: active.launcher_integration
          ? { ...active.launcher_integration, hybrid_active: false }
          : null,
      }),
      null,
    );

    // Wait hint formatting across all ranges
    const t0 = 1_750_000_000_000;
    assert.equal(formatCatalogRefreshWaitHint(null, t0), "up to ~5m");
    assert.equal(formatCatalogRefreshWaitHint(0, t0), "up to ~5m");
    assert.equal(formatCatalogRefreshWaitHint(Number.NaN, t0), "up to ~5m");
    assert.equal(formatCatalogRefreshWaitHint(t0 + 1_000, t0), "up to ~5m"); // clock skew
    assert.equal(formatCatalogRefreshWaitHint(t0, t0), "~5m"); // 300s left
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 10_000), "~5m"); // 290s left
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 175_000), "~2m"); // 125s left
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 239_999), "~1m"); // 60.001s left
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 240_000), "<1m"); // 60s left
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 299_000), "<1m"); // 1s left
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 300_000), "<1m"); // overdue grace start
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 360_000), "<1m"); // overdue grace end
    assert.equal(formatCatalogRefreshWaitHint(t0, t0 + 360_001), "up to ~5m"); // stale > 6m

    // Platform copy
    assert.equal(
      formatCatalogRefreshCopy("macos", "~2m"),
      "Quit (\u2318Q) & reopen Antigravity, or wait ~2m for the model list to update.",
    );
    assert.equal(
      formatCatalogRefreshCopy("linux", "<1m"),
      "Quit & reopen Antigravity, or wait <1m for the model list to update.",
    );
    assert.equal(
      formatCatalogRefreshCopy("windows", "up to ~5m"),
      "Quit & reopen Antigravity, or wait up to ~5m for the model list to update.",
    );
  });
});

describe("ui/src/style.css theme tokens & WCAG AA contrast", () => {
  const css = fs.readFileSync(new URL("../src/style.css", import.meta.url), "utf8");

  const darkMatch = /:root\s*\{([\s\S]*?)\n\}/.exec(css);
  const lightAttrMatch =
    /:root\[data-theme="light"\],\s*\.companion-root\[data-theme="light"\]\s*\{([\s\S]*?)\n\}/.exec(
      css,
    );
  const lightMediaMatch =
    /@media\s*\(prefers-color-scheme:\s*light\)\s*\{\s*:root:not\(\[data-theme="dark"\]\)\s*\{([\s\S]*?)\n\s*\}\s*\}/.exec(
      css,
    );

  it("defines Light --brand: #1558b0 and matching WCAG AA tokens in both data-theme=light and prefers-color-scheme: light", () => {
    assert.ok(darkMatch, "Expected dark :root block in style.css");
    assert.ok(lightAttrMatch, 'Expected :root[data-theme="light"] block in style.css');
    assert.ok(lightMediaMatch, "Expected @media (prefers-color-scheme: light) block in style.css");

    const dark = extractCssHexTokens(darkMatch[1]);
    const lightAttr = extractCssHexTokens(lightAttrMatch[1]);
    const lightMedia = extractCssHexTokens(lightMediaMatch[1]);

    for (const [label, tokens] of [
      [':root[data-theme="light"]', lightAttr],
      ["@media (prefers-color-scheme: light)", lightMedia],
    ] as const) {
      assert.equal(tokens.get("--bg"), "#ffffff", `${label} --bg`);
      assert.equal(tokens.get("--surface"), "#f7f8fa", `${label} --surface`);
      assert.equal(tokens.get("--brand"), "#1558b0", `${label} --brand`);
      assert.equal(tokens.get("--brand-fg"), "#ffffff", `${label} --brand-fg`);

      const bg = tokens.get("--bg")!;
      const surface = tokens.get("--surface")!;
      const brand = tokens.get("--brand")!;
      const brandFg = tokens.get("--brand-fg")!;

      assert.ok(
        contrastRatio(brand, bg) >= 4.5,
        `${label} --brand vs --bg contrast must be >= 4.5:1`,
      );
      assert.ok(
        contrastRatio(brand, brandFg) >= 4.5,
        `${label} --brand vs --brand-fg contrast must be >= 4.5:1`,
      );
      assert.ok(
        contrastRatio(brand, surface) >= 4.5,
        `${label} --brand vs --surface contrast must be >= 4.5:1`,
      );

      for (const key of ["--fg", "--fg-2", "--fg-3", "--ok", "--warn", "--err"] as const) {
        const val = tokens.get(key);
        assert.ok(val, `${label} missing ${key}`);
        assert.ok(
          contrastRatio(val, bg) >= 4.5,
          `${label} ${key} (${val}) vs ${bg} contrast ${contrastRatio(val, bg).toFixed(2)}:1 < 4.5:1`,
        );
      }
    }

    const darkBg = dark.get("--bg");
    const darkBrand = dark.get("--brand");
    const darkBrandFg = dark.get("--brand-fg");
    assert.ok(darkBg && darkBrand && darkBrandFg);
    assert.ok(contrastRatio(darkBrand, darkBg) >= 4.5);
    assert.ok(contrastRatio(darkBrand, darkBrandFg) >= 4.5);
  });
});

describe("UX v2 open-source copy hygiene across ui/src/ and ui/dist/index.html", () => {
  // Exact 8 lowercase tokens mirrored from crates/companion-ui/src/tests.rs
  const FORBIDDEN_CI = [
    "cloudcode",
    "jetski",
    "-cloudcode-pa.googleapis.com",
    "restart required",
    "agy companion",
    "coming soon",
    "refreshbtn",
    "auto \u2192 workspace",
  ] as const;

  it("contains zero forbidden internal/legacy tokens in every ui/src/ file and ui/dist/index.html", () => {
    const uiSrcDir = fileURLToPath(new URL("../src/", import.meta.url));
    const srcFiles = listFilesRecursive(uiSrcDir);
    assert.ok(srcFiles.length >= 10, `Expected >= 10 files under ui/src, found ${srcFiles.length}`);

    for (const file of srcFiles) {
      const lower = fs.readFileSync(file, "utf8").toLowerCase();
      for (const token of FORBIDDEN_CI) {
        assert.equal(
          lower.includes(token),
          false,
          `Forbidden token ${JSON.stringify(token)} found in ${path.basename(file)}`,
        );
      }
    }

    const distPath = fileURLToPath(new URL("../dist/index.html", import.meta.url));
    assert.equal(fs.existsSync(distPath), true, "Expected built ui/dist/index.html to exist");
    const distHtml = fs.readFileSync(distPath, "utf8");
    const distLower = distHtml.toLowerCase();
    for (const token of FORBIDDEN_CI) {
      assert.equal(
        distLower.includes(token),
        false,
        `Forbidden token ${JSON.stringify(token)} found in ui/dist/index.html`,
      );
    }
  });
});
