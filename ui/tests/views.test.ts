import assert from "node:assert/strict";
import fs from "node:fs";
import { afterEach, beforeEach, describe, it } from "node:test";
import { buildBannerNode, computeBannerSignature } from "../src/banners.ts";
import {
  createBrandMarkSvg,
  createToastController,
  createTooltipController,
} from "../src/dom.ts";
import { createFooterController } from "../src/footer.ts";
import { buildMonitorView, computeMonitorSignature } from "../src/monitor.ts";
import { createSettingsView, formatCodexStatusBadge } from "../src/settings.ts";
import {
  CURATED_MODEL_CATALOG,
  parseSettingsSnapshot,
  parseUsageSummary,
  type AppState,
  type SettingsSnapshot,
  type UsageSummary,
} from "../src/types.ts";
import { buildUsageView, computeUsageSignature } from "../src/usage.ts";
import { FakeElement, FakeEvent, installFakeDom, type FakeDomEnv } from "./fake_dom.ts";

function loadFixture<T>(name: string): T {
  const url = new URL(`./fixtures/${name}`, import.meta.url);
  return JSON.parse(fs.readFileSync(url, "utf8")) as T;
}

function loadActiveSnapshot(): SettingsSnapshot {
  const parsed = parseSettingsSnapshot(loadFixture("settings_snapshot_active.json"));
  assert.ok(parsed !== null);
  return parsed;
}

function loadOfflineSnapshot(): SettingsSnapshot {
  const parsed = parseSettingsSnapshot(loadFixture("settings_snapshot_day0_offline.json"));
  assert.ok(parsed !== null);
  return parsed;
}

function loadUsage7d(): UsageSummary {
  const parsed = parseUsageSummary(loadFixture("usage_summary_7d.json"));
  assert.ok(parsed !== null);
  return parsed;
}

function makeState(overrides: Partial<AppState> = {}): AppState {
  return {
    connected: true,
    standalone: false,
    hydratedFromInitialResult: true,
    initialTabApplied: true,
    tabPinnedByUser: false,
    activeTab: "monitor",
    snapshot: loadActiveSnapshot(),
    snapshotEpoch: 1,
    loadError: null,

    refreshing: false,
    saving: false,
    testing: false,
    clearing: false,
    updatingIntegration: false,
    unhealthyStreak: 0,

    apiKeyDraft: "",
    geminiKeyEditOpen: false,
    clearConfirmOpen: false,
    disableProxyConfirmOpen: false,
    advOpen: false,
    keyFeedback: null,
    modelFeedback: null,
    advFeedback: null,
    catalogRefreshReminder: null,

    codexBusy: false,
    codexTesting: false,
    codexAuthorizeUrl: null,
    codexManualFallbackOpen: false,
    codexManualInputDraft: "",
    codexSignOutConfirmOpen: false,
    codexFeedback: null,
    codexModelFeedback: null,

    usageRange: "7d",
    usageUnit: "usd",
    usageGroup: "model",
    usageSummary: null,
    usageLoading: false,
    usageError: null,
    usageEpoch: 0,

    toastMessage: null,
    ...overrides,
  };
}

let env: FakeDomEnv;

beforeEach(() => {
  env = installFakeDom();
});

afterEach(() => {
  env.teardown();
});

describe("buildBannerNode priority queue & interactions", () => {
  const noopActions = () => {
    const calls = { recheck: 0, enableProxy: 0, dismissCatalog: 0, toasts: [] as string[] };
    return {
      calls,
      actions: {
        onRecheck: () => {
          calls.recheck += 1;
        },
        onEnableProxy: () => {
          calls.enableProxy += 1;
        },
        onDismissCatalogReminder: () => {
          calls.dismissCatalog += 1;
        },
        onToast: (msg: string) => {
          calls.toasts.push(msg);
        },
      },
    };
  };

  it("handles standalone, loadError + Re-check, and null snapshot", () => {
    const { actions, calls } = noopActions();

    const standalone = buildBannerNode(makeState({ standalone: true }), actions) as unknown as FakeElement;
    assert.ok(standalone);
    assert.equal(standalone.tagName, "ASIDE");
    assert.equal(standalone.getAttribute("role"), null);
    assert.equal(standalone.getAttribute("aria-label"), "Preview mode");
    assert.equal(standalone.className, "banner info");
    assert.ok(standalone.textContent.includes("Preview mode"));

    const errBanner = buildBannerNode(
      makeState({ snapshot: null, loadError: "Sidecar timeout", refreshing: false }),
      actions,
    ) as unknown as FakeElement;
    assert.ok(errBanner);
    assert.equal(errBanner.getAttribute("role"), "alert");
    assert.ok(errBanner.textContent.includes("Unable to load companion state"));
    assert.ok(errBanner.textContent.includes("Sidecar timeout"));
    const retryBtn = errBanner.querySelector("button");
    assert.ok(retryBtn);
    assert.equal(retryBtn.disabled, false);
    retryBtn.click();
    assert.equal(calls.recheck, 1);

    const refreshingBanner = buildBannerNode(
      makeState({ snapshot: null, loadError: "Sidecar timeout", refreshing: true }),
      actions,
    ) as unknown as FakeElement;
    assert.equal(refreshingBanner.querySelector("button")?.disabled, true);

    assert.equal(buildBannerNode(makeState({ snapshot: null, loadError: null }), actions), null);
  });

  it("enforces priority 1 (manual_required) over offline/disabled/!hybrid_active with Copy & Re-check", async () => {
    const { actions, calls } = noopActions();
    const offline = loadOfflineSnapshot(); // has manual_required + !reachable + !enabled + !hybrid_active
    let copiedText = "";
    env.mockClipboard(async (text) => {
      copiedText = text;
    });

    const node = buildBannerNode(makeState({ snapshot: offline }), actions) as unknown as FakeElement;
    assert.ok(node);
    assert.equal(node.getAttribute("role"), "status");
    assert.ok(node.textContent.includes("One manual step needed"));
    const code = node.querySelector("code");
    assert.ok(code && code.textContent.includes("env.sh"));

    const buttons = node.querySelectorAll("button");
    assert.equal(buttons.length, 2);
    const [copyBtn, recheckBtn] = buttons;
    assert.equal(copyBtn.textContent, "Copy");
    copyBtn.click();
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(copiedText, offline.launcher_integration?.manual_snippet);
    assert.equal(copyBtn.textContent, "Copied");
    assert.equal(env.pendingTimerCount(), 1);
    env.flushTimers();
    assert.equal(copyBtn.textContent, "Copy");

    recheckBtn.click();
    assert.equal(calls.recheck, 1);

    // When clipboard fails and execCommand returns false, fires onToast fallback
    env.mockClipboard(async () => {
      throw new Error("denied");
    });
    env.document.execCommandResult = false;
    copyBtn.click();
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(calls.toasts.length, 1);
    assert.ok(calls.toasts[0].includes("manually"));

    // Fallback snippet when manual_snippet is null
    const noSnippet: SettingsSnapshot = {
      ...offline,
      launcher_integration: offline.launcher_integration
        ? { ...offline.launcher_integration, manual_snippet: null }
        : null,
    };
    const fallbackNode = buildBannerNode(
      makeState({ snapshot: noSnippet }),
      actions,
    ) as unknown as FakeElement;
    assert.ok(
      fallbackNode
        .querySelector("code")
        ?.textContent.includes('$HOME/.local/share/agy-companion/env.sh'),
    );
  });

  it("enforces priority 2 (!reachable) > priority 3 (Unix !enabled) > priority 4 (!hybrid_active Windows/Unix)", () => {
    const { actions, calls } = noopActions();
    const base = loadActiveSnapshot();

    // Priority 2: !reachable overrides !enabled & !hybrid_active
    const offlineDisabled: SettingsSnapshot = {
      ...base,
      proxy: { ...base.proxy, reachable: false },
      launcher_integration: base.launcher_integration
        ? { ...base.launcher_integration, state: "disabled", enabled: false, hybrid_active: false }
        : null,
    };
    const p2 = buildBannerNode(makeState({ snapshot: offlineDisabled }), actions) as unknown as FakeElement;
    assert.equal(p2.getAttribute("role"), "alert");
    assert.ok(p2.textContent.includes("Local proxy offline"));
    assert.ok(p2.textContent.includes("\u2318Q"));

    // Priority 3: Unix !enabled overrides !hybrid_active
    const unixDisabled: SettingsSnapshot = {
      ...base,
      launcher_integration: base.launcher_integration
        ? { ...base.launcher_integration, state: "disabled", enabled: false, hybrid_active: false }
        : null,
    };
    const p3 = buildBannerNode(makeState({ snapshot: unixDisabled }), actions) as unknown as FakeElement;
    assert.equal(p3.getAttribute("role"), "status");
    assert.ok(p3.textContent.includes("Local proxy is disabled"));
    const enableBtn = p3.querySelector("button");
    assert.ok(enableBtn);
    assert.equal(enableBtn.textContent, "Re-enable proxy");
    enableBtn.click();
    assert.equal(calls.enableProxy, 1);

    const p3Busy = buildBannerNode(
      makeState({ snapshot: unixDisabled, updatingIntegration: true }),
      actions,
    ) as unknown as FakeElement;
    assert.equal(p3Busy.querySelector("button")?.textContent, "Re-enabling\u2026");
    assert.equal(p3Busy.querySelector("button")?.disabled, true);

    // Priority 4a: Windows !hybrid_active (even when enabled: false) -> Companion shortcut banner
    const winNotHybrid: SettingsSnapshot = {
      ...base,
      platform: "windows",
      launcher_integration: base.launcher_integration
        ? { ...base.launcher_integration, state: "disabled", enabled: false, hybrid_active: false }
        : null,
    };
    const p4Win = buildBannerNode(makeState({ snapshot: winNotHybrid }), actions) as unknown as FakeElement;
    assert.equal(p4Win.tagName, "ASIDE");
    assert.equal(p4Win.getAttribute("role"), null);
    assert.equal(p4Win.getAttribute("aria-label"), "Reopen Antigravity");
    assert.ok(p4Win.textContent.includes("Reopen Antigravity via Companion shortcut"));

    // Priority 4b: macOS enabled && !hybrid_active
    const macNotHybrid: SettingsSnapshot = {
      ...base,
      platform: "macos",
      launcher_integration: base.launcher_integration
        ? { ...base.launcher_integration, state: "enabled", enabled: true, hybrid_active: false }
        : null,
    };
    const p4Mac = buildBannerNode(makeState({ snapshot: macNotHybrid }), actions) as unknown as FakeElement;
    assert.equal(p4Mac.tagName, "ASIDE");
    assert.equal(p4Mac.getAttribute("role"), null);
    assert.equal(p4Mac.getAttribute("aria-label"), "Reopen Antigravity");
    assert.ok(p4Mac.textContent.includes("Quit (\u2318Q) and reopen Antigravity"));

    // Priority 4c: Linux enabled && !hybrid_active (no ⌘Q)
    const linuxNotHybrid: SettingsSnapshot = {
      ...macNotHybrid,
      platform: "linux",
    };
    const p4Linux = buildBannerNode(makeState({ snapshot: linuxNotHybrid }), actions) as unknown as FakeElement;
    assert.ok(p4Linux.textContent.includes("Quit and reopen Antigravity"));
    assert.equal(p4Linux.textContent.includes("\u2318Q"), false);

    // Healthy + hybrid_active (including with version_skew: true) -> null (no banner)
    assert.equal(buildBannerNode(makeState({ snapshot: base }), actions), null);
    assert.equal(
      buildBannerNode(makeState({ snapshot: { ...base, version_skew: true } }), actions),
      null,
    );

    // ls_telemetry_active WITHOUT any BYOM key configured:
    // suppresses Priority 3 (Unix disabled) & Priority 4 (!hybrid_active), keeps Priority 1 & 2
    const lsNoByomBase: SettingsSnapshot = {
      ...base,
      ls_telemetry_active: true,
      config: {
        ...base.config,
        providers: {
          ...base.config.providers,
          gemini: { ...base.config.providers.gemini, api_key_configured: false, api_key_hint: null },
          codex: { ...base.config.providers.codex, authenticated: false },
        },
      },
      launcher_integration: base.launcher_integration
        ? { ...base.launcher_integration, state: "enabled", enabled: true, hybrid_active: false }
        : null,
    };
    assert.equal(buildBannerNode(makeState({ snapshot: lsNoByomBase }), actions), null);
    assert.equal(
      buildBannerNode(
        makeState({
          snapshot: {
            ...lsNoByomBase,
            launcher_integration: lsNoByomBase.launcher_integration
              ? { ...lsNoByomBase.launcher_integration, state: "disabled", enabled: false }
              : null,
          },
        }),
        actions,
      ),
      null,
    );
    assert.equal(
      buildBannerNode(makeState({ snapshot: { ...lsNoByomBase, platform: "windows" } }), actions),
      null,
    );
    // Priority 1 manual_required & Priority 2 !reachable remain active even when ls_telemetry_active
    const lsManual = buildBannerNode(
      makeState({
        snapshot: {
          ...lsNoByomBase,
          launcher_integration: lsNoByomBase.launcher_integration
            ? { ...lsNoByomBase.launcher_integration, state: "manual_required" }
            : null,
        },
      }),
      actions,
    ) as unknown as FakeElement;
    assert.ok(lsManual.textContent.includes("One manual step needed"));
    const lsOffline = buildBannerNode(
      makeState({
        snapshot: {
          ...lsNoByomBase,
          proxy: { ...lsNoByomBase.proxy, reachable: false },
        },
      }),
      actions,
    ) as unknown as FakeElement;
    assert.ok(lsOffline.textContent.includes("Local proxy offline"));

    // ls_telemetry_active WITH BYOM key configured STILL shows "Quit (⌘Q) and reopen Antigravity" when !hybrid_active
    const lsWithKeyNotHybrid = buildBannerNode(
      makeState({
        snapshot: {
          ...macNotHybrid,
          ls_telemetry_active: true,
        },
      }),
      actions,
    ) as unknown as FakeElement;
    assert.ok(lsWithKeyNotHybrid.textContent.includes("Quit (\u2318Q) and reopen Antigravity"));
  });

  it("renders Priority 5 catalog refresh reminder with platform copy, wait hint, Dismiss button, and P1-P4 preemption", () => {
    const { actions, calls } = noopActions();
    const base = loadActiveSnapshot(); // last_catalog_refresh_at_ms = 1_747_299_990_000
    const reminder = {
      baselineCatalogRefreshAtMs: base.last_catalog_refresh_at_ms,
      changedAtMs: 1_747_300_100_000,
    };

    // macOS with live countdown (~3m when 120s elapsed -> 180s remaining)
    const macNode = buildBannerNode(
      makeState({ snapshot: base, catalogRefreshReminder: reminder }),
      actions,
      1_747_299_990_000 + 120_000,
    ) as unknown as FakeElement;
    assert.ok(macNode);
    assert.equal(macNode.className, "banner info banner-dismissible");
    assert.equal(macNode.getAttribute("role"), "status");
    assert.ok(macNode.textContent.includes("Model list updated"));
    assert.ok(
      macNode.textContent.includes(
        "Quit (\u2318Q) & reopen Antigravity, or wait ~3m for the model list to update.",
      ),
    );
    const dismissBtn = macNode.querySelector("button.banner-dismiss")!;
    assert.ok(dismissBtn);
    assert.equal(dismissBtn.getAttribute("aria-label"), "Dismiss");
    assert.equal(/refresh/i.test(dismissBtn.getAttribute("aria-label") ?? ""), false);
    dismissBtn.click();
    assert.equal(calls.dismissCatalog, 1);

    // Linux & Windows omit ⌘Q and fall back to "up to ~5m" when timestamp is null
    for (const platform of ["linux", "windows"] as const) {
      const node = buildBannerNode(
        makeState({
          snapshot: { ...base, platform, last_catalog_refresh_at_ms: null },
          catalogRefreshReminder: { baselineCatalogRefreshAtMs: null, changedAtMs: 1_747_300_100_000 },
        }),
        actions,
        1_747_300_100_000,
      ) as unknown as FakeElement;
      assert.ok(node);
      assert.ok(
        node.textContent.includes(
          "Quit & reopen Antigravity, or wait up to ~5m for the model list to update.",
        ),
      );
      assert.equal(node.textContent.includes("\u2318Q"), false);
    }

    // Still renders after clearing last BYOM key when ls_telemetry_active && hybrid_active
    const lsClearedLastProvider = buildBannerNode(
      makeState({
        snapshot: {
          ...base,
          ls_telemetry_active: true,
          config: {
            ...base.config,
            providers: {
              ...base.config.providers,
              gemini: { ...base.config.providers.gemini, api_key_configured: false, api_key_hint: null },
              codex: { ...base.config.providers.codex, authenticated: false },
            },
          },
        },
        catalogRefreshReminder: reminder,
      }),
      actions,
      1_747_299_990_000 + 250_000,
    ) as unknown as FakeElement;
    assert.ok(lsClearedLastProvider);
    assert.ok(lsClearedLastProvider.textContent.includes("wait <1m"));

    // Preempted by Priority 4 (!hybrid_active) — zero duplicate banners
    const p4Wins = buildBannerNode(
      makeState({
        snapshot: {
          ...base,
          launcher_integration: base.launcher_integration
            ? { ...base.launcher_integration, hybrid_active: false }
            : null,
        },
        catalogRefreshReminder: reminder,
      }),
      actions,
      1_747_300_100_000,
    ) as unknown as FakeElement;
    assert.ok(p4Wins);
    assert.ok(p4Wins.textContent.includes("Quit (\u2318Q) and reopen Antigravity"));
    assert.equal(p4Wins.textContent.includes("Model list updated"), false);

    // Preempted by Priority 2 (!reachable)
    const p2Wins = buildBannerNode(
      makeState({
        snapshot: { ...base, proxy: { ...base.proxy, reachable: false } },
        catalogRefreshReminder: reminder,
      }),
      actions,
      1_747_300_100_000,
    ) as unknown as FakeElement;
    assert.ok(p2Wins.textContent.includes("Local proxy offline"));
    assert.equal(p2Wins.textContent.includes("Model list updated"), false);
  });

  it("computeBannerSignature stays stable across unrelated poll updates and flips when banner state changes", () => {
    const offline = loadOfflineSnapshot();
    const s1 = makeState({ snapshot: offline, refreshing: false });
    const s2 = makeState({
      snapshot: {
        ...offline,
        proxy_rss_bytes: 99_999_999,
      },
      refreshing: false,
    });
    assert.equal(computeBannerSignature(s1, 1000), computeBannerSignature(s2, 1000));
    assert.notEqual(
      computeBannerSignature(s1, 1000),
      computeBannerSignature(makeState({ snapshot: offline, refreshing: true }), 1000),
    );

    const active = loadActiveSnapshot();
    const reminder = {
      baselineCatalogRefreshAtMs: active.last_catalog_refresh_at_ms,
      changedAtMs: 1_747_300_100_000,
    };
    const remState = makeState({ snapshot: active, catalogRefreshReminder: reminder });
    const t0 = 1_747_299_990_000 + 120_000; // ~3m
    const tSameBucket = t0 + 10_000; // still ~3m
    const tLaterBucket = 1_747_299_990_000 + 250_000; // <1m
    assert.equal(computeBannerSignature(remState, t0), computeBannerSignature(remState, tSameBucket));
    assert.notEqual(
      computeBannerSignature(remState, t0),
      computeBannerSignature(remState, tLaterBucket),
    );
  });
});

