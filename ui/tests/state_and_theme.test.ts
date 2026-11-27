import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import {
  CATALOG_POLL_INTERVAL_MS,
  CODEX_PENDING_POLL_MS,
  FEEDBACK_AUTO_DISMISS_MS,
  HEALTHY_POLL_MS,
  UNHEALTHY_BACKOFF_MS,
  chooseInitialTab,
  createAutoDismissTimer,
  createCatalogRefreshReminder,
  formatCatalogRefreshWaitHint,
  needsSettingsAttention,
  nextPollDelayMs,
  nextTabForKey,
  reconcileCatalogRefreshReminder,
  shouldApplyRefresh,
  toggleEnabledModel,
} from "../src/state.ts";
import {
  createThemeController,
  formatCatalogRefreshCopy,
  type ThemeMediaQueryListLike,
} from "../src/dom.ts";
import { installFakeDom } from "./fake_dom.ts";
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

function extractLightDarkTokens(block: string): {
  light: Map<string, string>;
  dark: Map<string, string>;
} {
  const light = new Map<string, string>();
  const dark = new Map<string, string>();
  const re =
    /(--[a-z0-9-]+)\s*:\s*[^;]*?light-dark\(\s*(#[0-9a-fA-F]{6})\s*,\s*(#[0-9a-fA-F]{6})\s*\)/g;
  for (const m of block.matchAll(re)) {
    light.set(m[1], m[2].toLowerCase());
    dark.set(m[1], m[3].toLowerCase());
  }
  return { light, dark };
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
  it("createAutoDismissTimer schedules dismissal, cancels stale timers on reschedule, and respects cancel()", () => {
    assert.ok(FEEDBACK_AUTO_DISMISS_MS > 0 && FEEDBACK_AUTO_DISMISS_MS <= 10_000);
    let nextId = 1;
    const pending = new Map<number, { cb: () => void; ms: number }>();
    const cleared: number[] = [];
    const timer = createAutoDismissTimer(
      (cb, ms) => {
        const id = nextId++;
        pending.set(id, { cb, ms });
        return id;
      },
      (id) => {
        cleared.push(id);
        pending.delete(id);
      },
    );

    const events: string[] = [];
    timer.schedule(FEEDBACK_AUTO_DISMISS_MS, () => events.push("first"));
    assert.equal(pending.size, 1);
    assert.equal(pending.get(1)?.ms, FEEDBACK_AUTO_DISMISS_MS);

    // Rescheduling cancels the first timer
    timer.schedule(FEEDBACK_AUTO_DISMISS_MS, () => events.push("second"));
    assert.deepEqual(cleared, [1]);
    assert.equal(pending.size, 1);
    pending.get(2)!.cb();
    assert.deepEqual(events, ["second"]);

    // Explicit cancel prevents firing
    timer.schedule(FEEDBACK_AUTO_DISMISS_MS, () => events.push("third"));
    const thirdCb = pending.get(3)!.cb;
    timer.cancel();
    assert.deepEqual(cleared, [1, 3]);
    thirdCb(); // even if invoked late, epoch guard ignores it
    assert.deepEqual(events, ["second"]);
  });

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

  const rootMatch = /:root\s*\{([\s\S]*?)\n\}/.exec(css);
  const lightSwitchMatch =
    /:root\[data-theme="light"\],\s*\.companion-root\[data-theme="light"\]\s*\{([\s\S]*?)\n\}/.exec(
      css,
    );
  const darkSwitchMatch =
    /:root\[data-theme="dark"\],\s*\.companion-root\[data-theme="dark"\]\s*\{([\s\S]*?)\n\}/.exec(
      css,
    );

  it("unifies Light & Dark tokens with light-dark() in :root, switches color-scheme via [data-theme], and meets WCAG AA >= 4.5:1", () => {
    assert.ok(rootMatch, "Expected unified :root block in style.css");
    assert.ok(
      /color-scheme\s*:\s*dark\s+light\s*;/.test(rootMatch[1]),
      "Expected :root to declare color-scheme: dark light;",
    );
    assert.equal(
      /@media\s*\(\s*prefers-color-scheme\s*:\s*light\s*\)/.test(css),
      false,
      "Duplicated @media (prefers-color-scheme: light) block must be removed in favor of light-dark()",
    );

    assert.ok(lightSwitchMatch, 'Expected :root[data-theme="light"] color-scheme switch');
    assert.ok(/color-scheme\s*:\s*light\s*;/.test(lightSwitchMatch[1]));
    assert.equal(
      /--[a-z0-9-]+\s*:/.test(lightSwitchMatch[1]),
      false,
      '[data-theme="light"] must switch color-scheme only with zero duplicated -- tokens',
    );

    assert.ok(darkSwitchMatch, 'Expected :root[data-theme="dark"] color-scheme switch');
    assert.ok(/color-scheme\s*:\s*dark\s*;/.test(darkSwitchMatch[1]));
    assert.equal(
      /--[a-z0-9-]+\s*:/.test(darkSwitchMatch[1]),
      false,
      '[data-theme="dark"] must switch color-scheme only with zero duplicated -- tokens',
    );

    const { light, dark } = extractLightDarkTokens(rootMatch[1]);
    assert.equal(light.size, 18, `Expected 18 light-dark() hex color tokens, found ${light.size}`);
    assert.equal(dark.size, 18, `Expected 18 light-dark() hex color tokens, found ${dark.size}`);

    // Exact Light palette pins & WCAG AA contrast
    assert.equal(light.get("--bg"), "#ffffff");
    assert.equal(light.get("--surface"), "#f7f8fa");
    assert.equal(light.get("--brand"), "#1558b0");
    assert.equal(light.get("--brand-fg"), "#ffffff");

    // Exact Dark palette pins & WCAG AA contrast
    assert.equal(dark.get("--bg"), "#1b1c1f");
    assert.equal(dark.get("--surface"), "#232428");
    assert.equal(dark.get("--brand"), "#8ab4f8");
    assert.equal(dark.get("--brand-fg"), "#0f1b2e");

    for (const [label, tokens] of [
      ["Light (light-dark())", light],
      ["Dark (light-dark())", dark],
    ] as const) {
      const bg = tokens.get("--bg")!;
      const surface = tokens.get("--surface")!;
      const brand = tokens.get("--brand")!;
      const brandFg = tokens.get("--brand-fg")!;
      const err = tokens.get("--err")!;
      const errFg = tokens.get("--err-fg")!;

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
      assert.ok(
        contrastRatio(err, errFg) >= 4.5,
        `${label} --err vs --err-fg contrast must be >= 4.5:1`,
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
  });

  it("uses native :has(:checked), reduced-motion, contrast scrollbar, symmetric side gaps, smooth details, and top-layer popover animations", () => {
    assert.ok(css.includes(".pr-row:has(.pr-inp:checked)"));
    assert.equal(css.includes(".pr-row.sel"), false, "Legacy JS .pr-row.sel class must be removed");

    assert.equal(
      css.includes("scrollbar-gutter:"),
      false,
      "Must not reserve an asymmetric scrollbar-gutter on .pane-scroll",
    );

    assert.ok(/@media\s*\(prefers-contrast:\s*more\)/.test(css));
    assert.ok(/@media\s*\(prefers-reduced-motion:\s*reduce\)/.test(css));
    assert.equal(
      /\*\s*\{\s*animation-duration\s*:/.test(css),
      false,
      "Must not use destructive wildcard animation-duration override",
    );
    const reducedBlock = /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{([\s\S]*?)\n\}/.exec(css);
    assert.ok(reducedBlock, "Expected @media (prefers-reduced-motion: reduce) block");
    assert.equal(
      reducedBlock[1].includes(".host-toast"),
      false,
      "prefers-reduced-motion must not touch .host-toast translateX(-50%) centering",
    );
    assert.ok(
      reducedBlock[1].includes("[popover]"),
      "prefers-reduced-motion must disable [popover] transitions and translate",
    );
    assert.ok(
      reducedBlock[1].includes(".model-tooltip.visible"),
      "prefers-reduced-motion must match .model-tooltip.visible specificity",
    );

    assert.ok(css.includes("interpolate-size: allow-keywords"));
    assert.ok(css.includes("details.adv-details::details-content"));
    assert.ok(css.includes("details.codex-fallback::details-content"));

    // Native top-layer popover + @starting-style + overlay + allow-discrete
    assert.ok(css.includes("@supports selector(:popover-open)"));
    assert.ok(css.includes(":popover-open"));
    assert.ok(css.includes("@starting-style"));
    assert.ok(css.includes("allow-discrete"));
    assert.ok(/\boverlay\b/.test(css));
    assert.ok(
      /\.footer-popover\s*\{[^}]*\bcolor\s*:\s*var\(--fg\)\s*;/.test(css),
      ".footer-popover must declare color: var(--fg) to override UA [popover] CanvasText",
    );
  });

  it("uses relative rem typography with 1rem === 16px root and zero px font-size declarations", () => {
    const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");

    assert.deepEqual(
      stripped.match(/\bfont-size\s*:\s*[^;]*\b\d+(?:\.\d+)?px\b/gi) ?? [],
      [],
      "Expected zero px font-size declarations in style.css",
    );
    assert.deepEqual(
      stripped.match(/\bfont\s*:\s*[^;]*\b\d+(?:\.\d+)?px\b/gi) ?? [],
      [],
      "Expected zero px font shorthand declarations in style.css",
    );

    for (const m of stripped.matchAll(/(?:^|\})\s*html(?:\s*,\s*body)?\s*\{([^}]*)\}/g)) {
      assert.equal(
        /\bfont(?:-size)?\s*:/.test(m[1]),
        false,
        "html rule must not declare font or font-size so 1rem stays 16px",
      );
    }

    const bodyMatch = /(?:^|\})\s*body\s*\{([^}]*)\}/.exec(stripped);
    assert.ok(bodyMatch, "Expected standalone body rule in style.css");
    assert.ok(
      /\bfont\s*:\s*0\.8125rem\/1\.45\s+var\(--font-ui\)\s*;/.test(bodyMatch[1]),
      "Expected body to set font: 0.8125rem/1.45 var(--font-ui);",
    );

    const remFontSizes = stripped.match(/\bfont-size\s*:\s*\d+(?:\.\d+)?rem\b/g) ?? [];
    assert.ok(
      remFontSizes.length >= 50,
      `Expected >= 50 rem font-size declarations, found ${remFontSizes.length}`,
    );
  });
});

