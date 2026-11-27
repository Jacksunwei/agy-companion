import assert from "node:assert/strict";
import fs from "node:fs";
import { afterEach, beforeEach, describe, it } from "node:test";
import { buildBannerNode } from "../src/banners.ts";
import { createTooltipController } from "../src/dom.ts";
import { createFooterController } from "../src/footer.ts";
import { buildMonitorView } from "../src/monitor.ts";
import { createSettingsView } from "../src/settings.ts";
import {
  parseSettingsSnapshot,
  parseUsageSummary,
  type AppState,
  type SettingsSnapshot,
  type UsageSummary,
} from "../src/types.ts";
import { buildUsageView } from "../src/usage.ts";
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
    clearConfirmOpen: false,
    disableProxyConfirmOpen: false,
    advOpen: false,
    keyFeedback: null,
    modelFeedback: null,
    advFeedback: null,

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
    const calls = { recheck: 0, enableProxy: 0, toasts: [] as string[] };
    return {
      calls,
      actions: {
        onRecheck: () => {
          calls.recheck += 1;
        },
        onEnableProxy: () => {
          calls.enableProxy += 1;
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
    assert.equal(standalone.className, "banner info");
    assert.ok(standalone.textContent.includes("Preview mode"));

    const errBanner = buildBannerNode(
      makeState({ snapshot: null, loadError: "Sidecar timeout", refreshing: false }),
      actions,
    ) as unknown as FakeElement;
    assert.ok(errBanner);
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
    const container = env.document.createElement("div");
    const outside = env.document.createElement("div");
    env.document.body.append(container, outside);

    const ctrl = createFooterController(container as unknown as HTMLElement);
    ctrl.update(makeState());
    const trigger = (ctrl.footer as unknown as FakeElement).querySelector(".status-trigger")!;
    const popover = ctrl.popover as unknown as FakeElement;

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
    assert.equal(cards[0].getAttribute("aria-label"), "This session");
    assert.equal(cards[1].getAttribute("aria-label"), "Usage by model");

    // Latest prompt hero & proportional bar widths
    assert.ok(cards[0].textContent.includes("Latest prompt"));
    assert.ok(cards[0].textContent.includes("Gemini 4 Carbon"));
    const segs = cards[0].querySelectorAll(".prop-bar .prop-seg");
    assert.ok(segs.length >= 3);
    assert.ok(segs[0].style.width.endsWith("%"));

    // Call strip dots: Cold prompt, Cache hit, Cache break -> 1 break
    const dots = cards[0].querySelectorAll(".cdot");
    assert.deepEqual(
      dots.map((d) => d.title),
      ["Cold prompt", "Cache hit", "Cache break"],
    );
    assert.ok(cards[0].querySelector(".call-strip-row")?.textContent.includes("1 break"));

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

    // Ranked breakdown rows with mixed Gemini (tag-brand) and Antigravity (tag) badges
    const tags = cards[1].querySelectorAll(".bk-name .tag");
    assert.equal(tags.length, 2);
    assert.equal(tags[0].textContent, "Gemini");
    assert.equal(tags[0].className, "tag tag-brand");
    assert.equal(tags[1].textContent, "Antigravity");
    assert.equal(tags[1].className, "tag");
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
        onSaveKey: () => calls.push("saveKey"),
        onTestConnection: () => calls.push("testConn"),
        onToggleClearConfirm: () => calls.push("toggleClear"),
        onCancelClear: () => calls.push("cancelClear"),
        onClearKey: () => calls.push("clearKey"),
        onToggleModel: (id) => calls.push(`toggleModel:${id}`),
        onRestoreDefaultModels: () => calls.push("restoreDefaults"),
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

  it("drives write-only API key lifecycle, Enter save, 2-click Clear/Cancel, focus guard, and Get API key link", () => {
    const { view, root, calls, tooltip } = makeHarness();
    const offline = loadOfflineSnapshot();

    // Unconfigured: "Not set", Clear hidden, Save & Test disabled
    view.update(makeState({ snapshot: offline }));
    const keyInput = root.querySelector("#gemini-api-key")!;
    const actButtons = root.querySelectorAll(".act-row button");
    const [saveBtn, testBtn, clearBtn] = actButtons;
    assert.ok(root.textContent.includes("Not set"));
    assert.equal(saveBtn.disabled, true);
    assert.equal(testBtn.disabled, true);
    assert.equal(clearBtn.hidden, true);

    // Get API key external link
    const getKeyBtn = root.querySelector(".lnk")!;
    assert.equal(getKeyBtn.textContent, "Get API key \u2197");
    getKeyBtn.click();
    assert.equal(calls.at(-1), "open:https://aistudio.google.com/api-keys");

    // Draft typing fires onDraftChange, enables Save, keeps Test disabled, Enter fires onSaveKey
    keyInput.focus();
    keyInput.value = "synthetic-draft-key";
    keyInput.dispatchEvent(new FakeEvent("input", { bubbles: true }));
    assert.equal(calls.at(-1), "draft:synthetic-draft-key");

    view.update(makeState({ snapshot: offline, apiKeyDraft: "synthetic-draft-key" }));
    assert.equal(saveBtn.disabled, false);
    assert.equal(testBtn.disabled, true);
    keyInput.dispatchEvent(new FakeEvent("keydown", { bubbles: true, key: "Enter" }));
    assert.equal(calls.at(-1), "saveKey");

    // While focused, background update must not clobber active typing
    view.update(makeState({ snapshot: loadActiveSnapshot(), apiKeyDraft: "" }));
    assert.equal(keyInput.value, "synthetic-draft-key");

    // Explicit clearKeyInputDom() clears value
    view.clearKeyInputDom();
    assert.equal(keyInput.value, "");

    // Configured with empty draft: masked hint shown, Test enabled, Clear visible + 2-click confirm
    view.update(makeState({ snapshot: loadActiveSnapshot(), apiKeyDraft: "" }));
    assert.ok(root.textContent.includes("AIza...1234"));
    assert.equal(testBtn.disabled, false);
    assert.equal(clearBtn.hidden, false);
    testBtn.click();
    assert.equal(calls.at(-1), "testConn");

    clearBtn.click();
    assert.equal(calls.at(-1), "toggleClear");
    view.update(makeState({ snapshot: loadActiveSnapshot(), clearConfirmOpen: true }));
    const clearRow = root.querySelector(".confirm-row")!;
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
    const chips = root.querySelectorAll(".m-chip");
    assert.equal(chips.length, 5);
    assert.equal(chips[0].getAttribute("role"), "checkbox");
    assert.equal(chips[0].getAttribute("aria-checked"), "true");
    assert.equal(chips[3].getAttribute("aria-checked"), "false");
    chips[3].click();
    assert.equal(calls.at(-1), "toggleModel:gemini-4-argon");

    const zeroBox = root.querySelector(".zero-models-note")!;
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
    assert.ok(root.textContent.includes("0 enabled"));
    assert.equal(zeroBox.hidden, false);
    const statusLines = root.querySelectorAll(".status-line");
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

    // Antigravity plan radios: Auto selected with resolved "Organization" badge
    const planRadios = root.querySelectorAll('input[name="antigravity-plan"]');
    assert.equal(planRadios.length, 3);
    assert.equal(planRadios[0].checked, true);
    const autoBadge = root.querySelector(".pr-row .tag-brand")!;
    assert.equal(autoBadge.hidden, false);
    assert.equal(autoBadge.textContent, "Organization");

    // Select Personal radio
    planRadios[1].checked = true;
    planRadios[1].dispatchEvent(new FakeEvent("change", { bubbles: true }));
    assert.equal(calls.at(-1), "plan:personal");

    // Unix Local proxy: Active -> 2-click Disable proxy -> Confirm / Cancel
    const proxyWrap = root.querySelector(".adv-sep")!;
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
});
