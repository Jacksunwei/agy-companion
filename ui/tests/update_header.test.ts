import assert from "node:assert/strict";
import fs from "node:fs";
import { afterEach, beforeEach, describe, it } from "node:test";
import { createFooterController } from "../src/footer.ts";
import { callSidecarApi } from "../src/transport.ts";
import {
  compareSemVer,
  parseSettingsSnapshot,
  parseUpdateStatusSnapshot,
  type AppState,
  type HostPlatform,
  type SettingsSnapshot,
  type UpdateStatusSnapshot,
} from "../src/types.ts";
import {
  UPGRADE_DISCONNECT_GRACE_MS,
  UPGRADE_POLL_INTERVAL_MS,
  createUpdateHeaderController,
  createUpgradePoller,
} from "../src/update_header.ts";
import { FakeElement, FakeEvent, installFakeDom, type FakeDomEnv } from "./fake_dom.ts";

function loadFixture<T>(name: string): T {
  const url = new URL(`./fixtures/${name}`, import.meta.url);
  return JSON.parse(fs.readFileSync(url, "utf8")) as T;
}

function loadActiveSnapshot(overrides: Partial<SettingsSnapshot> = {}): SettingsSnapshot {
  const parsed = parseSettingsSnapshot(loadFixture("settings_snapshot_active.json"));
  assert.ok(parsed !== null);
  return { ...parsed, ...overrides };
}

function makeUpdate(overrides: Partial<UpdateStatusSnapshot> = {}): UpdateStatusSnapshot {
  return {
    state: "up_to_date",
    channel: "npx",
    current_version: "0.1.3",
    latest_version: "0.1.3",
    can_one_click_upgrade: true,
    fallback_command: "npx -y agy-companion@latest install",
    checked_at_ms: 1_760_000_000_000,
    error: null,
    ...overrides,
  };
}

function makeAppState(snapshot: SettingsSnapshot): AppState {
  return {
    connected: true,
    standalone: false,
    hydratedFromInitialResult: true,
    initialTabApplied: true,
    tabPinnedByUser: false,
    activeTab: "monitor",
    snapshot,
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
  };
}