describe("createThemeController live OS matchMedia & host precedence", () => {
  function createMockMql(initialMatches: boolean): ThemeMediaQueryListLike & {
    emit: (next: boolean) => void;
    listenerCount: () => number;
  } {
    let matches = initialMatches;
    const listeners = new Set<(event: { matches: boolean }) => void>();
    return {
      get matches() {
        return matches;
      },
      addEventListener: (_type, fn) => {
        listeners.add(fn);
      },
      removeEventListener: (_type, fn) => {
        listeners.delete(fn);
      },
      emit: (next: boolean) => {
        matches = next;
        for (const fn of [...listeners]) {
          fn({ matches });
        }
      },
      listenerCount: () => listeners.size,
    };
  }

  it("syncs initial OS theme, follows live matchMedia flips, honors explicit host override, and cleans up on destroy", () => {
    const env = installFakeDom();
    try {
      const root = env.document.createElement("div");
      const mql = createMockMql(false);
      let queried = "";

      const ctrl = createThemeController(
        env.document.documentElement as unknown as HTMLElement,
        root as unknown as HTMLElement,
        (q) => {
          queried = q;
          return mql;
        },
      );

      assert.equal(queried, "(prefers-color-scheme: light)");
      assert.equal(ctrl.currentTheme(), "dark");
      assert.equal(ctrl.isHostExplicit(), false);
      assert.equal(env.document.documentElement.dataset.theme, "dark");
      assert.equal(env.document.documentElement.style.colorScheme, "dark");
      assert.equal(root.getAttribute("data-theme"), "dark");
      assert.equal(mql.listenerCount(), 1);

      // Live OS flip: dark -> light -> dark
      mql.emit(true);
      assert.equal(ctrl.currentTheme(), "light");
      assert.equal(env.document.documentElement.dataset.theme, "light");
      assert.equal(env.document.documentElement.style.colorScheme, "light");
      assert.equal(root.getAttribute("data-theme"), "light");

      mql.emit(false);
      assert.equal(ctrl.currentTheme(), "dark");
      assert.equal(root.getAttribute("data-theme"), "dark");

      // Explicit host theme-change locks precedence over subsequent OS flips
      ctrl.applyHostTheme("dark");
      assert.equal(ctrl.isHostExplicit(), true);
      mql.emit(true);
      assert.equal(ctrl.currentTheme(), "dark");
      assert.equal(env.document.documentElement.dataset.theme, "dark");

      // Subsequent explicit host change still applies
      ctrl.applyHostTheme("light");
      assert.equal(ctrl.currentTheme(), "light");
      assert.equal(env.document.documentElement.style.colorScheme, "light");
      assert.equal(root.getAttribute("data-theme"), "light");

      // destroy() removes listener cleanly
      ctrl.destroy();
      assert.equal(mql.listenerCount(), 0);
      mql.emit(false);
      assert.equal(ctrl.currentTheme(), "light");
    } finally {
      env.teardown();
    }
  });

  it("handles initial OS light preference and missing matchMedia gracefully", () => {
    const env = installFakeDom();
    try {
      const root = env.document.createElement("div");
      const lightMql = createMockMql(true);
      const ctrl1 = createThemeController(
        env.document.documentElement as unknown as HTMLElement,
        root as unknown as HTMLElement,
        () => lightMql,
      );
      assert.equal(ctrl1.currentTheme(), "light");
      assert.equal(env.document.documentElement.dataset.theme, "light");
      ctrl1.destroy();

      const ctrl2 = createThemeController(
        env.document.documentElement as unknown as HTMLElement,
        root as unknown as HTMLElement,
        null,
      );
      assert.equal(ctrl2.currentTheme(), "dark");
      ctrl2.applyHostTheme("light");
      assert.equal(ctrl2.currentTheme(), "light");
      ctrl2.destroy();
    } finally {
      env.teardown();
    }
  });
});

describe("WAI-ARIA tablist keyboard navigation helper", () => {
  const tabs = ["monitor", "usage", "settings"] as const;

  it("wraps ArrowRight/ArrowLeft and jumps Home/End while ignoring other keys", () => {
    assert.equal(nextTabForKey(tabs, "monitor", "ArrowRight"), "usage");
    assert.equal(nextTabForKey(tabs, "settings", "ArrowRight"), "monitor");
    assert.equal(nextTabForKey(tabs, "monitor", "ArrowLeft"), "settings");
    assert.equal(nextTabForKey(tabs, "usage", "ArrowLeft"), "monitor");
    assert.equal(nextTabForKey(tabs, "usage", "Home"), "monitor");
    assert.equal(nextTabForKey(tabs, "monitor", "End"), "settings");
    assert.equal(nextTabForKey(tabs, "monitor", "Enter"), null);
    assert.equal(nextTabForKey([], "monitor", "ArrowRight"), null);
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