describe("createFooterController health-only popover, RAM indicator & interactions", () => {
  it("renders Live pill, right-side RAM, and strictly 4 health-only rows when healthy", () => {
    const container = env.document.createElement("div");
    env.document.body.append(container);
    const ctrl = createFooterController(container as unknown as HTMLElement);
    ctrl.update(makeState());

    const footer = ctrl.footer as unknown as FakeElement;
    const popover = ctrl.popover as unknown as FakeElement;

    const trigger = footer.querySelector(".status-trigger")!;
    assert.equal(trigger.textContent, "Live");
    assert.ok(trigger.querySelector(".sdot.ok"));

    const ram = footer.querySelector(".footer-ram")!;
    assert.equal(ram.hidden, false);
    assert.equal(ram.textContent, "24 MB RAM");

    assert.ok(popover.textContent.includes("Local proxy live"));
    assert.ok(popover.textContent.includes("Healthy"));

    const rows = popover.querySelectorAll(".kv-row");
    const visibleKeys = rows
      .filter((r) => !r.hidden)
      .map((r) => r.querySelector(".kv-k")?.textContent);
    assert.deepEqual(visibleKeys, ["Endpoint", "Antigravity plan", "Proxy / CLI", "Memory"]);

    // Strictly health-only: never leaks settings/session/platform clutter into popover
    for (const forbidden of ["Enabled models", "Session", "Platform"]) {
      assert.equal(popover.textContent.includes(forbidden), false);
    }

    ctrl.destroy();
  });

  it("hides RAM when unreachable, null, or 0, and surfaces offline/disabled/version_skew states", () => {
    const container = env.document.createElement("div");
    env.document.body.append(container);
    const ctrl = createFooterController(container as unknown as HTMLElement);
    const footer = ctrl.footer as unknown as FakeElement;
    const popover = ctrl.popover as unknown as FakeElement;
    const base = loadActiveSnapshot();

    // proxy_rss_bytes: null -> RAM hidden, Memory shows em-dash
    ctrl.update(
      makeState({
        snapshot: {
          ...base,
          proxy_rss_bytes: null,
          stats: base.stats ? { ...base.stats, proxy_rss_bytes: null } : null,
        },
      }),
    );
    assert.equal(footer.querySelector(".footer-ram")?.hidden, true);
    assert.ok(popover.textContent.includes("Memory\u2014"));

    // proxy_rss_bytes: 0 -> RAM hidden
    ctrl.update(
      makeState({
        snapshot: {
          ...base,
          proxy_rss_bytes: 0,
          stats: base.stats ? { ...base.stats, proxy_rss_bytes: 0 } : null,
        },
      }),
    );
    assert.equal(footer.querySelector(".footer-ram")?.hidden, true);

    // Offline -> Proxy offline · Retrying…, RAM hidden, Issue row visible
    ctrl.update(makeState({ snapshot: loadOfflineSnapshot() }));
    assert.equal(footer.querySelector(".status-trigger")?.textContent, "Proxy offline \u00b7 Retrying\u2026");
    assert.equal(footer.querySelector(".footer-ram")?.hidden, true);
    const issueRow = popover.querySelectorAll(".kv-row")[4];
    assert.equal(issueRow.hidden, false);
    assert.ok(issueRow.textContent.includes("connection refused"));

    // Unix disabled -> Proxy disabled
    ctrl.update(
      makeState({
        snapshot: {
          ...base,
          launcher_integration: base.launcher_integration
            ? { ...base.launcher_integration, state: "disabled", enabled: false }
            : null,
        },
      }),
    );
    assert.equal(footer.querySelector(".status-trigger")?.textContent, "Proxy disabled");

    // Version skew -> Update pending / Version skew / Issue row with CLI & proxy versions
    ctrl.update(
      makeState({
        snapshot: {
          ...base,
          binary_version: "0.2.0",
          version_skew: true,
          proxy: { ...base.proxy, version: "0.1.0" },
        },
      }),
    );
    assert.equal(footer.querySelector(".status-trigger")?.textContent, "Update pending");
    assert.ok(popover.textContent.includes("Version skew"));
    assert.equal(issueRow.hidden, false);
    assert.ok(issueRow.textContent.includes("v0.1.0"));
    assert.ok(issueRow.textContent.includes("v0.2.0"));

    ctrl.destroy();
  });

  it("supports trigger click toggle, outside-click dismissal, inside-click retention, hover & closePopover/destroy", () => {
    assert.equal("popover" in HTMLElement.prototype, false);
    const container = env.document.createElement("div");
    const outside = env.document.createElement("div");
    env.document.body.append(container, outside);

    const ctrl = createFooterController(container as unknown as HTMLElement);
    ctrl.update(makeState());
    const trigger = (ctrl.footer as unknown as FakeElement).querySelector(".status-trigger")!;
    const popover = ctrl.popover as unknown as FakeElement;

    assert.equal(popover.getAttribute("popover"), "auto");
    assert.equal(ctrl.isPopoverOpen(), false);
    assert.equal(popover.hidden, true);

    // Click trigger opens
    trigger.click();
    assert.equal(ctrl.isPopoverOpen(), true);
    assert.equal(popover.hidden, false);
    assert.equal(trigger.getAttribute("aria-expanded"), "true");

    // Click inside popover keeps it open
    popover.click();
    assert.equal(ctrl.isPopoverOpen(), true);

    // Click outside closes it via document click listener
    outside.click();
    assert.equal(ctrl.isPopoverOpen(), false);
    assert.equal(trigger.getAttribute("aria-expanded"), "false");

    // Hover opens and mouseleave schedules close
    trigger.dispatchEvent(new FakeEvent("mouseenter", { bubbles: false }));
    assert.equal(ctrl.isPopoverOpen(), true);
    trigger.dispatchEvent(new FakeEvent("mouseleave", { bubbles: false }));
    assert.equal(env.pendingTimerCount(), 1);
    env.flushTimers();
    assert.equal(ctrl.isPopoverOpen(), false);

    // Trigger toggle open then closePopover() (Escape hook)
    trigger.click();
    assert.equal(ctrl.isPopoverOpen(), true);
    ctrl.closePopover();
    assert.equal(ctrl.isPopoverOpen(), false);

    ctrl.destroy();
    assert.equal(container.children.length, 0);
  });

  it("uses native Popover API (popover=auto, popovertarget, toggle sync) when supported without setting hidden=true", () => {
    env.enableNativePopover();
    assert.equal("popover" in HTMLElement.prototype, true);

    const container = env.document.createElement("div");
    env.document.body.append(container);

    const ctrl = createFooterController(container as unknown as HTMLElement);
    ctrl.update(makeState());
    const trigger = (ctrl.footer as unknown as FakeElement).querySelector(".status-trigger")!;
    const popover = ctrl.popover as unknown as FakeElement;

    assert.equal(popover.getAttribute("popover"), "auto");
    assert.equal(trigger.getAttribute("popovertarget"), "status-popover");
    assert.equal(popover.hidden, false, "Native popover must NOT set hidden=true");
    assert.equal(ctrl.isPopoverOpen(), false);
    assert.equal(popover.matches(":popover-open"), false);
    assert.equal(trigger.getAttribute("aria-expanded"), "false");

    // Simulated pointer sequence (mouseenter -> mousedown -> focus -> click) opens cleanly without double-toggling
    trigger.dispatchEvent(new FakeEvent("mouseenter", { bubbles: false }));
    trigger.dispatchEvent(new FakeEvent("mousedown", { bubbles: true }));
    trigger.focus();
    trigger.click();
    assert.equal(popover.matches(":popover-open"), true);
    assert.equal(ctrl.isPopoverOpen(), true);
    assert.equal(trigger.getAttribute("aria-expanded"), "true");

    // Second click on trigger immediately toggles closed via popovertarget
    trigger.dispatchEvent(new FakeEvent("mousedown", { bubbles: true }));
    trigger.click();
    assert.equal(popover.matches(":popover-open"), false);
    assert.equal(ctrl.isPopoverOpen(), false);
    assert.equal(trigger.getAttribute("aria-expanded"), "false");

    // Third click on trigger while hovered re-opens via popovertarget
    trigger.dispatchEvent(new FakeEvent("mousedown", { bubbles: true }));
    trigger.click();
    assert.equal(popover.matches(":popover-open"), true);
    assert.equal(trigger.getAttribute("aria-expanded"), "true");
    ctrl.closePopover();

    // Hover opens via showPopover(), native light-dismiss (hidePopover) syncs aria-expanded via toggle event
    trigger.dispatchEvent(new FakeEvent("mouseleave", { bubbles: false }));
    trigger.dispatchEvent(new FakeEvent("mouseenter", { bubbles: false }));
    assert.equal(popover.matches(":popover-open"), true);
    assert.equal(trigger.getAttribute("aria-expanded"), "true");

    (popover as unknown as { hidePopover: () => void }).hidePopover();
    assert.equal(ctrl.isPopoverOpen(), false);
    assert.equal(popover.matches(":popover-open"), false);
    assert.equal(trigger.getAttribute("aria-expanded"), "false");

    // Hover open -> mouseleave timer closes via hidePopover()
    trigger.dispatchEvent(new FakeEvent("mouseenter", { bubbles: false }));
    assert.equal(ctrl.isPopoverOpen(), true);
    trigger.dispatchEvent(new FakeEvent("mouseleave", { bubbles: false }));
    env.flushTimers();
    assert.equal(ctrl.isPopoverOpen(), false);
    assert.equal(trigger.getAttribute("aria-expanded"), "false");

    // closePopover() and destroy() clean up without throwing
    trigger.click();
    assert.equal(ctrl.isPopoverOpen(), true);
    ctrl.closePopover();
    assert.equal(ctrl.isPopoverOpen(), false);

    ctrl.destroy();
    assert.equal(container.children.length, 0);
  });
});