describe("update wire types, SemVer comparator, and transport allow-list", () => {
  it("parses UpdateStatusSnapshot and embeds it in SettingsSnapshot", () => {
    const rawUpdate = makeUpdate({
      state: "available",
      channel: "uvx",
      current_version: "0.1.3-dev.1",
      latest_version: "0.1.4",
      can_one_click_upgrade: false,
      fallback_command: "uvx --refresh agy-companion install",
    });
    const parsed = parseUpdateStatusSnapshot(rawUpdate);
    assert.deepEqual(parsed, rawUpdate);

    assert.equal(parseUpdateStatusSnapshot(null), null);
    assert.equal(parseUpdateStatusSnapshot({ ...rawUpdate, state: "bogus" }), null);
    assert.equal(parseUpdateStatusSnapshot({ ...rawUpdate, channel: "brew" }), null);
    assert.equal(parseUpdateStatusSnapshot({ ...rawUpdate, current_version: "" }), null);

    const rawSnap = loadFixture<Record<string, unknown>>("settings_snapshot_active.json");
    const withUpdate = parseSettingsSnapshot({ ...rawSnap, update: rawUpdate });
    assert.ok(withUpdate !== null);
    assert.deepEqual(withUpdate.update, rawUpdate);

    const withoutUpdate = parseSettingsSnapshot(rawSnap);
    assert.ok(withoutUpdate !== null);
    assert.equal(withoutUpdate.update, null);
  });

  it("compares SemVer 2.0.0 versions including prereleases accurately", () => {
    assert.equal(compareSemVer("0.1.2", "0.1.3"), -1);
    assert.equal(compareSemVer("0.1.3-dev.2", "0.1.3"), -1);
    assert.equal(compareSemVer("0.1.3-dev.2", "0.1.3-dev.10"), -1);
    assert.equal(compareSemVer("0.1.3", "0.1.3"), 0);
    assert.equal(compareSemVer("0.2.0", "0.1.9"), 1);
    assert.equal(compareSemVer("v0.1.3", "0.1.3"), null);
  });

  it("allows get_update_status, check_update, and start_upgrade without conversation_id", async () => {
    const requests: Array<{ url: string; headers: Record<string, string>; body: Record<string, unknown> }> = [];
    const origFetch = globalThis.fetch;
    globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
      requests.push({
        url,
        headers: (init?.headers ?? {}) as Record<string, string>,
        body,
      });
      return new Response(JSON.stringify(makeUpdate()), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;

    try {
      const ctx = { token: "tok-upd", conversationId: "conv-upd-123456" };
      for (const tool of ["get_update_status", "check_update", "start_upgrade"] as const) {
        const res = await callSidecarApi(ctx, tool, {}, parseUpdateStatusSnapshot);
        assert.equal(res.ok, true, `${tool} should succeed`);
      }
      assert.deepEqual(
        requests.map((r) => r.url),
        ["/_api/get_update_status", "/_api/check_update", "/_api/start_upgrade"],
      );
      for (const req of requests) {
        assert.equal(req.headers["X-Sidecar-Token"], "tok-upd");
        assert.equal("conversation_id" in req.body, false);
      }
    } finally {
      globalThis.fetch = origFetch;
    }
  });
});

describe("createUpdateHeaderController header pills & popovers", () => {
  let env: FakeDomEnv;

  beforeEach(() => {
    env = installFakeDom();
  });

  afterEach(() => {
    env.teardown();
  });

  function mountHeader() {
    const root = document.createElement("div") as unknown as HTMLElement;
    const brand = document.createElement("div") as unknown as HTMLElement;
    root.append(brand);
    document.body.append(root);

    let checkCalls = 0;
    let upgradeCalls = 0;
    let reloadCalls = 0;
    const toasts: string[] = [];

    const ctrl = createUpdateHeaderController(brand, root, {
      onCheckUpdate: () => {
        checkCalls += 1;
      },
      onStartUpgrade: () => {
        upgradeCalls += 1;
      },
      onReloadPanel: () => {
        reloadCalls += 1;
      },
      onToast: (msg) => {
        toasts.push(msg);
      },
    });

    return {
      ctrl,
      get checkCalls() {
        return checkCalls;
      },
      get upgradeCalls() {
        return upgradeCalls;
      },
      get reloadCalls() {
        return reloadCalls;
      },
      toasts,
    };
  }

  it("shows version pill popover with Check for updates and hides upgrade pill when up_to_date or disabled", () => {
    const h = mountHeader();
    const snap = loadActiveSnapshot({
      binary_version: "0.1.3",
      update: makeUpdate({ state: "up_to_date", channel: "npx", current_version: "0.1.3" }),
    });

    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: snap,
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });

    assert.equal(h.ctrl.versionBtn.hidden, false);
    assert.equal(h.ctrl.versionBtn.textContent, "0.1.3");
    assert.equal(h.ctrl.upgradeBtn.hidden, true);
    assert.equal(h.ctrl.isVersionPopoverOpen(), false);

    // Click version pill -> opens popover with version, channel, status, and Check for updates button
    (h.ctrl.versionBtn as unknown as FakeElement).dispatchEvent(new FakeEvent("click"));
    assert.equal(h.ctrl.isVersionPopoverOpen(), true);
    assert.match(h.ctrl.versionPopover.textContent ?? "", /v0\.1\.3 · npx/);
    assert.match(h.ctrl.versionPopover.textContent ?? "", /Up to date/);

    const checkBtn = h.ctrl.versionPopover.querySelector("button") as unknown as FakeElement;
    assert.ok(checkBtn);
    assert.equal(checkBtn.textContent, "Check for updates");
    assert.equal(checkBtn.disabled, false);

    checkBtn.dispatchEvent(new FakeEvent("click"));
    assert.equal(h.checkCalls, 1);

    // While checking, button disables with Checking...
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: snap,
      checkingUpdate: true,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });
    assert.equal(checkBtn.textContent, "Checking\u2026");
    assert.equal(checkBtn.disabled, true);

    // Disabled state also hides upgrade pill
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: loadActiveSnapshot({
        binary_version: "0.1.3",
        update: makeUpdate({ state: "disabled" }),
      }),
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });
    assert.equal(h.ctrl.upgradeBtn.hidden, true);
    h.ctrl.destroy();
  });

  it("triggers direct 1-click upgrade with NO confirmation popover when available and can_one_click_upgrade is true", () => {
    const h = mountHeader();
    const snap = loadActiveSnapshot({
      binary_version: "0.1.3",
      update: makeUpdate({
        state: "available",
        current_version: "0.1.3",
        latest_version: "0.2.0",
        can_one_click_upgrade: true,
        fallback_command: "npx -y agy-companion@latest install",
      }),
    });

    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: snap,
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });

    assert.equal(h.ctrl.upgradeBtn.hidden, false);
    assert.equal(h.ctrl.upgradeBtn.textContent, "\u2191 Upgrade to 0.2.0");
    assert.equal(h.ctrl.upgradeBtn.title, "npx -y agy-companion@latest install");
    assert.equal(h.ctrl.upgradeBtn.disabled, false);

    (h.ctrl.upgradeBtn as unknown as FakeElement).dispatchEvent(new FakeEvent("click"));
    assert.equal(h.upgradeCalls, 1);
    assert.equal(h.ctrl.isUpgradePopoverOpen(), false);
    h.ctrl.destroy();
  });

  it("opens copyable fallback command popover when available and can_one_click_upgrade is false", () => {
    const h = mountHeader();
    const snap = loadActiveSnapshot({
      binary_version: "0.1.3",
      update: makeUpdate({
        state: "available",
        channel: "uvx",
        current_version: "0.1.3",
        latest_version: "0.2.0",
        can_one_click_upgrade: false,
        fallback_command: "uvx --refresh agy-companion install",
      }),
    });

    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: snap,
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });

    (h.ctrl.upgradeBtn as unknown as FakeElement).dispatchEvent(new FakeEvent("click"));
    assert.equal(h.upgradeCalls, 0);
    assert.equal(h.ctrl.isUpgradePopoverOpen(), true);
    assert.match(
      h.ctrl.upgradePopover.textContent ?? "",
      /uvx not found on PATH\. Run manually:/,
    );
    const code = h.ctrl.upgradePopover.querySelector(".cmd code");
    assert.equal(code?.textContent, "uvx --refresh agy-companion install");
    const copyBtn = h.ctrl.upgradePopover.querySelector(".cmd button");
    assert.equal(copyBtn?.textContent, "Copy");
    h.ctrl.destroy();
  });

  it("renders disabled spinner pill during upgrading and disables Check for updates", () => {
    const h = mountHeader();
    const snap = loadActiveSnapshot({
      binary_version: "0.1.3",
      update: makeUpdate({
        state: "upgrading",
        current_version: "0.1.3",
        latest_version: "0.2.0",
      }),
    });

    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: snap,
      checkingUpdate: false,
      holdingUpgrading: true,
      optimisticUpdate: null,
    });

    assert.equal(h.ctrl.upgradeBtn.hidden, false);
    assert.equal(h.ctrl.upgradeBtn.disabled, true);
    assert.ok(h.ctrl.upgradeBtn.querySelector(".hdr-spinner") !== null);
    assert.match(h.ctrl.upgradeBtn.textContent ?? "", /Upgrading\u2026/);
    const checkBtn = h.ctrl.versionPopover.querySelector("button") as unknown as FakeElement;
    assert.equal(checkBtn.disabled, true);
    h.ctrl.destroy();
  });

  it("renders Windows green Updated pill with info popover (no quit/reopen) and Unix Reload panel pill", () => {
    const h = mountHeader();

    // Windows upgraded
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: loadActiveSnapshot({
        platform: "windows",
        binary_version: "0.1.3",
        update: makeUpdate({
          state: "upgraded",
          current_version: "0.1.3",
          latest_version: "0.2.0",
        }),
      }),
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });

    assert.equal(h.ctrl.upgradeBtn.textContent, "\u2713 Updated to 0.2.0");
    (h.ctrl.upgradeBtn as unknown as FakeElement).dispatchEvent(new FakeEvent("click"));
    assert.equal(h.ctrl.isUpgradePopoverOpen(), true);
    assert.equal(h.reloadCalls, 0);
    assert.equal(h.ctrl.upgradePopover.textContent, "Proxy updated to v0.2.0.");
    assert.doesNotMatch(h.ctrl.upgradePopover.textContent ?? "", /Quit|reopen/i);

    // Unix upgraded (seen without initiating from this tab)
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: loadActiveSnapshot({
        platform: "macos",
        binary_version: "0.1.3",
        update: makeUpdate({
          state: "upgraded",
          current_version: "0.1.3",
          latest_version: "0.2.0",
        }),
      }),
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });
    assert.equal(h.ctrl.upgradeBtn.textContent, "\u2713 Reload panel");
    (h.ctrl.upgradeBtn as unknown as FakeElement).dispatchEvent(new FakeEvent("click"));
    assert.equal(h.reloadCalls, 1);
    h.ctrl.destroy();
  });

  it("auto-opens failed popover on transition to failed with error, fallback cmd, and conditional Retry", () => {
    const h = mountHeader();
    const baseSnap = loadActiveSnapshot({
      binary_version: "0.1.3",
      update: makeUpdate({ state: "upgrading", latest_version: "0.2.0" }),
    });
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: baseSnap,
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });
    assert.equal(h.ctrl.isUpgradePopoverOpen(), false);

    // Transition to failed with can_one_click_upgrade: true
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: loadActiveSnapshot({
        binary_version: "0.1.3",
        update: makeUpdate({
          state: "failed",
          latest_version: "0.2.0",
          can_one_click_upgrade: true,
          fallback_command: "npx -y agy-companion@latest install",
          error: "npm exited with code 1",
        }),
      }),
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });

    assert.equal(h.ctrl.upgradeBtn.textContent, "! Upgrade failed");
    assert.equal(h.ctrl.isUpgradePopoverOpen(), true);
    assert.match(h.ctrl.upgradePopover.textContent ?? "", /npm exited with code 1/);
    assert.equal(
      h.ctrl.upgradePopover.querySelector(".cmd code")?.textContent,
      "npx -y agy-companion@latest install",
    );

    const retryBtn = h.ctrl.upgradePopover.querySelector(
      ".hdr-pop-act-row button",
    ) as unknown as FakeElement;
    assert.ok(retryBtn);
    assert.equal(retryBtn.textContent, "Retry");
    retryBtn.dispatchEvent(new FakeEvent("click"));
    assert.equal(h.upgradeCalls, 1);
    assert.equal(h.ctrl.isUpgradePopoverOpen(), false);

    // Failed without PATH hides Retry button
    h.ctrl.update({
      connected: true,
      standalone: false,
      snapshot: loadActiveSnapshot({
        binary_version: "0.1.3",
        update: makeUpdate({
          state: "failed",
          can_one_click_upgrade: false,
          error: "npx missing",
        }),
      }),
      checkingUpdate: false,
      holdingUpgrading: false,
      optimisticUpdate: null,
    });
    assert.equal(h.ctrl.upgradePopover.querySelector(".hdr-pop-act-row button"), null);
    h.ctrl.destroy();
  });
});

