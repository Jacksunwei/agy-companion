import {
  createIcon,
  el,
  formatResolvedPlanLabel,
  quitReopenSentence,
  type TooltipController,
} from "./dom.ts";
import {
  DEFAULT_ENABLED_MODELS,
  LIVE_PROVIDERS,
  type AccountPlanChoice,
  type AppState,
  type CuratedModelInfo,
} from "./types.ts";

export interface SettingsViewActions {
  onOpenExternalUrl: (url: string, displayUrl: string) => void;
  onDraftChange: (draft: string) => void;
  onSaveKey: () => void;
  onTestConnection: () => void;
  onToggleClearConfirm: () => void;
  onCancelClear: () => void;
  onClearKey: () => void;
  onToggleModel: (modelId: string) => void;
  onRestoreDefaultModels: () => void;
  onSelectAccountPlan: (plan: AccountPlanChoice) => void;
  onToggleAdvOpen: (open: boolean) => void;
  onRecheckIntegration: () => void;
  onOpenDisableProxyConfirm: () => void;
  onCancelDisableProxy: () => void;
  onDisableProxy: () => void;
  onEnableProxy: () => void;
}

export interface SettingsViewController {
  root: HTMLElement;
  update: (state: AppState) => void;
  clearKeyInputDom: () => void;
}

const PLAN_OPTIONS: ReadonlyArray<{
  id: AccountPlanChoice;
  title: string;
  hint: string;
}> = [
  {
    id: "auto",
    title: "Auto",
    hint: "Detect from your signed-in Antigravity account",
  },
  {
    id: "personal",
    title: "Personal",
    hint: "Google AI Pro or personal Google account",
  },
  {
    id: "organization",
    title: "Organization",
    hint: "Managed organization, Workspace, or PayGo plan",
  },
];