describe("createToastController fallback & native popover=manual paths", () => {
  it("toggles hidden on fallback path, resets timer on rapid re-show, and cleans up on destroy", () => {
    assert.equal("popover" in HTMLElement.prototype, false);
    const container = env.document.createElement("div");
    env.document.body.append(container);

    const toast = createToastController(container as unknown as HTMLElement);
    const el = toast.element as unknown as FakeElement;
    assert.equal(el.getAttribute("popover"), "manual");
    assert.equal(el.getAttribute("role"), "status");
    assert.equal(el.hidden, true);
    assert.equal(toast.isOpen(), false);

    let hiddenCount = 0;
    toast.show("Copied", 1000, () => {
      hiddenCount += 1;
    });
    assert.equal(el.textContent, "Copied");
    assert.equal(el.hidden, false);
    assert.equal(toast.isOpen(), true);
    assert.equal(env.pendingTimerCount(), 1);

    toast.show("Updated", 1000, () => {
      hiddenCount += 1;
    });
    assert.equal(el.textContent, "Updated");
    assert.equal(env.pendingTimerCount(), 1);

    env.flushTimers();
    assert.equal(el.hidden, true);
    assert.equal(toast.isOpen(), false);
    assert.equal(hiddenCount, 1);

    toast.show("Closing");
    toast.destroy();
    assert.equal(env.pendingTimerCount(), 0);
    assert.equal(container.children.length, 0);
  });

  it("uses native showPopover()/hidePopover() with popover=manual and hidden=false when supported", () => {
    env.enableNativePopover();
    const container = env.document.createElement("div");
    env.document.body.append(container);

    const toast = createToastController(container as unknown as HTMLElement);
    const el = toast.element as unknown as FakeElement;
    assert.equal(el.getAttribute("popover"), "manual");
    assert.equal(el.getAttribute("role"), "status");
    assert.equal(el.hidden, false, "Native popover=manual toast must NOT set hidden=true");
    assert.equal(toast.isOpen(), false);
    assert.equal(el.matches(":popover-open"), false);

    toast.show("Saved", 2000);
    assert.equal(el.textContent, "Saved");
    assert.equal(el.matches(":popover-open"), true);
    assert.equal(toast.isOpen(), true);

    env.flushTimers();
    assert.equal(el.matches(":popover-open"), false);
    assert.equal(toast.isOpen(), false);

    toast.show("Still open");
    assert.equal(el.matches(":popover-open"), true);
    toast.destroy();
    assert.equal(el.matches(":popover-open"), false);
    assert.equal(container.children.length, 0);
  });
});