describe("createUpgradePoller 2s poll, disconnect grace, and Unix auto-reload once", () => {
  function createFakeClock() {
    let now = 1_000_000;
    let nextId = 1;
    const timers = new Map<number, { at: number; cb: () => void }>();

    return {
      nowMs: () => now,
      scheduleFn: (cb: () => void, ms: number) => {
        const id = nextId++;
        timers.set(id, { at: now + ms, cb });
        return id;
      },
      clearFn: (id: number) => {
        timers.delete(id);
      },
      async advance(ms: number) {
        now += ms;
        for (const [id, entry] of Array.from(timers.entries())) {
          if (entry.at <= now) {
            timers.delete(id);
            entry.cb();
            await Promise.resolve();
            await Promise.resolve();
          }
        }
      },
      pendingCount: () => timers.size,
    };
  }

  it("holds Upgrading across <10s Unix execve disconnect blips and auto-reloads once on new version", async () => {
    const clock = createFakeClock();
    const responses: Array<
      { ok: true; data: UpdateStatusSnapshot } | { ok: false; error: string }
    > = [
      { ok: false, error: "Failed to fetch" },
      { ok: false, error: "Failed to fetch" },
      {
        ok: true,
        data: makeUpdate({
          state: "up_to_date",
          current_version: "0.2.0",
          latest_version: "0.2.0",
        }),
      },
    ];
    let pollIdx = 0;
    let reloadCount = 0;
    let disconnectTimeouts = 0;
    const platform: HostPlatform = "macos";

    const poller = createUpgradePoller({
      pollUpdateStatus: async () => responses[Math.min(pollIdx++, responses.length - 1)],
      getPlatform: () => platform,
      onUpdateStatus: () => {},
      onHoldingChange: () => {},
      onDisconnectTimeout: () => {
        disconnectTimeouts += 1;
      },
      reloadPage: () => {
        reloadCount += 1;
      },
      nowMs: clock.nowMs,
      scheduleFn: clock.scheduleFn,
      clearFn: clock.clearFn,
    });

    poller.notifyUpgradeStarted(
      makeUpdate({ state: "upgrading", current_version: "0.1.3", latest_version: "0.2.0" }),
    );
    assert.equal(poller.isHoldingUpgrading(), true);
    assert.equal(poller.isPolling(), true);

    // 2s: transient disconnect #1 -> still holding Upgrading
    await clock.advance(UPGRADE_POLL_INTERVAL_MS);
    assert.equal(poller.isHoldingUpgrading(), true);
    assert.equal(reloadCount, 0);
    assert.equal(disconnectTimeouts, 0);

    // 4s: transient disconnect #2 -> still holding Upgrading
    await clock.advance(UPGRADE_POLL_INTERVAL_MS);
    assert.equal(poller.isHoldingUpgrading(), true);
    assert.equal(reloadCount, 0);

    // 6s: sidecar back after execve on 0.2.0 -> auto-reloads ONCE
    await clock.advance(UPGRADE_POLL_INTERVAL_MS);
    assert.equal(reloadCount, 1);
    assert.equal(poller.didAutoReload(), true);
    assert.equal(poller.isHoldingUpgrading(), false);
    assert.equal(poller.isPolling(), false);

    // Further snapshots must NOT trigger a second reload
    poller.syncSettingsSnapshot(
      loadActiveSnapshot({
        platform: "macos",
        binary_version: "0.2.0",
        update: makeUpdate({ state: "upgraded", current_version: "0.2.0", latest_version: "0.2.0" }),
      }),
    );
    assert.equal(reloadCount, 1);
    poller.destroy();
  });

  it("releases hold and invokes onDisconnectTimeout after >=10s disconnect grace", async () => {
    const clock = createFakeClock();
    let disconnectTimeouts = 0;

    const poller = createUpgradePoller({
      pollUpdateStatus: async () => ({ ok: false, error: "connection refused" }),
      getPlatform: () => "linux",
      onUpdateStatus: () => {},
      onHoldingChange: () => {},
      onDisconnectTimeout: () => {
        disconnectTimeouts += 1;
      },
      reloadPage: () => {
        assert.fail("must not reload on disconnect timeout");
      },
      nowMs: clock.nowMs,
      scheduleFn: clock.scheduleFn,
      clearFn: clock.clearFn,
    });

    poller.notifyUpgradeStarted(
      makeUpdate({ state: "upgrading", current_version: "0.1.3", latest_version: "0.2.0" }),
    );

    // First failure at t+2s starts grace clock; after UPGRADE_DISCONNECT_GRACE_MS more, timeout fires
    for (let elapsed = 0; elapsed <= UPGRADE_DISCONNECT_GRACE_MS + UPGRADE_POLL_INTERVAL_MS; elapsed += UPGRADE_POLL_INTERVAL_MS) {
      await clock.advance(UPGRADE_POLL_INTERVAL_MS);
    }

    assert.equal(disconnectTimeouts, 1);
    assert.equal(poller.isHoldingUpgrading(), false);
    assert.equal(poller.isPolling(), false);
    poller.destroy();
  });

  it("does NOT auto-reload on Windows and delivers upgraded status to UI", async () => {
    const clock = createFakeClock();
    let reloadCount = 0;
    const delivered: UpdateStatusSnapshot[] = [];

    const poller = createUpgradePoller({
      pollUpdateStatus: async () => ({
        ok: true,
        data: makeUpdate({
          state: "upgraded",
          current_version: "0.1.3",
          latest_version: "0.2.0",
        }),
      }),
      getPlatform: () => "windows",
      onUpdateStatus: (status) => {
        delivered.push(status);
      },
      onHoldingChange: () => {},
      onDisconnectTimeout: () => {},
      reloadPage: () => {
        reloadCount += 1;
      },
      nowMs: clock.nowMs,
      scheduleFn: clock.scheduleFn,
      clearFn: clock.clearFn,
    });

    poller.notifyUpgradeStarted(
      makeUpdate({ state: "upgrading", current_version: "0.1.3", latest_version: "0.2.0" }),
    );
    await clock.advance(UPGRADE_POLL_INTERVAL_MS);

    assert.equal(reloadCount, 0);
    assert.equal(delivered.length, 1);
    assert.equal(delivered[0].state, "upgraded");
    assert.equal(poller.isPolling(), false);
    poller.destroy();
  });
});

