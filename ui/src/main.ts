import {
  App,
  applyDocumentTheme,
  applyHostStyleVariables,
  type McpUiHostContext,
} from "@modelcontextprotocol/ext-apps";
import { buildBannerNode } from "./banners.ts";
import {
  copyTextWithFallback,
  createBrandMarkSvg,
  createIcon,
  createTooltipController,
  el,
  quitReopenSentence,
} from "./dom.ts";
import { createFooterController } from "./footer.ts";
import { buildMonitorView } from "./monitor.ts";
import { createSettingsView } from "./settings.ts";
import "./style.css";
import {
  CURATED_MODEL_CATALOG,
  DEFAULT_ENABLED_MODELS,
  extractStructuredPayload,
  extractToolErrorText,
  formatCaughtError,
  normalizeHostPlatform,
  parseSettingsSnapshot,
  parseTestConnectionPayload,
  parseUsageSummary,
  validateApiKey,
  type AccountPlanChoice,
  type ActiveTab,
  type AppState,
  type HostPlatform,
  type SaveSettingsArgs,
  type SettingsSnapshot,
  type ToolCallResultLike,
  type UsageGroup,
  type UsageRange,
  type UsageUnit,
} from "./types.ts";
import { buildUsageView } from "./usage.ts";

const HEALTHY_POLL_MS = 10_000;
const UNHEALTHY_BACKOFF_MS: ReadonlyArray<number> = [3_000, 5_000, 10_000];
const FOCUS_RECHECK_MIN_GAP_MS = 2_000;

type ToolOutcome<T> =
  | { ok: true; data: T }
  | { ok: false; error: string };