describe("buildMonitorView empty states & active mixed session", () => {
  it("renders calm 2-state empty cards with zero buttons and zero API key prompts", () => {
    const base = loadActiveSnapshot();

    // !hybrid_active empty state
    const notHybridSnap: SettingsSnapshot = {
      ...base,
      stats: null,
      launcher_integration: base.launcher_integration
        ? { ...base.launcher_integration, hybrid_active: false }
        : null,
    };
    const host1 = env.document.createElement("div");
    host1.append(buildMonitorView(makeState({ snapshot: notHybridSnap })) as unknown as FakeElement);
    assert.ok(host1.textContent.includes("Waiting for Antigravity to connect"));
    assert.ok(host1.textContent.includes("\u2318Q"));
    assert.equal(host1.querySelectorAll("button").length, 0);
    assert.equal(host1.querySelectorAll(".tile").length, 0);
    assert.equal(host1.textContent.includes("Open Settings"), false);

    // hybrid_active empty state
    const hybridEmptySnap: SettingsSnapshot = { ...base, stats: null };
    const host2 = env.document.createElement("div");
    host2.append(buildMonitorView(makeState({ snapshot: hybridEmptySnap })) as unknown as FakeElement);
    assert.ok(host2.textContent.includes("Waiting for requests"));
    assert.equal(host2.querySelectorAll("button").length, 0);
    assert.equal(host2.querySelectorAll(".tile").length, 0);
    assert.equal(host2.textContent.includes("Open Settings"), false);

    // ls_telemetry_active empty state even when !hybrid_active -> connected "Waiting for requests"
    const lsEmptySnap: SettingsSnapshot = {
      ...notHybridSnap,
      ls_telemetry_active: true,
    };
    const host3 = env.document.createElement("div");
    host3.append(buildMonitorView(makeState({ snapshot: lsEmptySnap })) as unknown as FakeElement);
    assert.ok(host3.textContent.includes("Waiting for requests"));
    assert.equal(host3.textContent.includes("Waiting for Antigravity to connect"), false);
    assert.equal(host3.querySelectorAll("button").length, 0);
    assert.equal(host3.querySelectorAll(".tile").length, 0);
  });

  it("renders Vertex Claude session with Anthropic builtin badge, 200K context window, and est. list price", () => {
    const base = loadActiveSnapshot();
    const claudeSnap: SettingsSnapshot = {
      ...base,
      ls_telemetry_active: true,
      stats: {
        usage: {
          requests: 1,
          prompt_tokens: 100_000,
          cached_tokens: 80_000,
          output_tokens: 2_000,
          reasoning_tokens: 500,
          cache_hit_ratio: 0.8,
        },
        proxy_rss_bytes: 25_165_824,
        max_context_tokens: null,
        recent_calls: [
          {
            timestamp_ms: 1747300200000,
            provider: "vertex_claude",
            source: "builtin",
            model: "claude-sonnet-4-6-high",
            input_tokens: 100_000,
            cached_tokens: 80_000,
            output_tokens: 2_000,
            reasoning_tokens: 500,
            latency_ms: 600,
            cold_start: false,
            cache_break: false,
          },
        ],
        by_model: [
          {
            provider: "vertex_claude",
            source: "builtin",
            model: "claude-sonnet-4-6-high",
            requests: 1,
            input_tokens: 100_000,
            cached_tokens: 80_000,
            output_tokens: 2_000,
            reasoning_tokens: 500,
            total_latency_ms: 600,
          },
        ],
      },
    };
    const host = env.document.createElement("div");
    host.append(buildMonitorView(makeState({ snapshot: claudeSnap })) as unknown as FakeElement);
    const hdrTag = host.querySelector(".mon-model-hdr .tag");
    assert.equal(hdrTag?.textContent, "Anthropic");
    assert.equal(hdrTag?.className, "tag");
    assert.ok(host.textContent.includes("of 200K"));
    assert.ok(host.textContent.includes("50%"));
    const firstBarSegs = host.querySelector(".prop-bar")?.querySelectorAll(".prop-seg") ?? [];
    assert.deepEqual(
      firstBarSegs.map((s) => s.style.width),
      ["40.0%", "10.0%", "1.0%"],
    );
    const costVal = host.querySelector(".cost-foot-val");
    assert.ok(costVal && costVal.textContent.startsWith("\u2248 $"));
  });

  it("renders active mixed built-in + BYOM session with prompt hero, call dots, 3 tiles, list price, and breakdown", () => {
    const active = loadActiveSnapshot();
    // Add a 3rd call with cache_break to exercise all 3 dot states
    const snap: SettingsSnapshot = {
      ...active,
      stats: {
        ...active.stats!,
        recent_calls: [
          ...active.stats!.recent_calls,
          {
            timestamp_ms: 1747300120000,
            provider: "gemini",
            source: "proxy",
            model: "gemini-4-carbon",
            input_tokens: 3000,
            cached_tokens: 0,
            output_tokens: 100,
            reasoning_tokens: 0,
            latency_ms: 510,
            cold_start: false,
            cache_break: true,
          },
        ],
      },
    };

    const host = env.document.createElement("div");
    host.append(buildMonitorView(makeState({ snapshot: snap })) as unknown as FakeElement);

    const cards = host.querySelectorAll(".card");
    assert.equal(cards.length, 2);
    assert.equal(cards[0].getAttribute("aria-label"), null);
    assert.equal(cards[1].getAttribute("aria-label"), null);
    assert.equal(cards[0].querySelector("h2.mon-model-name")?.textContent, "Gemini 4 Carbon");
    assert.equal(cards[1].querySelector("h2.sec-t")?.textContent, "Usage by model");

    // Latest prompt hero & proportional bar widths
    assert.ok(cards[0].textContent.includes("Latest prompt"));
    assert.ok(cards[0].textContent.includes("Gemini 4 Carbon"));
    const segs = cards[0].querySelectorAll(".prop-bar .prop-seg");
    assert.ok(segs.length >= 3);
    assert.ok(segs[0].style.width.endsWith("%"));

    // Call strip dots: Cold prompt, Cache hit, Cache break -> 1 break + accessible summary
    const dotsWrap = cards[0].querySelector(".call-dots")!;
    assert.equal(dotsWrap.getAttribute("aria-hidden"), "true");
    const dots = cards[0].querySelectorAll(".cdot");
    assert.deepEqual(
      dots.map((d) => d.title),
      ["Cold prompt", "Cache hit", "Cache break"],
    );
    const dotSummary = cards[0].querySelector(".call-strip-row .visually-hidden");
    assert.equal(dotSummary?.textContent, "1 cache hit, 1 cache break, 1 cold prompt");
    assert.ok(cards[0].querySelector(".call-strip-row")?.textContent.includes("1 break"));

    // computeMonitorSignature ignores proxy_rss_bytes-only poll ticks and reacts to real stats changes
    const sigBase = computeMonitorSignature(makeState({ snapshot: snap }));
    const sigRssOnly = computeMonitorSignature(
      makeState({
        snapshot: {
          ...snap,
          proxy_rss_bytes: 42_000_000,
          stats: { ...snap.stats!, proxy_rss_bytes: 42_000_000 },
        },
      }),
    );
    const sigNewReq = computeMonitorSignature(
      makeState({
        snapshot: {
          ...snap,
          stats: {
            ...snap.stats!,
            usage: { ...snap.stats!.usage, requests: snap.stats!.usage.requests + 1 },
          },
        },
      }),
    );
    assert.equal(sigBase, sigRssOnly);
    assert.notEqual(sigBase, sigNewReq);

    // 3 tiles: Session cache, Input, Output
    const tiles = cards[0].querySelectorAll(".tile");
    assert.equal(tiles.length, 3);
    assert.deepEqual(
      tiles.map((t) => t.querySelector(".tile-k")?.textContent),
      ["Session cache", "Input", "Output"],
    );

    // List price footer
    const costVal = cards[0].querySelector(".cost-foot-val");
    assert.ok(costVal && costVal.textContent.startsWith("\u2248 $"));

    // Ranked breakdown rows with mixed AI Studio (tag-brand) and Antigravity (tag) badges
    const tags = cards[1].querySelectorAll(".bk-name .tag");
    assert.equal(tags.length, 2);
    assert.equal(tags[0].textContent, "AI Studio");
    assert.equal(tags[0].className, "tag tag-brand");
    assert.equal(tags[1].textContent, "Antigravity");
    assert.equal(tags[1].className, "tag");
  });

  it("renders provider pill tags, model_display_name, and static placeholder fallback in Usage by model", () => {
    const base = loadActiveSnapshot();
    const snap: SettingsSnapshot = {
      ...base,
      stats: {
        ...base.stats!,
        recent_calls: [
          {
            timestamp_ms: 1747300400000,
            provider: "openai-codex",
            source: "proxy",
            model: "gpt-6.2-sol",
            input_tokens: 89_000,
            cached_tokens: 58_740,
            output_tokens: 245,
            reasoning_tokens: 0,
            latency_ms: 500,
            cold_start: false,
            cache_break: false,
          },
        ],
        by_model: [
          {
            provider: "antigravity",
            source: "builtin",
            model: "MODEL_PLACEHOLDER_M530",
            requests: 60,
            input_tokens: 7_300_000,
            cached_tokens: 6_789_000,
            output_tokens: 39_000,
            reasoning_tokens: 0,
            total_latency_ms: 12000,
          },
          {
            provider: "gemini",
            source: "proxy",
            model: "gemini-4-neon",
            requests: 37,
            input_tokens: 4_400_000,
            cached_tokens: 1_892_000,
            output_tokens: 9_800,
            reasoning_tokens: 0,
            total_latency_ms: 9000,
          },
          {
            provider: "openai-codex",
            source: "proxy",
            model: "gpt-6.2-sol",
            requests: 3,
            input_tokens: 89_000,
            cached_tokens: 58_740,
            output_tokens: 245,
            reasoning_tokens: 0,
            total_latency_ms: 1500,
          },
          {
            provider: "antigravity",
            source: "builtin",
            model: "gemini-5-nova",
            model_display_name: "Gemini 5 Nova (High)",
            requests: 1,
            input_tokens: 1_000,
            cached_tokens: 0,
            output_tokens: 50,
            reasoning_tokens: 0,
            total_latency_ms: 300,
          },
        ],
      },
    };
    const host = env.document.createElement("div");
    host.append(buildMonitorView(makeState({ snapshot: snap })) as unknown as FakeElement);
    const cards = host.querySelectorAll(".card");
    const rowTags = cards[1].querySelectorAll(".bk-name .tag").map((el) => el.textContent);
    const rowNames = cards[1].querySelectorAll(".bk-name .bk-name-text").map((el) => el.textContent);
    assert.deepEqual(rowTags, ["Antigravity", "AI Studio", "OpenAI Codex", "Antigravity"]);
    assert.deepEqual(rowNames, [
      "Gemini 4 Hydrogen",
      "Gemini 4 Neon",
      "GPT-6.2 Sol",
      "Gemini 5 Nova (High)",
    ]);
  });

  it("scales Latest prompt hero and bar to live compaction window with fallback, overflow, and zero-token guards", () => {
    const base = loadActiveSnapshot();
    const makeNeonSnap = (
      maxContextTokens: number | null,
      inputTokens: number,
      cachedTokens: number,
      outputTokens: number,
    ): SettingsSnapshot => ({
      ...base,
      ls_telemetry_active: true,
      stats: {
        usage: {
          requests: 1,
          prompt_tokens: inputTokens,
          cached_tokens: cachedTokens,
          output_tokens: outputTokens,
          reasoning_tokens: 0,
          cache_hit_ratio: inputTokens > 0 ? cachedTokens / inputTokens : 0,
        },
        proxy_rss_bytes: 25_165_824,
        max_context_tokens: maxContextTokens,
        recent_calls: [
          {
            timestamp_ms: 1747300300000,
            provider: "antigravity",
            source: "builtin",
            model: "gemini-4-neon",
            input_tokens: inputTokens,
            cached_tokens: cachedTokens,
            output_tokens: outputTokens,
            reasoning_tokens: 0,
            latency_ms: 400,
            cold_start: false,
            cache_break: false,
          },
        ],
        by_model: [
          {
            provider: "antigravity",
            source: "builtin",
            model: "gemini-4-neon",
            requests: 1,
            input_tokens: inputTokens,
            cached_tokens: cachedTokens,
            output_tokens: outputTokens,
            reasoning_tokens: 0,
            total_latency_ms: 400,
          },
        ],
      },
    });

    // 1. Live compaction window (512K) overrides Gemini 4 Neon 1M model window and scales bar with headroom
    const host1 = env.document.createElement("div");
    host1.append(
      buildMonitorView(makeState({ snapshot: makeNeonSnap(512_000, 77_000, 64_000, 3_000) })) as unknown as FakeElement,
    );
    assert.ok(host1.textContent.includes("77K"));
    assert.ok(host1.textContent.includes("of 512K"));
    assert.ok(host1.textContent.includes("15%"));
    const bars1 = host1.querySelectorAll(".prop-bar");
    assert.equal(bars1.length, 2);
    const promptSegs1 = bars1[0].querySelectorAll(".prop-seg");
    assert.deepEqual(
      promptSegs1.map((s) => s.style.width),
      ["12.5%", "2.5%", "0.6%"],
    );
    // costBar (second .prop-bar) stays 100%-normalized
    const costSegs1 = bars1[1].querySelectorAll(".prop-seg");
    const costSum1 = costSegs1.reduce((acc, s) => acc + Number.parseFloat(s.style.width), 0);
    assert.ok(Math.abs(costSum1 - 100) <= 0.2, `expected costBar ~100%, got ${costSum1}`);

    // 2. Fallback to model context window (1M) on null, 0, -1, and NaN
    for (const invalid of [null, 0, -1, Number.NaN]) {
      const hostFb = env.document.createElement("div");
      hostFb.append(
        buildMonitorView(makeState({ snapshot: makeNeonSnap(invalid, 77_000, 64_000, 3_000) })) as unknown as FakeElement,
      );
      assert.ok(hostFb.textContent.includes("of 1M"));
      assert.ok(hostFb.textContent.includes("8%"));
    }

    // 3. Overflow clamping when latestCached + latestNew + latestOutput > maxCtx
    const hostOver = env.document.createElement("div");
    hostOver.append(
      buildMonitorView(makeState({ snapshot: makeNeonSnap(100_000, 100_000, 80_000, 20_000) })) as unknown as FakeElement,
    );
    assert.ok(hostOver.textContent.includes("of 100K"));
    assert.ok(hostOver.textContent.includes("100%"));
    const overSegs = hostOver.querySelector(".prop-bar")?.querySelectorAll(".prop-seg") ?? [];
    assert.deepEqual(
      overSegs.map((s) => s.style.width),
      ["66.7%", "16.7%", "16.7%"],
    );

    // 4. Zero tokens: all 3 segments stay in DOM at 0.0% with no NaN
    const hostZero = env.document.createElement("div");
    hostZero.append(
      buildMonitorView(makeState({ snapshot: makeNeonSnap(512_000, 0, 0, 0) })) as unknown as FakeElement,
    );
    const zeroSegs = hostZero.querySelector(".prop-bar")?.querySelectorAll(".prop-seg") ?? [];
    assert.deepEqual(
      zeroSegs.map((s) => s.style.width),
      ["0.0%", "0.0%", "0.0%"],
    );
  });

  it("renders main vs sub-agent two-tone stacked bars, conditional legend, tooltips, and meta copy", () => {
    const base = loadActiveSnapshot();

    // Case 1: No sub-agents -> header keeps "this session", single solid .bk-bar-fill, unchanged meta
    const mainOnlyHost = env.document.createElement("div");
    mainOnlyHost.append(buildMonitorView(makeState({ snapshot: base })) as unknown as FakeElement);
    const mainOnlyCard = mainOnlyHost.querySelectorAll(".card")[1];
    assert.equal(mainOnlyCard.querySelector(".sec-count")?.textContent, "this session");
    assert.equal(mainOnlyCard.querySelector(".bk-legend"), null);
    assert.equal(mainOnlyCard.querySelectorAll(".bk-bar-fill").length, 2);
    assert.equal(mainOnlyCard.querySelectorAll(".bk-bar-sub").length, 0);
    assert.equal(mainOnlyCard.querySelectorAll(".bk-bar-gap").length, 0);

    // Case 2: Sub-agent-only rows (plural & singular) -> conditional legend + lighter .bk-bar-sub + sub-agent meta
    const subOnlySnap: SettingsSnapshot = {
      ...base,
      stats: {
        ...base.stats!,
        usage: {
          requests: 51,
          prompt_tokens: 11_200_000,
          cached_tokens: 10_400_000,
          output_tokens: 41_000,
          reasoning_tokens: 0,
          cache_hit_ratio: 0.93,
        },
        by_model: [
          {
            provider: "antigravity",
            source: "builtin",
            model: "gemini-4-hydrogen",
            requests: 50,
            input_tokens: 11_100_000,
            cached_tokens: 10_323_000,
            output_tokens: 40_000,
            reasoning_tokens: 0,
            total_latency_ms: 15_000,
            subagent: {
              requests: 50,
              input_tokens: 11_100_000,
              cached_tokens: 10_323_000,
              output_tokens: 40_000,
              reasoning_tokens: 0,
            },
          },
          {
            provider: "antigravity",
            source: "builtin",
            model: "gemini-4-neon",
            requests: 1,
            input_tokens: 100_000,
            cached_tokens: 77_000,
            output_tokens: 1_000,
            reasoning_tokens: 0,
            total_latency_ms: 400,
            subagent: {
              requests: 1,
              input_tokens: 100_000,
              cached_tokens: 77_000,
              output_tokens: 1_000,
              reasoning_tokens: 0,
            },
          },
        ],
      },
    };
    const subOnlyHost = env.document.createElement("div");
    subOnlyHost.append(
      buildMonitorView(makeState({ snapshot: subOnlySnap })) as unknown as FakeElement,
    );
    const subOnlyCard = subOnlyHost.querySelectorAll(".card")[1];
    assert.equal(subOnlyCard.querySelector(".sec-count"), null);
    const legend = subOnlyCard.querySelector(".bk-legend");
    assert.ok(legend);
    assert.ok(legend.querySelector(".bk-legend-dot.is-main"));
    assert.ok(legend.querySelector(".bk-legend-dot.is-sub"));
    assert.ok(legend.textContent.includes("Main"));
    assert.ok(legend.textContent.includes("Sub-agents"));

    const subOnlyBars = subOnlyCard.querySelectorAll(".bk-bar");
    assert.equal(subOnlyBars.length, 2);
    assert.equal(subOnlyBars[0].querySelectorAll(".bk-bar-fill").length, 0);
    assert.equal(subOnlyBars[0].querySelectorAll(".bk-bar-gap").length, 0);
    const subSeg0 = subOnlyBars[0].querySelector(".bk-bar-sub");
    assert.ok(subSeg0);
    assert.ok(subSeg0.title.includes("Sub-agents:"));
    assert.ok(subSeg0.title.includes("50 calls"));

    const subOnlyMetas = subOnlyCard.querySelectorAll(".bk-meta").map((m) => m.textContent);
    assert.ok(subOnlyMetas[0].startsWith("50 sub-agent calls \u00b7 "));
    assert.ok(subOnlyMetas[1].startsWith("1 sub-agent call \u00b7 "));

    // Case 3: Mixed main + sub-agents (priced + unpriced) -> legend + main + gap + sub + mixed meta
    const mixedSnap: SettingsSnapshot = {
      ...base,
      stats: {
        ...base.stats!,
        usage: {
          requests: 70,
          prompt_tokens: 16_500_000,
          cached_tokens: 14_650_000,
          output_tokens: 59_000,
          reasoning_tokens: 0,
          cache_hit_ratio: 0.89,
        },
        by_model: [
          {
            provider: "gemini",
            source: "proxy",
            model: "gemini-4-hydrogen",
            requests: 68,
            input_tokens: 16_400_000,
            cached_tokens: 14_596_000,
            output_tokens: 58_000,
            reasoning_tokens: 0,
            total_latency_ms: 20_000,
            subagent: {
              requests: 50,
              input_tokens: 11_100_000,
              cached_tokens: 10_323_000,
              output_tokens: 40_000,
              reasoning_tokens: 0,
            },
          },
          {
            provider: "openai-codex",
            source: "proxy",
            model: "gpt-6.2-sol",
            requests: 2,
            input_tokens: 100_000,
            cached_tokens: 54_000,
            output_tokens: 1_000,
            reasoning_tokens: 0,
            total_latency_ms: 900,
            subagent: {
              requests: 1,
              input_tokens: 50_000,
              cached_tokens: 27_000,
              output_tokens: 500,
              reasoning_tokens: 0,
            },
          },
        ],
      },
    };
    const mixedHost = env.document.createElement("div");
    mixedHost.append(
      buildMonitorView(makeState({ snapshot: mixedSnap })) as unknown as FakeElement,
    );
    const mixedCard = mixedHost.querySelectorAll(".card")[1];
    assert.ok(mixedCard.querySelector(".bk-legend"));
    const mixedBars = mixedCard.querySelectorAll(".bk-bar");
    assert.equal(mixedBars.length, 2);

    // Priced mixed row (100% of priced session cost): main + gap + sub summing to 100%
    const mainSeg0 = mixedBars[0].querySelector(".bk-bar-fill");
    const gapSeg0 = mixedBars[0].querySelector(".bk-bar-gap");
    const subMixSeg0 = mixedBars[0].querySelector(".bk-bar-sub");
    assert.ok(mainSeg0 && gapSeg0 && subMixSeg0);
    const sumPct0 =
      Number.parseFloat(mainSeg0.style.width) + Number.parseFloat(subMixSeg0.style.width);
    assert.ok(Math.abs(sumPct0 - 100) <= 0.2, `expected mixed priced bar ~100%, got ${sumPct0}`);
    assert.ok(mainSeg0.title.includes("Main:"));
    assert.ok(mainSeg0.title.includes("18 calls"));
    assert.ok(subMixSeg0.title.includes("Sub-agents:"));
    assert.ok(subMixSeg0.title.includes("50 calls"));

    const mixedMetas = mixedCard.querySelectorAll(".bk-meta").map((m) => m.textContent);
    assert.ok(mixedMetas[0].startsWith("68 calls (18 main \u00b7 50 sub-agents) \u00b7 "));
    assert.ok(mixedMetas[1].startsWith("2 calls (1 main \u00b7 1 sub-agent) \u00b7 "));

    // Unpriced-only mixed session splits by token share (50% / 50% of 100%)
    const unpricedMixedSnap: SettingsSnapshot = {
      ...base,
      stats: {
        ...base.stats!,
        usage: {
          requests: 2,
          prompt_tokens: 100_000,
          cached_tokens: 54_000,
          output_tokens: 1_000,
          reasoning_tokens: 0,
          cache_hit_ratio: 0.54,
        },
        by_model: [mixedSnap.stats!.by_model[1]],
      },
    };
    const unpricedHost = env.document.createElement("div");
    unpricedHost.append(
      buildMonitorView(makeState({ snapshot: unpricedMixedSnap })) as unknown as FakeElement,
    );
    const unpricedBar = unpricedHost.querySelectorAll(".card")[1].querySelector(".bk-bar")!;
    const uMain = unpricedBar.querySelector(".bk-bar-fill");
    const uGap = unpricedBar.querySelector(".bk-bar-gap");
    const uSub = unpricedBar.querySelector(".bk-bar-sub");
    assert.ok(uMain && uGap && uSub);
    assert.equal(uMain.style.width, "50.0%");
    assert.equal(uSub.style.width, "50.0%");
    assert.ok(uMain.title.includes("tokens (1 call)"));
    assert.ok(uSub.title.includes("tokens (1 call)"));

    // computeMonitorSignature changes when subagent field changes
    const noSubRow = { ...mixedSnap.stats!.by_model[0], subagent: null };
    const sigWithSub = computeMonitorSignature(makeState({ snapshot: mixedSnap }));
    const sigWithoutSub = computeMonitorSignature(
      makeState({
        snapshot: {
          ...mixedSnap,
          stats: { ...mixedSnap.stats!, by_model: [noSubRow, mixedSnap.stats!.by_model[1]] },
        },
      }),
    );
    assert.notEqual(sigWithSub, sigWithoutSub);
  });
});