describe("footer version-skew suppression during upgrading/upgraded", () => {
  let env: FakeDomEnv;

  beforeEach(() => {
    env = installFakeDom();
  });

  afterEach(() => {
    env.teardown();
  });

  it("suppresses Update pending / Version skew while update.state is upgrading or upgraded", () => {
    const container = document.createElement("div") as unknown as HTMLElement;
    document.body.append(container);
    const footer = createFooterController(container);

    // Normal version_skew without active upgrade -> shows Update pending / Version skew
    const skewed = loadActiveSnapshot({
      version_skew: true,
      binary_version: "0.2.0",
      update: makeUpdate({ state: "up_to_date" }),
    });
    footer.update(makeAppState(skewed));
    assert.match(footer.footer.textContent ?? "", /Update pending/);
    assert.match(footer.popover.textContent ?? "", /Version skew/);

    // During upgrading -> suppressed to Live / Healthy
    footer.update(
      makeAppState({
        ...skewed,
        update: makeUpdate({ state: "upgrading", latest_version: "0.2.0" }),
      }),
    );
    assert.match(footer.footer.textContent ?? "", /Live/);
    assert.doesNotMatch(footer.footer.textContent ?? "", /Update pending/);
    assert.match(footer.popover.textContent ?? "", /Healthy/);
    assert.doesNotMatch(footer.popover.textContent ?? "", /Version skew/);

    // During upgraded -> suppressed to Live / Healthy
    footer.update(
      makeAppState({
        ...skewed,
        update: makeUpdate({ state: "upgraded", latest_version: "0.2.0" }),
      }),
    );
    assert.match(footer.footer.textContent ?? "", /Live/);
    assert.doesNotMatch(footer.footer.textContent ?? "", /Update pending/);
    assert.doesNotMatch(footer.popover.textContent ?? "", /Version skew/);

    footer.destroy();
  });
});