export function createSettingsView(
  actions: SettingsViewActions,
  tooltip: TooltipController,
): SettingsViewController {
  const root = el("div", "settings-panel");

  // Section A: Model providers
  root.append(el("div", "sec-kicker", "Model providers"));
  const providerStack = el("div", "provider-stack");

  // Render live providers from typed registry (Google AI Studio today)
  const geminiProvider = LIVE_PROVIDERS[0];

  const card = el("section", "card");
  card.setAttribute("aria-label", `${geminiProvider.title} provider`);

  const hdr = el("div", "sec-h");
  hdr.append(el("span", "prov-title", geminiProvider.title));

  const statusTag = el("span", "tag");
  const statusDot = el("span", "sdot ok");
  const statusText = el("span", null, "Not set");
  statusTag.append(statusDot, statusText);
  hdr.append(statusTag);

  const right = el("span", "sec-right");
  const getKeyBtn = el("button", "lnk", "Get API key \u2197");
  getKeyBtn.type = "button";
  getKeyBtn.title = geminiProvider.keyPortalUrl;
  getKeyBtn.addEventListener("click", () => {
    actions.onOpenExternalUrl(
      geminiProvider.keyPortalUrl,
      geminiProvider.keyPortalDisplay,
    );
  });
  right.append(getKeyBtn);
  hdr.append(right);
  card.append(hdr);

  // Write-only API key input
  const keyInput = el("input", "inp mono");
  keyInput.id = "gemini-api-key";
  keyInput.type = "password";
  keyInput.autocomplete = "off";
  keyInput.spellcheck = false;
  keyInput.placeholder = "Paste Google AI Studio API key";
  keyInput.setAttribute("aria-label", "Google AI Studio API key");

  const actRow = el("div", "act-row");
  const saveBtn = el("button", "btn btn-primary", "Save");
  saveBtn.type = "button";
  saveBtn.disabled = true;

  const testBtn = el("button", "btn", "Test");
  testBtn.type = "button";
  testBtn.disabled = true;

  const clearBtn = el("button", "lnk-danger", "Clear");
  clearBtn.type = "button";
  clearBtn.hidden = true;

  keyInput.addEventListener("input", () => {
    actions.onDraftChange(keyInput.value);
  });
  keyInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      if (!saveBtn.disabled) {
        actions.onSaveKey();
      }
    }
  });
  saveBtn.addEventListener("click", () => actions.onSaveKey());
  testBtn.addEventListener("click", () => actions.onTestConnection());
  clearBtn.addEventListener("click", () => actions.onToggleClearConfirm());

  actRow.append(saveBtn, testBtn, clearBtn);
  card.append(keyInput, actRow);

  // Inline 2-click Clear confirm row
  const clearConfirmRow = el("div", "confirm-row");
  clearConfirmRow.hidden = true;
  clearConfirmRow.append(el("span", null, "Remove stored Google AI Studio key?"));
  const confirmClearBtn = el("button", "btn btn-sm btn-danger-solid", "Confirm clear");
  confirmClearBtn.type = "button";
  confirmClearBtn.addEventListener("click", () => actions.onClearKey());
  const cancelClearBtn = el("button", "btn btn-sm", "Cancel");
  cancelClearBtn.type = "button";
  cancelClearBtn.addEventListener("click", () => actions.onCancelClear());
  clearConfirmRow.append(confirmClearBtn, cancelClearBtn);
  card.append(clearConfirmRow);

  // Key feedback status line
  const keyFeedbackEl = el("div", "status-line");
  keyFeedbackEl.setAttribute("role", "status");
  keyFeedbackEl.hidden = true;
  card.append(keyFeedbackEl);

  // Divider + Multi-select Enabled models
  const modSec = el("div", "divider");
  const mh = el("div", "sec-h");
  mh.append(el("span", "sec-t", "Enabled models"));
  const modelCountEl = el("span", "sec-right sec-count num", `${DEFAULT_ENABLED_MODELS.length} enabled`);
  mh.append(modelCountEl);
  modSec.append(mh);

  const chipStrip = el("div", "chip-strip");
  chipStrip.setAttribute("role", "group");
  chipStrip.setAttribute("aria-label", "Enabled Google AI Studio models");

  const chipMap = new Map<string, { btn: HTMLButtonElement; info: CuratedModelInfo }>();
  for (const model of geminiProvider.models) {
    const chip = el("button", "m-chip");
    chip.type = "button";
    chip.setAttribute("role", "checkbox");
    chip.setAttribute("aria-checked", String(model.defaultOn));
    chip.setAttribute("aria-describedby", "model-tooltip");

    const chk = el("span", "chk");
    chk.append(createIcon("check"));
    chip.append(chk, el("span", null, model.label));

    chip.addEventListener("mouseenter", () => tooltip.show(chip, model));
    chip.addEventListener("mouseleave", () => tooltip.scheduleHide());
    chip.addEventListener("focus", () => tooltip.show(chip, model));
    chip.addEventListener("blur", () => tooltip.hideNow());
    chip.addEventListener("click", () => {
      tooltip.hideNow();
      actions.onToggleModel(model.id);
    });

    chipMap.set(model.id, { btn: chip, info: model });
    chipStrip.append(chip);
  }
  modSec.append(chipStrip);

  // 0-models inline notice + Restore defaults
  const zeroModelsBox = el("div", "zero-models-note");
  zeroModelsBox.hidden = true;
  zeroModelsBox.append(
    el("span", null, "No models enabled \u2014 none will appear in Antigravity's selector."),
  );
  const restoreDefaultsBtn = el("button", "lnk", "Restore defaults");
  restoreDefaultsBtn.type = "button";
  restoreDefaultsBtn.addEventListener("click", () => actions.onRestoreDefaultModels());
  zeroModelsBox.append(restoreDefaultsBtn);
  modSec.append(zeroModelsBox);

  const modelFeedbackEl = el("div", "status-line");
  modelFeedbackEl.setAttribute("role", "status");
  modelFeedbackEl.hidden = true;
  modSec.append(modelFeedbackEl);

  card.append(modSec);
  providerStack.append(card);
  root.append(providerStack);

  // Section B: Collapsed Advanced Disclosure (ONLY Antigravity plan + Unix-only Local proxy)
  const advDetails = el("details", "adv-details");
  advDetails.open = false;
  advDetails.setAttribute("aria-label", "Advanced settings");
  advDetails.addEventListener("toggle", () => {
    actions.onToggleAdvOpen(advDetails.open);
  });

  const advSum = el("summary", "adv-sum");
  const advChev = createIcon("chev");
  advChev.setAttribute("class", "adv-chev");
  const advHintEl = el("span", "adv-hint", "Plan \u00b7 Local proxy");
  advSum.append(advChev, el("span", "adv-title", "Advanced"), advHintEl);
  advDetails.append(advSum);

  const advBody = el("div", "adv-body");

  // 1. Antigravity plan radio cards
  const planWrap = el("div");
  planWrap.append(el("span", "field-lbl", "Antigravity plan"));
  const prList = el("div", "pr");
  prList.setAttribute("role", "radiogroup");
  prList.setAttribute("aria-label", "Antigravity plan");

  const planRows = new Map<
    AccountPlanChoice,
    {
      row: HTMLLabelElement;
      input: HTMLInputElement;
      resolvedBadge: HTMLSpanElement | null;
    }
  >();

  for (const opt of PLAN_OPTIONS) {
    const row = el("label", "pr-row");
    const rWrap = el("span", "pr-radio");
    const inp = el("input", "pr-inp");
    inp.type = "radio";
    inp.name = "antigravity-plan";
    inp.value = opt.id;
    inp.addEventListener("change", () => {
      if (inp.checked) {
        actions.onSelectAccountPlan(opt.id);
      }
    });
    rWrap.append(inp, el("span", "pr-dot"));

    const rBody = el("div", "pr-body");
    const tRow = el("div", "pr-t");
    tRow.append(el("span", null, opt.title));
    let resolvedBadge: HTMLSpanElement | null = null;
    if (opt.id === "auto") {
      resolvedBadge = el("span", "tag tag-brand");
      resolvedBadge.hidden = true;
      tRow.append(resolvedBadge);
    }
    rBody.append(tRow, el("div", "pr-h", opt.hint));

    row.append(rWrap, rBody);
    prList.append(row);
    planRows.set(opt.id, { row, input: inp, resolvedBadge });
  }
  planWrap.append(prList);
  advBody.append(planWrap);

  // 2. Local proxy (Unix only — hidden on Windows)
  const proxyWrap = el("div", "adv-sep");
  const proxyRow = el("div", "proxy-ctrl-row");
  const proxyInfo = el("div", "proxy-ctrl-info");
  const proxyHdr = el("div", "sec-h proxy-ctrl-hdr");
  proxyHdr.append(el("span", "sec-t", "Local proxy"));
  const proxyBadge = el("span", "tag tag-ok", "Active");
  proxyHdr.append(proxyBadge);
  const proxySub = el("div", "opt-sub");
  proxyInfo.append(proxyHdr, proxySub);

  const recheckProxyBtn = el("button", "btn btn-sm proxy-ctrl-btn", "Re-check");
  recheckProxyBtn.type = "button";
  recheckProxyBtn.hidden = true;
  recheckProxyBtn.addEventListener("click", () => actions.onRecheckIntegration());

  const disableProxyBtn = el("button", "btn btn-sm btn-danger-outline proxy-ctrl-btn", "Disable proxy");
  disableProxyBtn.type = "button";
  disableProxyBtn.hidden = true;
  disableProxyBtn.addEventListener("click", () => actions.onOpenDisableProxyConfirm());

  const enableProxyBtn = el("button", "btn btn-primary btn-sm proxy-ctrl-btn", "Re-enable proxy");
  enableProxyBtn.type = "button";
  enableProxyBtn.hidden = true;
  enableProxyBtn.addEventListener("click", () => actions.onEnableProxy());

  proxyRow.append(proxyInfo, recheckProxyBtn, disableProxyBtn, enableProxyBtn);
  proxyWrap.append(proxyRow);

  // Inline 2-click Disable proxy confirm row
  const disableConfirmRow = el("div", "confirm-row");
  disableConfirmRow.hidden = true;
  disableConfirmRow.append(
    el(
      "span",
      null,
      "Disable local proxy? Models won't load after you reopen Antigravity.",
    ),
  );
  const confirmDisableBtn = el("button", "btn btn-danger-solid btn-sm", "Confirm disable");
  confirmDisableBtn.type = "button";
  confirmDisableBtn.addEventListener("click", () => actions.onDisableProxy());
  const cancelDisableBtn = el("button", "btn btn-sm", "Cancel");
  cancelDisableBtn.type = "button";
  cancelDisableBtn.addEventListener("click", () => actions.onCancelDisableProxy());
  disableConfirmRow.append(confirmDisableBtn, cancelDisableBtn);
  proxyWrap.append(disableConfirmRow);

  advBody.append(proxyWrap);

  const advFeedbackEl = el("div", "status-line");
  advFeedbackEl.setAttribute("role", "status");
  advFeedbackEl.hidden = true;
  advBody.append(advFeedbackEl);

  advDetails.append(advBody);
  root.append(advDetails);

  const update = (state: AppState) => {
    const snap = state.snapshot;
    const gemini = snap?.config.providers.gemini;
    const apiKeyConfigured = gemini?.api_key_configured === true;
    const apiKeyHint = gemini?.api_key_hint ?? null;
    const enabledSet = new Set(gemini?.enabled_models ?? DEFAULT_ENABLED_MODELS);
    const platform = snap?.platform ?? "macos";
    const canCall = state.connected && !state.standalone;
    const writeBusy = state.saving || state.clearing || state.updatingIntegration;
    const hasDraft = state.apiKeyDraft.trim().length > 0;

    // Status pill
    if (apiKeyConfigured) {
      statusTag.className = "tag tag-ok mono";
      statusDot.hidden = false;
      statusText.textContent = apiKeyHint || "Configured";
    } else {
      statusTag.className = "tag";
      statusDot.hidden = true;
      statusText.textContent = "Not set";
    }

    // Input placeholder (never clobber active typing)
    keyInput.placeholder = apiKeyConfigured
      ? "Paste new key to replace"
      : "Paste Google AI Studio API key";
    keyInput.disabled = !canCall || state.saving || state.clearing;
    if (document.activeElement !== keyInput && keyInput.value !== state.apiKeyDraft) {
      keyInput.value = state.apiKeyDraft;
    }

    // Buttons: Test disabled on unsaved draft, unconfigured, or busy
    saveBtn.textContent = state.saving ? "Saving\u2026" : "Save";
    saveBtn.disabled = !canCall || !hasDraft || writeBusy;

    testBtn.textContent = state.testing ? "Testing\u2026" : "Test";
    testBtn.disabled = !canCall || !apiKeyConfigured || hasDraft || writeBusy || state.testing;

    clearBtn.hidden = !apiKeyConfigured;
    clearBtn.disabled = !canCall || writeBusy;

    clearConfirmRow.hidden = !(state.clearConfirmOpen && apiKeyConfigured);
    confirmClearBtn.textContent = state.clearing ? "Clearing\u2026" : "Confirm clear";
    confirmClearBtn.disabled = !canCall || writeBusy;
    cancelClearBtn.disabled = writeBusy;

    if (state.keyFeedback) {
      keyFeedbackEl.className = `status-line ${state.keyFeedback.tone}`;
      keyFeedbackEl.textContent = state.keyFeedback.text;
      keyFeedbackEl.hidden = false;
    } else {
      keyFeedbackEl.hidden = true;
    }

    // Enabled models chips
    const enabledCount = enabledSet.size;
    modelCountEl.textContent = `${enabledCount} enabled`;
    for (const [id, entry] of chipMap.entries()) {
      entry.btn.setAttribute("aria-checked", String(enabledSet.has(id)));
      entry.btn.disabled = !canCall || writeBusy;
    }

    zeroModelsBox.hidden = enabledCount > 0;
    restoreDefaultsBtn.disabled = !canCall || writeBusy;

    if (enabledCount > 0 && state.modelFeedback) {
      modelFeedbackEl.className = `status-line ${state.modelFeedback.tone}`;
      modelFeedbackEl.textContent = state.modelFeedback.text;
      modelFeedbackEl.hidden = false;
    } else {
      modelFeedbackEl.hidden = true;
    }

    // Advanced disclosure
    if (advDetails.open !== state.advOpen) {
      advDetails.open = state.advOpen;
    }
    advHintEl.textContent = platform === "windows" ? "Plan" : "Plan \u00b7 Local proxy";

    // Antigravity plan radio cards
    const planMode = snap?.account_plan.mode ?? "auto";
    const resolvedLabel = formatResolvedPlanLabel(snap?.account_plan.resolved ?? null);
    for (const [id, entry] of planRows.entries()) {
      const selected = planMode === id;
      entry.row.className = selected ? "pr-row sel" : "pr-row";
      entry.input.checked = selected;
      entry.input.disabled = !canCall || writeBusy;
      if (entry.resolvedBadge) {
        if (resolvedLabel) {
          entry.resolvedBadge.textContent = resolvedLabel;
          entry.resolvedBadge.hidden = false;
        } else {
          entry.resolvedBadge.hidden = true;
        }
      }
    }

    // Local proxy section (Unix only)
    const isWindows = platform === "windows";
    const li = snap?.launcher_integration ?? null;
    proxyWrap.hidden = !snap || isWindows || li?.state === "unsupported";

    if (proxyWrap.hidden) {
      proxySub.textContent = "";
      recheckProxyBtn.hidden = true;
      disableProxyBtn.hidden = true;
      enableProxyBtn.hidden = true;
      disableConfirmRow.hidden = true;
    } else {
      const isManual = li?.state === "manual_required";
      const isEnabled = li?.enabled === true;
      const isHybrid = li?.hybrid_active === true;

      if (isManual) {
        proxyBadge.className = "tag tag-warn";
        proxyBadge.textContent = "Manual";
        proxySub.textContent =
          "Your shell profile is symlinked \u2014 follow the manual step in the banner above.";
        recheckProxyBtn.hidden = false;
        recheckProxyBtn.disabled = !canCall || state.refreshing || writeBusy;
        disableProxyBtn.hidden = true;
        enableProxyBtn.hidden = true;
        disableConfirmRow.hidden = true;
      } else if (isEnabled) {
        proxyBadge.className = isHybrid ? "tag tag-ok" : "tag tag-warn";
        proxyBadge.textContent = isHybrid ? "Active" : "Reopen Antigravity";
        proxySub.textContent =
          "Not recommended to disable \u2014 Antigravity needs the local proxy to load your enabled models.";
        recheckProxyBtn.hidden = true;
        disableProxyBtn.hidden = state.disableProxyConfirmOpen;
        disableProxyBtn.disabled = !canCall || writeBusy;
        enableProxyBtn.hidden = true;
        disableConfirmRow.hidden = !state.disableProxyConfirmOpen;
        confirmDisableBtn.textContent = state.updatingIntegration
          ? "Disabling\u2026"
          : "Confirm disable";
        confirmDisableBtn.disabled = !canCall || writeBusy;
        cancelDisableBtn.disabled = writeBusy;
      } else {
        proxyBadge.className = "tag tag-warn";
        proxyBadge.textContent = "Disabled";
        proxySub.textContent = `Local proxy is disabled. Re-enable it, then ${quitReopenSentence(platform)} to use your models.`;
        recheckProxyBtn.hidden = true;
        disableProxyBtn.hidden = true;
        enableProxyBtn.hidden = false;
        enableProxyBtn.textContent = state.updatingIntegration
          ? "Re-enabling\u2026"
          : "Re-enable proxy";
        enableProxyBtn.disabled = !canCall || writeBusy;
        disableConfirmRow.hidden = true;
      }
    }

    if (state.advFeedback) {
      advFeedbackEl.className = `status-line ${state.advFeedback.tone}`;
      advFeedbackEl.textContent = state.advFeedback.text;
      advFeedbackEl.hidden = false;
    } else {
      advFeedbackEl.hidden = true;
    }
  };

  const clearKeyInputDom = () => {
    keyInput.value = "";
  };

  return { root, update, clearKeyInputDom };
}