describe("buildUsageView filters, states & breakdowns", () => {
  const makeActions = () => {
    const events: string[] = [];
    return {
      events,
      actions: {
        onSelectRange: (r: string) => events.push(`range:${r}`),
        onSelectUnit: (u: string) => events.push(`unit:${u}`),
        onSelectGroup: (g: string) => events.push(`group:${g}`),
      },
    };
  };

  it("renders loading, error, and range-specific empty states and wires filter callbacks", () => {
    const { actions, events } = makeActions();

    const loadingHost = env.document.createElement("div");
    loadingHost.append(
      buildUsageView(makeState({ usageLoading: true, usageSummary: null }), actions) as unknown as FakeElement,
    );
    assert.ok(loadingHost.textContent.includes("Loading usage history"));

    // Click Today and Tokens filters
    const filterButtons = loadingHost.querySelectorAll(".usage-filters button");
    assert.equal(filterButtons.length, 5);
    filterButtons[0].click(); // Today -> 1d
    filterButtons[2].click(); // 30 days -> 30d
    filterButtons[4].click(); // Tokens -> tok
    assert.deepEqual(events, ["range:1d", "range:30d", "unit:tok"]);

    const errHost = env.document.createElement("div");
    errHost.append(
      buildUsageView(makeState({ usageError: "DB locked", usageSummary: null }), actions) as unknown as FakeElement,
    );
    assert.ok(errHost.textContent.includes("Unable to load usage history"));
    assert.ok(errHost.textContent.includes("DB locked"));

    for (const [range, phrase] of [
      ["1d", "today"],
      ["7d", "in the last 7 days"],
      ["30d", "in the last 30 days"],
    ] as const) {
      const emptyHost = env.document.createElement("div");
      emptyHost.append(
        buildUsageView(makeState({ usageRange: range, usageSummary: null }), actions) as unknown as FakeElement,
      );
      assert.ok(emptyHost.textContent.includes("No usage recorded yet"));
      assert.ok(emptyHost.textContent.includes(phrase));
    }
  });

  it("renders populated $ vs Tokens hero, 7 daily bars, and Model vs Provider breakdown", () => {
    const usage = loadUsage7d();
    const { actions, events } = makeActions();

    // $ + Model view
    const usdHost = env.document.createElement("div");
    usdHost.append(
      buildUsageView(
        makeState({ usageRange: "7d", usageUnit: "usd", usageGroup: "model", usageSummary: usage }),
        actions,
      ) as unknown as FakeElement,
    );
    assert.ok(usdHost.querySelector(".hero-val")?.textContent.startsWith("\u2248 $"));
    assert.equal(usdHost.querySelector(".hero-sub")?.textContent, "est. list price");
    assert.equal(usdHost.querySelectorAll(".daily-bars .dbar-col").length, 7);
    assert.equal(usdHost.querySelectorAll(".daily-bars .dbar-stack.is-empty").length, 5);
    assert.ok(usdHost.querySelector(".daily-chart-hdr")?.textContent.includes("Peak May 15"));
    assert.ok(usdHost.querySelectorAll(".daily-axis span").length >= 2);
    assert.equal(usdHost.querySelectorAll(".breakdown-list .bk-row-top").length, 3);

    // Semantic card headings (no redundant region aria-label), accessible chart table, and data-usage-ctrl tags
    const usageCards = usdHost.querySelectorAll("section.card");
    assert.equal(usageCards.length, 2);
    assert.equal(usageCards[0].getAttribute("aria-label"), null);
    assert.equal(usageCards[1].getAttribute("aria-label"), null);
    assert.equal(usageCards[0].querySelector("h2.visually-hidden")?.textContent, "Usage summary");
    assert.equal(usageCards[1].querySelector("h2.visually-hidden")?.textContent, "Usage breakdown");
    const chartTable = usdHost.querySelector("table.visually-hidden");
    assert.ok(chartTable, "Expected visually-hidden table alternative for .daily-bars");
    assert.equal(chartTable.querySelector("caption")?.textContent, "Last 7 days usage");
    assert.equal(chartTable.querySelectorAll("tbody tr").length, 7);
    assert.deepEqual(
      usdHost.querySelectorAll("button[data-usage-ctrl]").map((b) => b.getAttribute("data-usage-ctrl")),
      ["range:1d", "range:7d", "range:30d", "unit:usd", "unit:tok", "group:model", "group:provider"],
    );
    assert.equal(
      computeUsageSignature(makeState({ usageRange: "7d", usageUnit: "usd", usageGroup: "model", usageSummary: usage })),
      computeUsageSignature(makeState({ usageRange: "7d", usageUnit: "usd", usageGroup: "model", usageSummary: usage })),
    );
    assert.notEqual(
      computeUsageSignature(makeState({ usageRange: "7d", usageUnit: "usd", usageGroup: "model", usageSummary: usage })),
      computeUsageSignature(makeState({ usageRange: "7d", usageUnit: "tok", usageGroup: "model", usageSummary: usage })),
    );

    // Click Provider switcher
    const groupButtons = usdHost.querySelectorAll('[aria-label="Breakdown grouping"] button');
    assert.equal(groupButtons[0].getAttribute("aria-pressed"), "true");
    assert.equal(groupButtons[1].getAttribute("aria-pressed"), "false");
    groupButtons[1].click();
    assert.deepEqual(events, ["group:provider"]);

    // Tokens + Provider view
    const tokHost = env.document.createElement("div");
    tokHost.append(
      buildUsageView(
        makeState({ usageRange: "7d", usageUnit: "tok", usageGroup: "provider", usageSummary: usage }),
        actions,
      ) as unknown as FakeElement,
    );
    assert.equal(tokHost.querySelector(".hero-val")?.textContent, "4.3K");
    assert.equal(tokHost.querySelector(".hero-sub")?.textContent, "total tokens");
    const provNames = tokHost
      .querySelectorAll(".breakdown-list .bk-name")
      .map((n) => n.textContent);
    assert.deepEqual(provNames, ["Google AI Studio", "Antigravity"]);

    // Vertex Claude & Vertex OpenAI provider names
    const vertexUsage: UsageSummary = {
      ...usage,
      by_provider: [
        { ...usage.by_provider[0], key: "vertex_claude", provider: "vertex_claude", source: null },
        { ...usage.by_provider[1], key: "vertex_openai", provider: "vertex_openai", source: null },
      ],
    };
    const vertexHost = env.document.createElement("div");
    vertexHost.append(
      buildUsageView(
        makeState({ usageRange: "7d", usageUnit: "tok", usageGroup: "provider", usageSummary: vertexUsage }),
        actions,
      ) as unknown as FakeElement,
    );
    assert.deepEqual(
      vertexHost.querySelectorAll(".breakdown-list .bk-name").map((n) => n.textContent),
      ["Vertex Claude", "Vertex OpenAI"],
    );
  });
});