async function main(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;

  const app = new App({ name: "Antigravity Companion", version: "0.1.0" }, {});

  const state: AppState = {
    connected: false,
    standalone: false,
    hydratedFromInitialResult: false,
    initialTabApplied: false,
    tabPinnedByUser: false,
    activeTab: "settings",
    snapshot: null,
    snapshotEpoch: 0,
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
  };

  let pollTimer: number | null = null;
  let toastTimer: number | null = null;
  let lastPollCompletedAt = 0;

  function isWriteInFlight(): boolean {
    return state.saving || state.clearing || state.updatingIntegration;
  }

  function activePlatform(): HostPlatform {
    return state.snapshot?.platform ?? normalizeHostPlatform(null);
  }

  /* =========================================================================
   * STATIC 3-ZONE SHELL MOUNT (PRESERVES INPUT FOCUS & POPOVER STABILITY)
   * ========================================================================= */
  const hdr = el("header", "hdr");
  const brand = el("div", "brand");
  const mark = el("span", "brand-mark");
  mark.append(createBrandMarkSvg());
  const verEl = el("span", "hdr-ver", "0.1.0");
  brand.append(mark, el("h1", "brand-name", "Antigravity Companion"), verEl);

  const nav = el("nav", "hdr-tabs");
  nav.setAttribute("role", "tablist");
  nav.setAttribute("aria-label", "Companion views");

  const tabSpecs: ReadonlyArray<{
    id: ActiveTab;
    icon: "pulse" | "bars" | "gear";
    label: string;
  }> = [
    { id: "monitor", icon: "pulse", label: "Monitor" },
    { id: "usage", icon: "bars", label: "Usage & Costs" },
    { id: "settings", icon: "gear", label: "Settings" },
  ];

  const tabBtns = new Map<ActiveTab, HTMLButtonElement>();
  const settingsDot = el("span", "tab-dot");
  settingsDot.hidden = true;

  for (const spec of tabSpecs) {
    const btn = el("button", "icon-tab");
    btn.type = "button";
    btn.setAttribute("role", "tab");
    btn.setAttribute("aria-label", spec.label);
    btn.setAttribute("aria-selected", String(state.activeTab === spec.id));
    btn.title = spec.label;
    btn.append(createIcon(spec.icon));
    if (spec.id === "settings") {
      btn.append(settingsDot);
    }
    btn.addEventListener("click", () => {
      selectTab(spec.id, true);
    });
    tabBtns.set(spec.id, btn);
    nav.append(btn);
  }
  hdr.append(brand, nav);

  const scroll = el("div", "pane-scroll");
  const contentWrap = el("div", "content-wrap");
  const bannerSlot = el("div", "banner-slot");
  const monitorPanel = el("div", "tab-panel");
  monitorPanel.setAttribute("role", "tabpanel");
  monitorPanel.setAttribute("aria-label", "Monitor");

  const usagePanel = el("div", "tab-panel");
  usagePanel.setAttribute("role", "tabpanel");
  usagePanel.setAttribute("aria-label", "Usage");

  const toastEl = el("div", "host-toast");
  toastEl.setAttribute("role", "status");
  toastEl.hidden = true;

  root.replaceChildren(hdr, scroll);

  const tooltip = createTooltipController(root);
  const settingsView = createSettingsView(
    {
      onOpenExternalUrl: (url, displayUrl) => {
        void handleOpenExternalUrl(url, displayUrl);
      },
      onDraftChange: (draft) => {
        state.apiKeyDraft = draft;
        settingsView.update(state);
      },
      onSaveKey: () => {
        void handleSaveKey();
      },
      onTestConnection: () => {
        void handleTestConnection();
      },
      onToggleClearConfirm: () => {
        state.clearConfirmOpen = !state.clearConfirmOpen;
        settingsView.update(state);
      },
      onCancelClear: () => {
        state.clearConfirmOpen = false;
        settingsView.update(state);
      },
      onClearKey: () => {
        void handleClearKey();
      },
      onToggleModel: (modelId) => {
        void handleToggleModel(modelId);
      },
      onRestoreDefaultModels: () => {
        void handleRestoreDefaultModels();
      },
      onSelectAccountPlan: (plan) => {
        void handleSelectAccountPlan(plan);
      },
      onToggleAdvOpen: (open) => {
        state.advOpen = open;
      },
      onRecheckIntegration: () => {
        void refreshSettings(false);
      },
      onOpenDisableProxyConfirm: () => {
        state.disableProxyConfirmOpen = true;
        settingsView.update(state);
      },
      onCancelDisableProxy: () => {
        state.disableProxyConfirmOpen = false;
        settingsView.update(state);
      },
      onDisableProxy: () => {
        void handleSetLauncherIntegration(false);
      },
      onEnableProxy: () => {
        void handleSetLauncherIntegration(true);
      },
    },
    tooltip,
  );
  settingsView.root.setAttribute("role", "tabpanel");
  settingsView.root.setAttribute("aria-label", "Settings");

  contentWrap.append(bannerSlot, monitorPanel, usagePanel, settingsView.root);
  scroll.append(contentWrap);

  const footerCtrl = createFooterController(root);
  root.append(toastEl);

  /* =========================================================================
   * RENDERERS (IN-PLACE FOR HEADER, SETTINGS & FOOTER; REBUILD MONITOR/USAGE)
   * ========================================================================= */
  function showToast(message: string): void {
    state.toastMessage = message;
    toastEl.textContent = message;
    toastEl.hidden = false;
    if (toastTimer !== null) {
      window.clearTimeout(toastTimer);
    }
    toastTimer = window.setTimeout(() => {
      toastTimer = null;
      state.toastMessage = null;
      toastEl.hidden = true;
    }, 3200);
  }

  function renderHeader(): void {
    verEl.textContent = state.snapshot?.binary_version || "0.1.0";
    for (const [id, btn] of tabBtns.entries()) {
      btn.setAttribute("aria-selected", String(state.activeTab === id));
    }

    const snap = state.snapshot;
    const li = snap?.launcher_integration ?? null;
    const platform = activePlatform();
    const unixDisabled =
      platform !== "windows" &&
      li !== null &&
      !li.enabled &&
      li.state !== "unsupported";
    const needsAttention =
      snap !== null &&
      (!snap.config.providers.gemini.api_key_configured ||
        unixDisabled ||
        li?.state === "manual_required" ||
        li?.hybrid_active !== true);
    settingsDot.hidden = !(needsAttention && state.activeTab !== "settings");
  }

  function renderBanner(): void {
    const node = buildBannerNode(state, {
      onRecheck: () => {
        void refreshSettings(false);
      },
      onEnableProxy: () => {
        void handleSetLauncherIntegration(true);
      },
      onToast: showToast,
    });
    if (node) {
      bannerSlot.replaceChildren(node);
    } else {
      bannerSlot.replaceChildren();
    }
  }

  function renderActiveTab(): void {
    monitorPanel.hidden = state.activeTab !== "monitor";
    usagePanel.hidden = state.activeTab !== "usage";
    settingsView.root.hidden = state.activeTab !== "settings";

    if (state.activeTab === "monitor") {
      monitorPanel.replaceChildren(buildMonitorView(state));
    } else if (state.activeTab === "usage") {
      usagePanel.replaceChildren(
        buildUsageView(state, {
          onSelectRange: (range: UsageRange) => {
            if (state.usageRange !== range) {
              state.usageRange = range;
              state.usageSummary = null;
              renderActiveTab();
              void loadUsage(range, false);
            }
          },
          onSelectUnit: (unit: UsageUnit) => {
            state.usageUnit = unit;
            renderActiveTab();
          },
          onSelectGroup: (group: UsageGroup) => {
            state.usageGroup = group;
            renderActiveTab();
          },
        }),
      );
    } else {
      settingsView.update(state);
    }
  }

  function renderAll(): void {
    renderHeader();
    renderBanner();
    renderActiveTab();
    footerCtrl.update(state);
  }

  function selectTab(tab: ActiveTab, fromUser: boolean): void {
    if (fromUser) {
      state.tabPinnedByUser = true;
    }
    if (state.activeTab !== tab) {
      tooltip.hideNow();
      state.activeTab = tab;
      scroll.scrollTop = 0;
    }
    renderAll();
    if (tab === "usage" && state.connected && !state.standalone) {
      void loadUsage(state.usageRange, state.usageSummary !== null);
    }
  }

  function applySnapshot(snap: SettingsSnapshot): void {
    state.snapshot = snap;
    state.loadError = null;
    if (snap.proxy.reachable) {
      state.unhealthyStreak = 0;
    } else {
      state.unhealthyStreak = Math.min(state.unhealthyStreak + 1, UNHEALTHY_BACKOFF_MS.length);
    }

    if (!state.initialTabApplied && !state.tabPinnedByUser) {
      const hybridActive = snap.launcher_integration?.hybrid_active === true;
      const apiKeyConfigured = snap.config.providers.gemini.api_key_configured;
      state.activeTab = hybridActive || apiKeyConfigured ? "monitor" : "settings";
      state.initialTabApplied = true;
    }

    renderAll();
  }

  function applyHostContext(ctx: McpUiHostContext | null | undefined): void {
    if (!ctx) return;
    if (ctx.theme) {
      applyDocumentTheme(ctx.theme);
      root?.setAttribute("data-theme", ctx.theme);
    }
    if (ctx.styles?.variables) {
      applyHostStyleVariables(ctx.styles.variables);
    }
  }

  /* =========================================================================
   * MCP TOOL INVOCATION HELPERS
   * ========================================================================= */
  async function callTool<T>(
    name: string,
    args: Record<string, unknown>,
    parser: (raw: unknown) => T | null,
  ): Promise<ToolOutcome<T>> {
    try {
      const result = (await app.callServerTool({
        name,
        arguments: args,
      })) as ToolCallResultLike;
      if (result.isError) {
        return { ok: false, error: extractToolErrorText(result) };
      }
      const payload = extractStructuredPayload(result);
      const parsed = parser(payload);
      if (parsed === null) {
        return { ok: false, error: `Unexpected response from ${name}.` };
      }
      return { ok: true, data: parsed };
    } catch (err) {
      return { ok: false, error: formatCaughtError(err) };
    }
  }

  async function refreshSettings(isBackgroundPoll: boolean): Promise<void> {
    if (!state.connected || state.standalone || state.refreshing) return;
    if (isBackgroundPoll && isWriteInFlight()) return;

    const epoch = state.snapshotEpoch;
    state.refreshing = true;
    if (!isBackgroundPoll) {
      renderBanner();
      settingsView.update(state);
    }

    const outcome = await callTool("get_settings", {}, parseSettingsSnapshot);
    state.refreshing = false;
    lastPollCompletedAt = Date.now();

    if (state.snapshotEpoch !== epoch || isWriteInFlight()) {
      scheduleNextPoll();
      return;
    }

    if (outcome.ok) {
      applySnapshot(outcome.data);
      if (state.activeTab === "usage") {
        void loadUsage(state.usageRange, true);
      }
    } else {
      state.unhealthyStreak = Math.min(state.unhealthyStreak + 1, UNHEALTHY_BACKOFF_MS.length);
      if (!state.snapshot) {
        state.loadError = outcome.error;
      }
      renderAll();
    }

    scheduleNextPoll();
  }

  async function loadUsage(range: UsageRange, silent: boolean): Promise<void> {
    if (!state.connected || state.standalone) return;
    state.usageEpoch += 1;
    const epoch = state.usageEpoch;

    if (!silent) {
      state.usageLoading = true;
      state.usageError = null;
      if (state.activeTab === "usage") {
        renderActiveTab();
      }
    }

    const outcome = await callTool("get_usage", { range }, parseUsageSummary);
    if (state.usageEpoch !== epoch) {
      return;
    }

    state.usageLoading = false;
    if (outcome.ok) {
      state.usageSummary = outcome.data;
      state.usageError = null;
    } else if (!silent || !state.usageSummary) {
      state.usageError = outcome.error;
    }

    if (state.activeTab === "usage") {
      renderActiveTab();
    }
  }

  /* =========================================================================
   * SETTINGS & LAUNCHER MUTATIONS (SERIALIZED WITH EPOCH + WRITE LOCKOUT)
   * ========================================================================= */
  async function handleOpenExternalUrl(url: string, displayUrl: string): Promise<void> {
    let opened = false;
    if (state.connected && !state.standalone) {
      try {
        const res = await app.openLink({ url });
        if (!res || res.isError !== true) {
          opened = true;
        }
      } catch {
        opened = false;
      }
    }
    if (!opened) {
      const copied = await copyTextWithFallback(url);
      showToast(copied ? `Copied ${displayUrl}` : `Open ${displayUrl} in your browser`);
    }
  }

  async function handleSaveKey(): Promise<void> {
    if (!state.connected || state.standalone || isWriteInFlight()) return;
    const validationErr = validateApiKey(state.apiKeyDraft);
    if (validationErr) {
      state.keyFeedback = { tone: "err", text: validationErr };
      settingsView.update(state);
      return;
    }

    const trimmed = state.apiKeyDraft.trim();
    state.snapshotEpoch += 1;
    state.saving = true;
    state.keyFeedback = null;
    settingsView.update(state);

    const args: SaveSettingsArgs = {
      providers: {
        gemini: {
          api_key: { set: trimmed },
        },
      },
    };
    const outcome = await callTool(
      "save_settings",
      args as unknown as Record<string, unknown>,
      parseSettingsSnapshot,
    );
    state.saving = false;

    if (outcome.ok) {
      state.snapshotEpoch += 1;
      state.apiKeyDraft = "";
      state.clearConfirmOpen = false;
      settingsView.clearKeyInputDom();
      state.keyFeedback = { tone: "ok", text: "API key saved securely (masked)" };
      applySnapshot(outcome.data);
    } else {
      state.keyFeedback = { tone: "err", text: outcome.error };
      renderAll();
    }
  }

  async function handleClearKey(): Promise<void> {
    if (!state.connected || state.standalone || isWriteInFlight()) return;
    state.snapshotEpoch += 1;
    state.clearing = true;
    state.keyFeedback = null;
    settingsView.update(state);

    const args: SaveSettingsArgs = {
      providers: {
        gemini: {
          api_key: "clear",
        },
      },
    };
    const outcome = await callTool(
      "save_settings",
      args as unknown as Record<string, unknown>,
      parseSettingsSnapshot,
    );
    state.clearing = false;

    if (outcome.ok) {
      state.snapshotEpoch += 1;
      state.clearConfirmOpen = false;
      state.keyFeedback = { tone: "warn", text: "API key cleared" };
      applySnapshot(outcome.data);
    } else {
      state.keyFeedback = { tone: "err", text: outcome.error };
      renderAll();
    }
  }

  async function handleTestConnection(): Promise<void> {
    if (!state.connected || state.standalone || state.testing || isWriteInFlight()) return;
    state.testing = true;
    state.keyFeedback = { tone: "info", text: "Testing Google AI Studio connection\u2026" };
    settingsView.update(state);

    const outcome = await callTool("test_connection", {}, parseTestConnectionPayload);
    state.testing = false;

    if (outcome.ok) {
      state.keyFeedback = {
        tone: outcome.data.ok ? "ok" : "err",
        text: outcome.data.message,
      };
    } else {
      state.keyFeedback = { tone: "err", text: outcome.error };
    }
    settingsView.update(state);
  }

  async function saveEnabledModels(
    nextModels: string[],
    successText: string | null,
  ): Promise<void> {
    if (!state.connected || state.standalone || isWriteInFlight()) return;
    state.snapshotEpoch += 1;
    state.saving = true;
    settingsView.update(state);

    const args: SaveSettingsArgs = {
      providers: {
        gemini: {
          enabled_models: nextModels,
        },
      },
    };
    const outcome = await callTool(
      "save_settings",
      args as unknown as Record<string, unknown>,
      parseSettingsSnapshot,
    );
    state.saving = false;

    if (outcome.ok) {
      state.snapshotEpoch += 1;
      const count = outcome.data.config.providers.gemini.enabled_models.length;
      state.modelFeedback =
        count > 0
          ? {
              tone: "ok",
              text:
                successText ??
                `Saved \u00b7 ${count} ${count === 1 ? "model" : "models"} active`,
            }
          : null;
      applySnapshot(outcome.data);
    } else {
      state.modelFeedback = { tone: "err", text: outcome.error };
      renderAll();
    }
  }

  async function handleToggleModel(modelId: string): Promise<void> {
    const current =
      state.snapshot?.config.providers.gemini.enabled_models ?? [...DEFAULT_ENABLED_MODELS];
    const nextSet = new Set(current);
    if (nextSet.has(modelId)) {
      nextSet.delete(modelId);
    } else {
      nextSet.add(modelId);
    }
    // Preserve canonical catalog order, followed by any extra persisted IDs
    const ordered: string[] = [];
    for (const m of CURATED_MODEL_CATALOG) {
      if (nextSet.has(m.id)) {
        ordered.push(m.id);
        nextSet.delete(m.id);
      }
    }
    for (const extra of nextSet) {
      ordered.push(extra);
    }
    await saveEnabledModels(ordered, null);
  }

  async function handleRestoreDefaultModels(): Promise<void> {
    await saveEnabledModels(
      [...DEFAULT_ENABLED_MODELS],
      `Restored default Gemini 4 models (${DEFAULT_ENABLED_MODELS.length} enabled)`,
    );
  }

  async function handleSelectAccountPlan(plan: AccountPlanChoice): Promise<void> {
    if (!state.connected || state.standalone || isWriteInFlight()) return;
    state.snapshotEpoch += 1;
    state.saving = true;
    state.advFeedback = null;
    settingsView.update(state);

    const args: SaveSettingsArgs = { account_plan: plan };
    const outcome = await callTool(
      "save_settings",
      args as unknown as Record<string, unknown>,
      parseSettingsSnapshot,
    );
    state.saving = false;

    if (outcome.ok) {
      state.snapshotEpoch += 1;
      const label =
        plan === "auto"
          ? "Auto"
          : plan === "personal"
            ? "Personal"
            : "Organization";
      state.advFeedback = { tone: "ok", text: `Saved \u00b7 Antigravity plan set to ${label}` };
      applySnapshot(outcome.data);
    } else {
      state.advFeedback = { tone: "err", text: outcome.error };
      renderAll();
    }
  }

  async function handleSetLauncherIntegration(enabled: boolean): Promise<void> {
    if (!state.connected || state.standalone || isWriteInFlight()) return;
    state.snapshotEpoch += 1;
    state.updatingIntegration = true;
    state.advFeedback = null;
    renderBanner();
    settingsView.update(state);

    const outcome = await callTool(
      "set_launcher_integration",
      { enabled },
      parseSettingsSnapshot,
    );
    state.updatingIntegration = false;

    if (outcome.ok) {
      state.snapshotEpoch += 1;
      state.disableProxyConfirmOpen = false;
      const platform = outcome.data.platform;
      const reopen = quitReopenSentence(platform);
      state.advFeedback = enabled
        ? { tone: "warn", text: `Proxy re-enabled \u2014 ${reopen}` }
        : {
            tone: "warn",
            text: `Proxy disabled \u00b7 Re-enable and ${reopen} to restore models`,
          };
      applySnapshot(outcome.data);
    } else {
      state.advFeedback = { tone: "err", text: outcome.error };
      renderAll();
    }
  }

  /* =========================================================================
   * VISIBILITY-AWARE BACKGROUND POLLING
   * ========================================================================= */
  function clearPollTimer(): void {
    if (pollTimer !== null) {
      window.clearTimeout(pollTimer);
      pollTimer = null;
    }
  }

  function nextPollDelayMs(): number {
    const reachable = state.snapshot?.proxy.reachable ?? true;
    if (reachable && state.unhealthyStreak === 0) {
      return HEALTHY_POLL_MS;
    }
    const idx = Math.min(
      Math.max(0, state.unhealthyStreak - 1),
      UNHEALTHY_BACKOFF_MS.length - 1,
    );
    return UNHEALTHY_BACKOFF_MS[idx];
  }

  function scheduleNextPoll(): void {
    clearPollTimer();
    if (!state.connected || state.standalone || document.hidden) return;
    pollTimer = window.setTimeout(() => {
      pollTimer = null;
      if (!document.hidden) {
        void refreshSettings(true);
      }
    }, nextPollDelayMs());
  }

  function triggerThrottledRecheck(): void {
    if (!state.connected || state.standalone || document.hidden) return;
    if (Date.now() - lastPollCompletedAt >= FOCUS_RECHECK_MIN_GAP_MS) {
      void refreshSettings(true);
    } else {
      scheduleNextPoll();
    }
  }

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      clearPollTimer();
    } else {
      triggerThrottledRecheck();
    }
  });

  window.addEventListener("focus", () => {
    triggerThrottledRecheck();
  });

  window.addEventListener("pagehide", () => {
    clearPollTimer();
    if (toastTimer !== null) {
      window.clearTimeout(toastTimer);
      toastTimer = null;
    }
    tooltip.destroy();
    footerCtrl.destroy();
  });

  document.addEventListener("keydown", (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      tooltip.hideNow();
      if (footerCtrl.isPopoverOpen()) {
        footerCtrl.closePopover();
      }
    }
  });

  /* =========================================================================
   * REGISTER MCP APP HANDLERS BEFORE app.connect() (REQUIRED BY EXT-APPS & S5)
   * ========================================================================= */
  app.ontoolinput = () => {
    // Initial snapshot arrives in ontoolresult
  };

  app.ontoolresult = (result) => {
    const typed = result as ToolCallResultLike;
    if (typed.isError) {
      state.loadError = extractToolErrorText(typed);
      renderAll();
      return;
    }
    const payload = extractStructuredPayload(typed);
    const parsed = parseSettingsSnapshot(payload);
    if (parsed) {
      state.hydratedFromInitialResult = true;
      state.snapshotEpoch += 1;
      applySnapshot(parsed);
      scheduleNextPoll();
    }
  };

  app.onhostcontextchanged = (ctx) => {
    applyHostContext(ctx);
  };

  renderAll();

  try {
    await app.connect();
    state.connected = true;
    state.standalone = false;
    applyHostContext(app.getHostContext());
    renderAll();

    window.setTimeout(() => {
      if (!state.hydratedFromInitialResult && !state.refreshing) {
        void refreshSettings(false);
      } else {
        scheduleNextPoll();
      }
    }, 120);
  } catch {
    state.connected = false;
    state.standalone = true;
    renderAll();
  }
}

void main();