describe("createSettingsView API key lifecycle, enabled_models, Advanced plan & Unix/Windows proxy", () => {
  const makeHarness = () => {
    const container = env.document.createElement("div");
    env.document.body.append(container);
    const tooltip = createTooltipController(container as unknown as HTMLElement);
    const calls: string[] = [];
    const view = createSettingsView(
      {
        onOpenExternalUrl: (url) => calls.push(`open:${url}`),
        onDraftChange: (d) => calls.push(`draft:${d}`),
        onOpenGeminiKeyEdit: () => calls.push("openGeminiKeyEdit"),
        onCancelGeminiKeyEdit: () => calls.push("cancelGeminiKeyEdit"),
        onSaveKey: () => calls.push("saveKey"),
        onTestConnection: () => calls.push("testConn"),
        onToggleClearConfirm: () => calls.push("toggleClear"),
        onCancelClear: () => calls.push("cancelClear"),
        onClearKey: () => calls.push("clearKey"),
        onToggleModel: (id) => calls.push(`toggleModel:${id}`),
        onRestoreDefaultModels: () => calls.push("restoreDefaults"),
        onStartCodexLogin: () => calls.push("codexStart"),
        onOpenPendingCodexUrl: () => calls.push("codexOpenPending"),
        onCopyPendingCodexUrl: () => calls.push("codexCopyPending"),
        onCancelCodexLogin: () => calls.push("codexCancel"),
        onToggleCodexFallbackOpen: (o) => calls.push(`codexFallbackOpen:${o}`),
        onCodexManualInputChange: (d) => calls.push(`codexDraft:${d}`),
        onCompleteCodexLogin: () => calls.push("codexComplete"),
        onTestCodexConnection: () => calls.push("codexTest"),
        onToggleCodexSignOutConfirm: () => calls.push("codexToggleSignOut"),
        onCancelCodexSignOut: () => calls.push("codexCancelSignOut"),
        onDisconnectCodex: () => calls.push("codexDisconnect"),
        onToggleCodexModel: (id) => calls.push(`codexToggleModel:${id}`),
        onRestoreDefaultCodexModels: () => calls.push("codexRestoreDefaults"),
        onSelectAccountPlan: (p) => calls.push(`plan:${p}`),
        onToggleAdvOpen: (o) => calls.push(`advOpen:${o}`),
        onRecheckIntegration: () => calls.push("recheck"),
        onOpenDisableProxyConfirm: () => calls.push("openDisableConfirm"),
        onCancelDisableProxy: () => calls.push("cancelDisable"),
        onDisableProxy: () => calls.push("disableProxy"),
        onEnableProxy: () => calls.push("enableProxy"),
      },
      tooltip,
    );
    container.append(view.root as unknown as FakeElement);
    return { view, root: view.root as unknown as FakeElement, calls, tooltip };
  };

  it("drives symmetric compact Google AI Studio lifecycle: unconfigured key-row, configured header Test/Replace/Clear, Escape cancel, focus guard, and 2-click Clear", () => {
    const { view, root, calls, tooltip } = makeHarness();
    const offline = loadOfflineSnapshot();

    // Unconfigured: "Not set", Get API key visible, Test/Replace/Clear hidden, inline key-row open, Enabled models hidden
    view.update(makeState({ snapshot: offline }));
    assert.equal(root.querySelector("h2.sec-kicker")?.textContent, "Model providers");
    const geminiCard = root.querySelector('[data-provider="gemini"]')!;
    assert.equal(geminiCard.getAttribute("aria-label"), null);
    assert.equal(geminiCard.querySelector("h3.prov-title")?.textContent, "Google AI Studio");
    assert.equal(geminiCard.querySelector("h4.sec-t")?.textContent, "Enabled models");
    const keyRow = geminiCard.querySelector(".key-row")!;
    const keyLabel = keyRow.querySelector('label.visually-hidden[for="gemini-api-key"]');
    assert.ok(keyLabel, "Expected programmatic visually-hidden <label for=gemini-api-key>");
    assert.equal(keyLabel.textContent, "Google AI Studio API key");
    const keyInput = geminiCard.querySelector("#gemini-api-key")!;
    assert.equal(keyInput.getAttribute("aria-label"), null);
    const headerBtns = geminiCard.querySelectorAll(".prov-actions button");
    const getKeyBtn = headerBtns.find((b) => b.textContent === "Get API key \u2197")!;
    const testBtn = headerBtns.find((b) => b.textContent === "Test")!;
    const replaceBtn = headerBtns.find((b) => b.textContent === "Replace")!;
    const clearBtn = headerBtns.find((b) => b.textContent === "Clear")!;
    const keyBtns = keyRow.querySelectorAll("button");
    const saveBtn = keyBtns.find((b) => b.textContent === "Save")!;
    const cancelEditBtn = keyBtns.find((b) => b.textContent === "Cancel")!;
    const modSec = geminiCard.querySelector(".divider")!;

    assert.ok(geminiCard.textContent.includes("Not set"));
    assert.equal(getKeyBtn.hidden, false);
    assert.equal(testBtn.hidden, true);
    assert.equal(replaceBtn.hidden, true);
    assert.equal(clearBtn.hidden, true);
    assert.equal(keyRow.hidden, false);
    assert.equal(saveBtn.disabled, true);
    assert.equal(cancelEditBtn.hidden, true);
    assert.equal(modSec.hidden, true);

    // Get API key external link
    getKeyBtn.click();
    assert.equal(calls.at(-1), "open:https://aistudio.google.com/api-keys");

    // Draft typing fires onDraftChange, enables Save, Enter fires onSaveKey
    keyInput.focus();
    keyInput.value = "synthetic-draft-key";
    keyInput.dispatchEvent(new FakeEvent("input", { bubbles: true }));
    assert.equal(calls.at(-1), "draft:synthetic-draft-key");

    view.update(makeState({ snapshot: offline, apiKeyDraft: "synthetic-draft-key" }));
    assert.equal(saveBtn.disabled, false);
    // IME composition Enter must NOT submit
    const callsBeforeIme = calls.length;
    keyInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter", isComposing: true }));
    keyInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter", keyCode: 229 }));
    assert.equal(calls.length, callsBeforeIme);

    keyInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter" }));
    assert.equal(calls.at(-1), "saveKey");

    // While focused, background update must not clobber active typing
    view.update(makeState({ snapshot: loadActiveSnapshot(), apiKeyDraft: "" }));
    assert.equal(keyInput.value, "synthetic-draft-key");

    // Explicit clearKeyInputDom() clears value
    view.clearKeyInputDom();
    assert.equal(keyInput.value, "");

    // Configured steady state: masked hint shown, key-row collapsed, header shows Test/Replace/Clear, Enabled models visible
    view.update(makeState({ snapshot: loadActiveSnapshot(), apiKeyDraft: "" }));
    assert.ok(geminiCard.textContent.includes("AIza...1234"));
    assert.equal(keyRow.hidden, true);
    assert.equal(getKeyBtn.hidden, true);
    assert.equal(testBtn.hidden, false);
    assert.equal(testBtn.disabled, false);
    assert.equal(replaceBtn.hidden, false);
    assert.equal(clearBtn.hidden, false);
    assert.equal(modSec.hidden, false);

    testBtn.click();
    assert.equal(calls.at(-1), "testConn");

    // Non-error keyFeedback ("Connected · Google AI Studio", "Testing connection…") renders inline
    // inside .prov-id right next to the API key tag, keeping the below-row .status-line hidden.
    const geminiProvId = geminiCard.querySelector(".prov-id")!;
    const geminiInlineStatus = geminiProvId.querySelector(".prov-inline-status")!;
    const geminiStatusLine = geminiCard.querySelector(".status-line")!;
    assert.ok(geminiInlineStatus, "expected .prov-inline-status inside .prov-id");
    assert.equal(geminiInlineStatus.hidden, true);

    view.update(
      makeState({
        snapshot: loadActiveSnapshot(),
        keyFeedback: { tone: "ok", text: "Connected \u00b7 Google AI Studio" },
      }),
    );
    assert.equal(geminiInlineStatus.hidden, false);
    assert.equal(geminiInlineStatus.className, "prov-inline-status ok");
    assert.equal(geminiInlineStatus.textContent, "Connected \u00b7 Google AI Studio");
    assert.equal(geminiStatusLine.hidden, true);

    // Error keyFeedback renders on the full-width .status-line below instead of the header inline slot
    view.update(
      makeState({
        snapshot: loadActiveSnapshot(),
        keyFeedback: { tone: "err", text: "Invalid API key (400)" },
      }),
    );
    assert.equal(geminiInlineStatus.hidden, true);
    assert.equal(geminiStatusLine.hidden, false);
    assert.equal(geminiStatusLine.className, "status-line err");
    assert.equal(geminiStatusLine.textContent, "Invalid API key (400)");

    // Clearing keyFeedback (e.g. after auto-dismiss) hides both
    view.update(makeState({ snapshot: loadActiveSnapshot(), keyFeedback: null }));
    assert.equal(geminiInlineStatus.hidden, true);
    assert.equal(geminiStatusLine.hidden, true);

    // Replace opens inline key-row with Cancel + swaps header to Get API key; Escape or Cancel closes
    replaceBtn.click();
    assert.equal(calls.at(-1), "openGeminiKeyEdit");
    view.update(
      makeState({ snapshot: loadActiveSnapshot(), geminiKeyEditOpen: true, apiKeyDraft: "" }),
    );
    assert.equal(keyRow.hidden, false);
    assert.equal(getKeyBtn.hidden, false);
    assert.equal(testBtn.hidden, true);
    assert.equal(replaceBtn.hidden, true);
    assert.equal(clearBtn.hidden, true);
    assert.equal(cancelEditBtn.hidden, false);
    keyInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Escape" }));
    assert.equal(calls.at(-1), "cancelGeminiKeyEdit");
    cancelEditBtn.click();
    assert.equal(calls.at(-1), "cancelGeminiKeyEdit");

    // 2-click Clear confirm
    view.update(makeState({ snapshot: loadActiveSnapshot(), geminiKeyEditOpen: false }));
    clearBtn.click();
    assert.equal(calls.at(-1), "toggleClear");
    view.update(makeState({ snapshot: loadActiveSnapshot(), clearConfirmOpen: true }));
    const clearRow = geminiCard.querySelector(".confirm-row")!;
    assert.equal(clearRow.hidden, false);
    const [confirmClearBtn, cancelClearBtn] = clearRow.querySelectorAll("button");
    confirmClearBtn.click();
    cancelClearBtn.click();
    assert.deepEqual(calls.slice(-2), ["clearKey", "cancelClear"]);

    tooltip.destroy();
  });

  it("renders 5 enabled_models checkboxes, [] pause notice hiding modelFeedback, and Restore defaults", () => {
    const { view, root, calls, tooltip } = makeHarness();
    const active = loadActiveSnapshot();

    view.update(
      makeState({
        snapshot: active,
        modelFeedback: { tone: "ok", text: "Saved" },
      }),
    );
    const geminiCard = root.querySelector('[data-provider="gemini"]')!;
    const chips = geminiCard.querySelectorAll(".m-chip");
    assert.equal(chips.length, 5);
    assert.equal(chips[0].getAttribute("role"), "checkbox");
    assert.equal(chips[0].getAttribute("aria-checked"), "true");
    assert.equal(chips[0].getAttribute("aria-describedby"), null);
    assert.equal(chips[3].getAttribute("aria-checked"), "false");
    assert.equal(chips[3].getAttribute("aria-describedby"), null);
    chips[3].click();
    assert.equal(calls.at(-1), "toggleModel:gemini-4-argon");

    const zeroBox = geminiCard.querySelector(".zero-models-note")!;
    assert.equal(zeroBox.hidden, true);

    // 0-models pause state
    const pausedSnap: SettingsSnapshot = {
      ...active,
      config: {
        ...active.config,
        providers: {
          ...active.config.providers,
          gemini: { ...active.config.providers.gemini, enabled_models: [] },
        },
      },
    };
    view.update(
      makeState({
        snapshot: pausedSnap,
        modelFeedback: { tone: "ok", text: "Should be hidden when 0 enabled" },
      }),
    );
    assert.ok(geminiCard.textContent.includes("0 enabled"));
    assert.equal(zeroBox.hidden, false);
    const statusLines = geminiCard.querySelectorAll(".status-line");
    assert.equal(statusLines[1].hidden, true);

    zeroBox.querySelector("button")!.click();
    assert.equal(calls.at(-1), "restoreDefaults");

    tooltip.destroy();
  });

  it("renders collapsed Advanced disclosure, Antigravity plan radio cards, Unix 2-click Disable/Re-enable/Re-check, and Windows hiding Local proxy", () => {
    const { view, root, calls, tooltip } = makeHarness();
    const active = loadActiveSnapshot();

    view.update(makeState({ snapshot: active }));
    const advDetails = root.querySelector("details.adv-details")!;
    assert.equal(advDetails.open, false);
    assert.equal(root.querySelector(".adv-hint")?.textContent, "Plan \u00b7 Local proxy");

    // Toggle disclosure open
    advDetails.open = true;
    advDetails.dispatchEvent(new FakeEvent("toggle", { bubbles: false }));
    assert.equal(calls.at(-1), "advOpen:true");

    // Antigravity plan radios: native <fieldset>/<legend>, explicit id/for, zero JS .sel class
    const planFieldset = root.querySelector("fieldset.plan-fieldset")!;
    assert.ok(planFieldset);
    assert.equal(planFieldset.querySelector("legend.field-lbl")?.textContent, "Antigravity plan");
    assert.equal(root.querySelector('[role="radiogroup"]'), null);
    const planLabels = planFieldset.querySelectorAll("label.pr-row");
    assert.equal(planLabels.length, 3);
    for (const lbl of planLabels) {
      assert.equal(lbl.classList.contains("sel"), false, "Must rely on CSS :has(:checked), not .sel");
    }
    const planRadios = root.querySelectorAll('input[name="antigravity-plan"]');
    assert.equal(planRadios.length, 3);
    assert.equal(planRadios[0].id, "antigravity-plan-auto");
    assert.equal(planLabels[0].getAttribute("for"), "antigravity-plan-auto");
    assert.equal(planRadios[0].checked, true);
    const autoBadge = root.querySelector(".pr-row .tag-brand")!;
    assert.equal(autoBadge.hidden, false);
    assert.equal(autoBadge.textContent, "Organization");

    // Select Personal radio
    planRadios[1].checked = true;
    planRadios[1].dispatchEvent(new FakeEvent("change", { bubbles: true }));
    assert.equal(calls.at(-1), "plan:personal");

    // Unix Local proxy: semantic <h3>, Active -> 2-click Disable proxy -> Confirm / Cancel
    const proxyWrap = root.querySelector(".adv-sep")!;
    assert.equal(proxyWrap.querySelector("h3.sec-t")?.textContent, "Local proxy");
    assert.equal(proxyWrap.hidden, false);
    assert.ok(proxyWrap.textContent.includes("Active"));
    const disableBtn = proxyWrap
      .querySelectorAll("button")
      .find((b) => b.textContent === "Disable proxy")!;
    assert.equal(disableBtn.hidden, false);
    disableBtn.click();
    assert.equal(calls.at(-1), "openDisableConfirm");

    view.update(makeState({ snapshot: active, advOpen: true, disableProxyConfirmOpen: true }));
    const disableConfirmRow = proxyWrap.querySelector(".confirm-row")!;
    assert.equal(disableConfirmRow.hidden, false);
    const [confirmDisableBtn, cancelDisableBtn] = disableConfirmRow.querySelectorAll("button");
    confirmDisableBtn.click();
    cancelDisableBtn.click();
    assert.deepEqual(calls.slice(-2), ["disableProxy", "cancelDisable"]);

    // Unix enabled && !hybrid_active -> "Reopen Antigravity" badge
    view.update(
      makeState({
        snapshot: {
          ...active,
          launcher_integration: active.launcher_integration
            ? { ...active.launcher_integration, state: "enabled", enabled: true, hybrid_active: false }
            : null,
        },
      }),
    );
    assert.ok(proxyWrap.textContent.includes("Reopen Antigravity"));

    // Unix Disabled -> Re-enable proxy
    view.update(
      makeState({
        snapshot: {
          ...active,
          launcher_integration: active.launcher_integration
            ? { ...active.launcher_integration, state: "disabled", enabled: false }
            : null,
        },
      }),
    );
    const enableBtn = proxyWrap
      .querySelectorAll("button")
      .find((b) => b.textContent === "Re-enable proxy")!;
    assert.equal(enableBtn.hidden, false);
    enableBtn.click();
    assert.equal(calls.at(-1), "enableProxy");

    // Unix Manual -> Re-check
    view.update(makeState({ snapshot: loadOfflineSnapshot() }));
    const recheckBtn = proxyWrap
      .querySelectorAll("button")
      .find((b) => b.textContent === "Re-check")!;
    assert.equal(recheckBtn.hidden, false);
    recheckBtn.click();
    assert.equal(calls.at(-1), "recheck");

    // Windows hides Local proxy section and updates hint to "Plan"
    view.update(makeState({ snapshot: { ...active, platform: "windows" } }));
    assert.equal(proxyWrap.hidden, true);
    assert.equal(root.querySelector(".adv-hint")?.textContent, "Plan");

    tooltip.destroy();
  });

  it("drives OpenAI Codex card across signed-out, pending (sync open + copy + cancel + manual paste), and signed-in (Test, 2-click Sign out, 4 GPT-6.x chips, 0-models restore)", () => {
    assert.equal(formatCodexStatusBadge("pro", "j***@example.com"), "Pro \u00b7 j***@example.com");
    assert.equal(formatCodexStatusBadge("plus", null), "Plus");
    assert.equal(formatCodexStatusBadge(null, "acct...9999"), "acct...9999");
    assert.equal(formatCodexStatusBadge(null, null), "Signed in");

    const { view, root, calls, tooltip } = makeHarness();
    const base = loadActiveSnapshot();
    const codexCard = root.querySelector('[data-provider="codex"]')!;
    assert.ok(codexCard, "OpenAI Codex card must be stacked inside .provider-stack");
    assert.equal(codexCard.getAttribute("aria-label"), null);
    assert.equal(codexCard.querySelector("h3.prov-title")?.textContent, "OpenAI Codex");
    assert.equal(codexCard.querySelector("h4.sec-t")?.textContent, "Enabled models");

    // 1. Signed-out state: header has Sign in with ChatGPT button, Test & Sign out hidden
    view.update(makeState({ snapshot: base }));
    assert.ok(codexCard.textContent.includes("Not signed in"));
    const codexActions = codexCard.querySelector(".prov-actions")!;
    const pendingWrap = codexCard.querySelector(".codex-pending")!;
    const headerBtns = codexActions.querySelectorAll("button");
    const signInBtn = headerBtns.find((b) => b.textContent === "Sign in with ChatGPT \u2197")!;
    const codexTestBtn = headerBtns.find((b) => b.textContent === "Test")!;
    const codexSignOutBtn = headerBtns.find((b) => b.textContent === "Sign out")!;
    assert.equal(codexActions.hidden, false);
    assert.equal(signInBtn.hidden, false);
    assert.equal(codexTestBtn.hidden, true);
    assert.equal(codexSignOutBtn.hidden, true);
    assert.equal(pendingWrap.hidden, true);
    signInBtn.click();
    assert.equal(calls.at(-1), "codexStart");

    // 2. Pending state with authorize_url (sync Open, Copy link, Cancel, and manual fallback)
    view.update(
      makeState({
        snapshot: { ...base, codex_login_pending: true },
        codexAuthorizeUrl: "https://auth.openai.com/oauth/authorize?state=abc",
        codexManualFallbackOpen: true,
        codexManualInputDraft: "",
      }),
    );
    assert.equal(codexActions.hidden, true);
    assert.equal(pendingWrap.hidden, false);
    assert.ok(codexCard.textContent.includes("Signing in\u2026"));
    assert.ok(pendingWrap.textContent.includes("Complete sign-in in your browser\u2026"));

    const pendingBtns = pendingWrap.querySelectorAll(".act-row button");
    const openPageBtn = pendingBtns.find((b) => b.textContent === "Open sign-in page \u2197")!;
    const copyLinkBtn = pendingBtns.find((b) => b.textContent === "Copy link")!;
    const cancelLoginBtn = pendingBtns.find((b) => b.textContent === "Cancel")!;
    const completeBtn = pendingBtns.find((b) => b.textContent === "Complete sign-in")!;
    assert.equal(openPageBtn.hidden, false);
    assert.equal(copyLinkBtn.hidden, false);
    assert.equal(completeBtn.disabled, true);

    openPageBtn.click();
    copyLinkBtn.click();
    cancelLoginBtn.click();
    assert.deepEqual(calls.slice(-3), ["codexOpenPending", "codexCopyPending", "codexCancel"]);

    const manualLabel = codexCard.querySelector('label.visually-hidden[for="codex-callback-input"]');
    assert.ok(manualLabel, "Expected programmatic visually-hidden <label for=codex-callback-input>");
    const manualInput = codexCard.querySelector("#codex-callback-input")!;
    assert.equal(manualInput.getAttribute("aria-label"), null);
    manualInput.focus();
    manualInput.value = "http://localhost:1455/auth/callback?code=c1&state=s1";
    manualInput.dispatchEvent(new FakeEvent("input", { bubbles: true }));
    assert.equal(calls.at(-1), "codexDraft:http://localhost:1455/auth/callback?code=c1&state=s1");

    view.update(
      makeState({
        snapshot: { ...base, codex_login_pending: true },
        codexAuthorizeUrl: "https://auth.openai.com/oauth/authorize?state=abc",
        codexManualFallbackOpen: true,
        codexManualInputDraft: "http://localhost:1455/auth/callback?code=c1&state=s1",
      }),
    );
    assert.equal(completeBtn.disabled, false);
    // IME composition Enter must NOT complete login
    const codexCallsBeforeIme = calls.length;
    manualInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter", isComposing: true }));
    manualInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter", keyCode: 229 }));
    assert.equal(calls.length, codexCallsBeforeIme);

    manualInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter" }));
    assert.equal(calls.at(-1), "codexComplete");
    view.clearCodexManualInputDom();
    assert.equal(manualInput.value, "");

    // 3. Signed-in state: status badge + title tooltip, header Test + Sign out, inline 2-click Sign out, 4 GPT-6.x chips, 0-models Restore defaults
    const codexSignedInSnap: SettingsSnapshot = {
      ...base,
      codex_login_pending: false,
      config: {
        ...base.config,
        providers: {
          ...base.config.providers,
          codex: {
            ...base.config.providers.codex,
            authenticated: true,
            plan_type: "pro",
            account_hint: "j***@example.com",
            enabled_models: ["gpt-6.2-astra", "gpt-6.2-sol"],
          },
        },
      },
    };
    view.update(makeState({ snapshot: codexSignedInSnap }));
    assert.equal(codexActions.hidden, false);
    assert.equal(signInBtn.hidden, true);
    assert.equal(codexTestBtn.hidden, false);
    assert.equal(codexSignOutBtn.hidden, false);
    assert.equal(pendingWrap.hidden, true);
    assert.ok(codexCard.textContent.includes("Pro \u00b7 j***@example.com"));
    const codexTag = codexCard.querySelector(".codex-status-tag")!;
    assert.equal(codexTag.title, "Pro \u00b7 j***@example.com");

    assert.equal(codexTestBtn.disabled, false);
    codexTestBtn.click();
    assert.equal(calls.at(-1), "codexTest");

    // Codex non-error feedback when signed in renders inline next to the status tag; errors stay below
    const codexProvId = codexCard.querySelector(".prov-id")!;
    const codexInlineStatus = codexProvId.querySelector(".prov-inline-status")!;
    const codexStatusLine = codexCard.querySelector(".status-line")!;
    assert.ok(codexInlineStatus, "expected .prov-inline-status inside codex .prov-id");
    view.update(
      makeState({
        snapshot: codexSignedInSnap,
        codexFeedback: { tone: "ok", text: "Connected \u00b7 OpenAI Codex" },
      }),
    );
    assert.equal(codexInlineStatus.hidden, false);
    assert.equal(codexInlineStatus.className, "prov-inline-status ok");
    assert.equal(codexInlineStatus.textContent, "Connected \u00b7 OpenAI Codex");
    assert.equal(codexStatusLine.hidden, true);

    view.update(
      makeState({
        snapshot: codexSignedInSnap,
        codexFeedback: { tone: "err", text: "Token expired" },
      }),
    );
    assert.equal(codexInlineStatus.hidden, true);
    assert.equal(codexStatusLine.hidden, false);
    assert.equal(codexStatusLine.textContent, "Token expired");

    codexSignOutBtn.click();
    assert.equal(calls.at(-1), "codexToggleSignOut");
    view.update(makeState({ snapshot: codexSignedInSnap, codexSignOutConfirmOpen: true }));
    const signOutRow = codexCard.querySelector(".confirm-row")!;
    assert.equal(signOutRow.hidden, false);
    const [confirmSignOutBtn, cancelSignOutBtn] = signOutRow.querySelectorAll("button");
    confirmSignOutBtn.click();
    cancelSignOutBtn.click();
    assert.deepEqual(calls.slice(-2), ["codexDisconnect", "codexCancelSignOut"]);

    const codexChips = codexCard.querySelectorAll(".m-chip");
    assert.equal(codexChips.length, 4);
    assert.equal(codexChips[0].getAttribute("aria-checked"), "true");
    assert.equal(codexChips[1].getAttribute("aria-checked"), "true");
    assert.equal(codexChips[2].getAttribute("aria-checked"), "false");
    assert.equal(codexChips[3].getAttribute("aria-checked"), "false");
    codexChips[2].click();
    assert.equal(calls.at(-1), "codexToggleModel:gpt-6.1-terra");

    // 0-models state reveals Restore defaults
    const codexPausedSnap: SettingsSnapshot = {
      ...codexSignedInSnap,
      config: {
        ...codexSignedInSnap.config,
        providers: {
          ...codexSignedInSnap.config.providers,
          codex: { ...codexSignedInSnap.config.providers.codex, enabled_models: [] },
        },
      },
    };
    view.update(makeState({ snapshot: codexPausedSnap }));
    const codexZeroBox = codexCard.querySelector(".zero-models-note")!;
    assert.equal(codexZeroBox.hidden, false);
    codexZeroBox.querySelector("button")!.click();
    assert.equal(calls.at(-1), "codexRestoreDefaults");

    tooltip.destroy();
  });

  it("createTooltipController attaches aria-describedby dynamically only on the active chip", () => {
    const container = env.document.createElement("div");
    env.document.body.append(container);
    const chipA = env.document.createElement("button");
    const chipB = env.document.createElement("button");
    container.append(chipA, chipB);

    const tooltip = createTooltipController(container as unknown as HTMLElement);
    assert.equal(chipA.getAttribute("aria-describedby"), null);
    assert.equal(chipB.getAttribute("aria-describedby"), null);

    tooltip.show(chipA as unknown as HTMLElement, CURATED_MODEL_CATALOG[0]);
    assert.equal(chipA.getAttribute("aria-describedby"), "model-tooltip");
    assert.equal(chipB.getAttribute("aria-describedby"), null);

    // Moving to chipB clears chipA
    tooltip.show(chipB as unknown as HTMLElement, CURATED_MODEL_CATALOG[1]);
    assert.equal(chipA.getAttribute("aria-describedby"), null);
    assert.equal(chipB.getAttribute("aria-describedby"), "model-tooltip");

    // scheduleHide clears aria-describedby when timer fires
    tooltip.scheduleHide();
    assert.equal(chipB.getAttribute("aria-describedby"), "model-tooltip");
    env.flushTimers();
    assert.equal(chipB.getAttribute("aria-describedby"), null);

    // hideNow & destroy clear aria-describedby immediately
    tooltip.show(chipA as unknown as HTMLElement, CURATED_MODEL_CATALOG[0]);
    tooltip.hideNow();
    assert.equal(chipA.getAttribute("aria-describedby"), null);

    tooltip.show(chipB as unknown as HTMLElement, CURATED_MODEL_CATALOG[1]);
    tooltip.destroy();
    assert.equal(chipB.getAttribute("aria-describedby"), null);
  });
});

interface ParsedSvgNode {
  tag: string;
  attrs: Record<string, string>;
  children: ParsedSvgNode[];
}

function parseSvgXml(xml: string): ParsedSvgNode {
  const stack: ParsedSvgNode[] = [];
  let root: ParsedSvgNode | null = null;
  const tokenRe = /<\s*(\/?)([A-Za-z][A-Za-z0-9:-]*)([^>]*?)(\/?)\s*>/g;
  const attrRe = /([A-Za-z_:][A-Za-z0-9_.:-]*)\s*=\s*"([^"]*)"/g;

  for (const match of xml.matchAll(tokenRe)) {
    const [, closing, tag, rawAttrs, selfClosing] = match;
    if (closing) {
      stack.pop();
      continue;
    }
    const attrs: Record<string, string> = {};
    for (const am of rawAttrs.matchAll(attrRe)) {
      attrs[am[1]] = am[2];
    }
    const node: ParsedSvgNode = { tag, attrs, children: [] };
    if (stack.length > 0) {
      stack[stack.length - 1].children.push(node);
    } else {
      root = node;
    }
    if (!selfClosing) {
      stack.push(node);
    }
  }
  assert.ok(root !== null, "expected root <svg> element in plugin/assets/logo.svg");
  return root;
}

function fakeSvgToTree(node: FakeElement): ParsedSvgNode {
  const attrs: Record<string, string> = {};
  for (const name of node.getAttributeNames()) {
    const val = node.getAttribute(name);
    if (val !== null) attrs[name] = val;
  }
  const children = node.children.map((child) => fakeSvgToTree(child));
  return { tag: node.tagName, attrs, children };
}

describe("createBrandMarkSvg parity with plugin/assets/logo.svg", () => {
  it("renders the exact twin-arch aurora SVG structure and attributes as plugin/assets/logo.svg", () => {
    const logoUrl = new URL("../../plugin/assets/logo.svg", import.meta.url);
    const expected = parseSvgXml(fs.readFileSync(logoUrl, "utf8"));
    // Root <svg> in the UI is sized by CSS (.brand-mark svg) and decorative.
    delete expected.attrs.xmlns;
    delete expected.attrs.width;
    delete expected.attrs.height;
    expected.attrs["aria-hidden"] = "true";
    expected.attrs.focusable = "false";

    const actualEl = createBrandMarkSvg() as unknown as FakeElement;
    assert.equal(actualEl.namespaceURI, "http://www.w3.org/2000/svg");
    const actual = fakeSvgToTree(actualEl);

    assert.deepEqual(actual, expected);
  });
});
