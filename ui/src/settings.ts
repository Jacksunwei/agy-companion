import {
  createIcon,
  el,
  formatResolvedPlanLabel,
  quitReopenSentence,
  type TooltipController,
} from "./dom.ts";
import {
  CURATED_CODEX_MODEL_CATALOG,
  DEFAULT_ENABLED_CODEX_MODELS,
  DEFAULT_ENABLED_MODELS,
  LIVE_PROVIDERS,
  type AccountPlanChoice,
  type AppState,
  type CuratedModelInfo,
} from "./types.ts";

export interface SettingsViewActions {
  onOpenExternalUrl: (url: string, displayUrl: string) => void;
  onDraftChange: (draft: string) => void;
  onOpenGeminiKeyEdit: () => void;
  onCancelGeminiKeyEdit: () => void;
  onSaveKey: () => void;
  onTestConnection: () => void;
  onToggleClearConfirm: () => void;
  onCancelClear: () => void;
  onClearKey: () => void;
  onToggleModel: (modelId: string) => void;
  onRestoreDefaultModels: () => void;
  onStartCodexLogin: () => void;
  onOpenPendingCodexUrl: () => void;
  onCopyPendingCodexUrl: () => void;
  onCancelCodexLogin: () => void;
  onToggleCodexFallbackOpen: (open: boolean) => void;
  onCodexManualInputChange: (draft: string) => void;
  onCompleteCodexLogin: () => void;
  onTestCodexConnection: () => void;
  onToggleCodexSignOutConfirm: () => void;
  onCancelCodexSignOut: () => void;
  onDisconnectCodex: () => void;
  onToggleCodexModel: (modelId: string) => void;
  onRestoreDefaultCodexModels: () => void;
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
  focusKeyInput: () => void;
  clearCodexManualInputDom: () => void;
}

export function formatCodexStatusBadge(
  planType: string | null | undefined,
  accountHint: string | null | undefined,
): string {
  const rawPlan = (planType ?? "").trim();
  const plan = rawPlan
    ? rawPlan.charAt(0).toUpperCase() + rawPlan.slice(1).toLowerCase()
    : "";
  const hint = (accountHint ?? "").trim();
  if (plan && hint) return `${plan} \u00b7 ${hint}`;
  if (plan) return plan;
  if (hint) return hint;
  return "Signed in";
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
  root.append(el("h2", "sec-kicker", "Model providers"));
  const providerStack = el("div", "provider-stack");

  // Render live providers from typed registry (Google AI Studio today)
  const geminiProvider = LIVE_PROVIDERS[0];

  const card = el("section", "card prov-card");
  card.setAttribute("data-provider", "gemini");

  const hdr = el("div", "prov-hdr");
  const provId = el("div", "prov-id");
  provId.append(el("h3", "prov-title", geminiProvider.title));

  const statusTag = el("span", "tag");
  const statusDot = el("span", "sdot ok");
  const statusText = el("span", null, "Not set");
  statusTag.append(statusDot, statusText);
  const keyInlineFeedbackEl = el("span", "prov-inline-status");
  keyInlineFeedbackEl.setAttribute("role", "status");
  keyInlineFeedbackEl.hidden = true;
  provId.append(statusTag, keyInlineFeedbackEl);

  const geminiActions = el("div", "prov-actions");
  const getKeyBtn = el("button", "lnk", "Get API key \u2197");
  getKeyBtn.type = "button";
  getKeyBtn.title = geminiProvider.keyPortalUrl;
  getKeyBtn.addEventListener("click", () => {
    actions.onOpenExternalUrl(
      geminiProvider.keyPortalUrl,
      geminiProvider.keyPortalDisplay,
    );
  });

  const testBtn = el("button", "btn btn-sm", "Test");
  testBtn.type = "button";
  testBtn.hidden = true;
  testBtn.disabled = true;

  const replaceBtn = el("button", "lnk", "Replace");
  replaceBtn.type = "button";
  replaceBtn.hidden = true;

  const clearBtn = el("button", "lnk-danger", "Clear");
  clearBtn.type = "button";
  clearBtn.hidden = true;

  geminiActions.append(getKeyBtn, testBtn, replaceBtn, clearBtn);
  hdr.append(provId, geminiActions);
  card.append(hdr);

  // Compact write-only API key row (shown when unconfigured or when Replace is open)
  const keyRow = el("div", "key-row");
  const keyLabel = el("label", "visually-hidden", "Google AI Studio API key");
  keyLabel.setAttribute("for", "gemini-api-key");
  const keyInput = el("input", "inp mono");
  keyInput.id = "gemini-api-key";
  keyInput.type = "password";
  keyInput.autocomplete = "off";
  keyInput.spellcheck = false;
  keyInput.placeholder = "Paste Google AI Studio API key";

  const keyActRow = el("div", "act-row key-act-row");
  const saveBtn = el("button", "btn btn-primary btn-sm", "Save");
  saveBtn.type = "button";
  saveBtn.disabled = true;

  const cancelKeyEditBtn = el("button", "btn btn-sm", "Cancel");
  cancelKeyEditBtn.type = "button";
  cancelKeyEditBtn.hidden = true;

  keyInput.addEventListener("input", () => {
    actions.onDraftChange(keyInput.value);
  });
  keyInput.addEventListener("keydown", (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter") {
      event.preventDefault();
      if (!saveBtn.disabled) {
        actions.onSaveKey();
      }
    } else if (event.key === "Escape" && !cancelKeyEditBtn.hidden) {
      event.preventDefault();
      event.stopPropagation();
      actions.onCancelGeminiKeyEdit();
    }
  });
  saveBtn.addEventListener("click", () => actions.onSaveKey());
  cancelKeyEditBtn.addEventListener("click", () => actions.onCancelGeminiKeyEdit());
  testBtn.addEventListener("click", () => actions.onTestConnection());
  replaceBtn.addEventListener("click", () => actions.onOpenGeminiKeyEdit());
  clearBtn.addEventListener("click", () => actions.onToggleClearConfirm());

  keyActRow.append(saveBtn, cancelKeyEditBtn);
  keyRow.append(keyLabel, keyInput, keyActRow);
  card.append(keyRow);

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
  mh.append(el("h4", "sec-t", "Enabled models"));
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

  // Provider 2: OpenAI Codex (ChatGPT OAuth)
  const codexCard = el("section", "card prov-card");
  codexCard.setAttribute("data-provider", "codex");

  const codexHdr = el("div", "prov-hdr");
  const codexProvId = el("div", "prov-id");
  codexProvId.append(el("h3", "prov-title", "OpenAI Codex"));

  const codexStatusTag = el("span", "tag codex-status-tag");
  const codexStatusDot = el("span", "sdot ok");
  codexStatusDot.hidden = true;
  const codexStatusText = el("span", "codex-status-text", "Not signed in");
  codexStatusTag.append(codexStatusDot, codexStatusText);
  const codexInlineFeedbackEl = el("span", "prov-inline-status");
  codexInlineFeedbackEl.setAttribute("role", "status");
  codexInlineFeedbackEl.hidden = true;
  codexProvId.append(codexStatusTag, codexInlineFeedbackEl);

  const codexActions = el("div", "prov-actions");
  const codexSignInBtn = el("button", "btn btn-primary btn-sm", "Sign in with ChatGPT \u2197");
  codexSignInBtn.type = "button";
  codexSignInBtn.addEventListener("click", () => actions.onStartCodexLogin());

  const codexTestBtn = el("button", "btn btn-sm", "Test");
  codexTestBtn.type = "button";
  codexTestBtn.hidden = true;
  codexTestBtn.addEventListener("click", () => actions.onTestCodexConnection());

  const codexSignOutBtn = el("button", "lnk-danger", "Sign out");
  codexSignOutBtn.type = "button";
  codexSignOutBtn.hidden = true;
  codexSignOutBtn.addEventListener("click", () => actions.onToggleCodexSignOutConfirm());

  codexActions.append(codexSignInBtn, codexTestBtn, codexSignOutBtn);
  codexHdr.append(codexProvId, codexActions);
  codexCard.append(codexHdr);

  // State 2: Pending sign-in panel (2s poll + sync open + copy + cancel + inline paste fallback)
  const codexPendingWrap = el("div", "codex-pending");
  codexPendingWrap.hidden = true;
  codexPendingWrap.append(
    el("div", "opt-sub codex-pending-msg", "Complete sign-in in your browser\u2026"),
  );

  const codexPendingActRow = el("div", "act-row");
  const codexOpenPageBtn = el("button", "btn btn-primary btn-sm", "Open sign-in page \u2197");
  codexOpenPageBtn.type = "button";
  codexOpenPageBtn.addEventListener("click", () => actions.onOpenPendingCodexUrl());

  const codexCopyLinkBtn = el("button", "btn btn-sm", "Copy link");
  codexCopyLinkBtn.type = "button";
  codexCopyLinkBtn.addEventListener("click", () => actions.onCopyPendingCodexUrl());

  const codexCancelLoginBtn = el("button", "btn btn-sm", "Cancel");
  codexCancelLoginBtn.type = "button";
  codexCancelLoginBtn.addEventListener("click", () => actions.onCancelCodexLogin());

  codexPendingActRow.append(codexOpenPageBtn, codexCopyLinkBtn, codexCancelLoginBtn);
  codexPendingWrap.append(codexPendingActRow);

  const codexFallbackDetails = el("details", "codex-fallback");
  codexFallbackDetails.open = false;
  codexFallbackDetails.addEventListener("toggle", () => {
    actions.onToggleCodexFallbackOpen(codexFallbackDetails.open);
  });
  const codexFallbackSum = el("summary", "codex-fallback-sum", "Having trouble? Paste callback URL");
  codexFallbackDetails.append(codexFallbackSum);

  const codexFallbackBody = el("div", "codex-fallback-body key-row");
  const codexManualLabel = el(
    "label",
    "visually-hidden",
    "OpenAI Codex callback URL or authorization code",
  );
  codexManualLabel.setAttribute("for", "codex-callback-input");
  const codexManualInput = el("input", "inp mono");
  codexManualInput.id = "codex-callback-input";
  codexManualInput.type = "text";
  codexManualInput.autocomplete = "off";
  codexManualInput.spellcheck = false;
  codexManualInput.placeholder = "Paste http://localhost:1455/auth/callback?code=...";
  codexManualInput.addEventListener("input", () => {
    actions.onCodexManualInputChange(codexManualInput.value);
  });
  codexManualInput.addEventListener("keydown", (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter") {
      event.preventDefault();
      if (!codexCompleteBtn.disabled) {
        actions.onCompleteCodexLogin();
      }
    }
  });

  const codexFallbackActRow = el("div", "act-row key-act-row");
  const codexCompleteBtn = el("button", "btn btn-primary btn-sm", "Complete sign-in");
  codexCompleteBtn.type = "button";
  codexCompleteBtn.disabled = true;
  codexCompleteBtn.addEventListener("click", () => actions.onCompleteCodexLogin());
  codexFallbackActRow.append(codexCompleteBtn);
  codexFallbackBody.append(codexManualLabel, codexManualInput, codexFallbackActRow);
  codexFallbackDetails.append(codexFallbackBody);
  codexPendingWrap.append(codexFallbackDetails);
  codexCard.append(codexPendingWrap);

  // Inline 2-click Sign out confirm row
  const codexSignOutConfirmRow = el("div", "confirm-row");
  codexSignOutConfirmRow.hidden = true;
  codexSignOutConfirmRow.append(el("span", null, "Sign out of OpenAI Codex?"));
  const codexConfirmSignOutBtn = el("button", "btn btn-sm btn-danger-solid", "Confirm sign out");
  codexConfirmSignOutBtn.type = "button";
  codexConfirmSignOutBtn.addEventListener("click", () => actions.onDisconnectCodex());
  const codexCancelSignOutBtn = el("button", "btn btn-sm", "Cancel");
  codexCancelSignOutBtn.type = "button";
  codexCancelSignOutBtn.addEventListener("click", () => actions.onCancelCodexSignOut());
  codexSignOutConfirmRow.append(codexConfirmSignOutBtn, codexCancelSignOutBtn);
  codexCard.append(codexSignOutConfirmRow);

  // Codex status feedback line (shared across signed-out, pending, signed-in)
  const codexFeedbackEl = el("div", "status-line");
  codexFeedbackEl.setAttribute("role", "status");
  codexFeedbackEl.hidden = true;
  codexCard.append(codexFeedbackEl);

  // Codex Enabled models (visible when signed in)
  const codexModSec = el("div", "divider");
  codexModSec.hidden = true;
  const cmh = el("div", "sec-h");
  cmh.append(el("h4", "sec-t", "Enabled models"));
  const codexModelCountEl = el(
    "span",
    "sec-right sec-count num",
    `${DEFAULT_ENABLED_CODEX_MODELS.length} enabled`,
  );
  cmh.append(codexModelCountEl);
  codexModSec.append(cmh);

  const codexChipStrip = el("div", "chip-strip");
  codexChipStrip.setAttribute("role", "group");
  codexChipStrip.setAttribute("aria-label", "Enabled OpenAI Codex models");

  const codexChipMap = new Map<string, { btn: HTMLButtonElement; info: CuratedModelInfo }>();
  for (const model of CURATED_CODEX_MODEL_CATALOG) {
    const chip = el("button", "m-chip");
    chip.type = "button";
    chip.setAttribute("role", "checkbox");
    chip.setAttribute("aria-checked", String(model.defaultOn));

    const chk = el("span", "chk");
    chk.append(createIcon("check"));
    chip.append(chk, el("span", null, model.label));

    chip.addEventListener("mouseenter", () => tooltip.show(chip, model));
    chip.addEventListener("mouseleave", () => tooltip.scheduleHide());
    chip.addEventListener("focus", () => tooltip.show(chip, model));
    chip.addEventListener("blur", () => tooltip.hideNow());
    chip.addEventListener("click", () => {
      tooltip.hideNow();
      actions.onToggleCodexModel(model.id);
    });

    codexChipMap.set(model.id, { btn: chip, info: model });
    codexChipStrip.append(chip);
  }
  codexModSec.append(codexChipStrip);

  const codexZeroModelsBox = el("div", "zero-models-note");
  codexZeroModelsBox.hidden = true;
  codexZeroModelsBox.append(
    el("span", null, "No models enabled \u2014 none will appear in Antigravity's selector."),
  );
  const codexRestoreDefaultsBtn = el("button", "lnk", "Restore defaults");
  codexRestoreDefaultsBtn.type = "button";
  codexRestoreDefaultsBtn.addEventListener("click", () => actions.onRestoreDefaultCodexModels());
  codexZeroModelsBox.append(codexRestoreDefaultsBtn);
  codexModSec.append(codexZeroModelsBox);

  const codexModelFeedbackEl = el("div", "status-line");
  codexModelFeedbackEl.setAttribute("role", "status");
  codexModelFeedbackEl.hidden = true;
  codexModSec.append(codexModelFeedbackEl);

  codexCard.append(codexModSec);
  providerStack.append(codexCard);
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
  const planFieldset = el("fieldset", "plan-fieldset");
  planFieldset.append(el("legend", "field-lbl", "Antigravity plan"));
  const prList = el("div", "pr");

  const planRows = new Map<
    AccountPlanChoice,
    {
      row: HTMLLabelElement;
      input: HTMLInputElement;
      resolvedBadge: HTMLSpanElement | null;
    }
  >();

  for (const opt of PLAN_OPTIONS) {
    const radioId = `antigravity-plan-${opt.id}`;
    const row = el("label", "pr-row");
    row.setAttribute("for", radioId);
    const rWrap = el("span", "pr-radio");
    const inp = el("input", "pr-inp");
    inp.id = radioId;
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
  planFieldset.append(prList);
  advBody.append(planFieldset);

  // 2. Local proxy (Unix only — hidden on Windows)
  const proxyWrap = el("div", "adv-sep");
  const proxyRow = el("div", "proxy-ctrl-row");
  const proxyInfo = el("div", "proxy-ctrl-info");
  const proxyHdr = el("div", "sec-h proxy-ctrl-hdr");
  proxyHdr.append(el("h3", "sec-t", "Local proxy"));
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
    const writeBusy =
      state.saving || state.clearing || state.updatingIntegration || state.codexBusy;
    const hasDraft = state.apiKeyDraft.trim().length > 0;

    // Status pill & symmetric header actions
    const geminiKeyEditVisible = !apiKeyConfigured || state.geminiKeyEditOpen;
    if (apiKeyConfigured) {
      statusTag.className = "tag tag-ok mono";
      statusDot.hidden = false;
      statusText.textContent = apiKeyHint || "Configured";
    } else {
      statusTag.className = "tag";
      statusDot.hidden = true;
      statusText.textContent = "Not set";
    }

    getKeyBtn.hidden = !geminiKeyEditVisible;
    testBtn.hidden = geminiKeyEditVisible;
    replaceBtn.hidden = geminiKeyEditVisible;
    clearBtn.hidden = geminiKeyEditVisible;

    // Compact inline key row (visible when unconfigured or replacing key)
    keyRow.hidden = !geminiKeyEditVisible;
    keyInput.placeholder = apiKeyConfigured
      ? "Paste new key to replace"
      : "Paste Google AI Studio API key";
    keyInput.disabled = !canCall || state.saving || state.clearing;
    if (document.activeElement !== keyInput && keyInput.value !== state.apiKeyDraft) {
      keyInput.value = state.apiKeyDraft;
    }

    saveBtn.textContent = state.saving ? "Saving\u2026" : "Save";
    saveBtn.disabled = !canCall || !hasDraft || writeBusy;
    cancelKeyEditBtn.hidden = !(apiKeyConfigured && state.geminiKeyEditOpen);
    cancelKeyEditBtn.disabled = writeBusy;

    testBtn.textContent = state.testing ? "Testing\u2026" : "Test";
    testBtn.disabled = !canCall || !apiKeyConfigured || hasDraft || writeBusy || state.testing;

    replaceBtn.disabled = !canCall || writeBusy;
    clearBtn.disabled = !canCall || writeBusy;

    clearConfirmRow.hidden = !(
      state.clearConfirmOpen &&
      apiKeyConfigured &&
      !state.geminiKeyEditOpen
    );
    confirmClearBtn.textContent = state.clearing ? "Clearing\u2026" : "Confirm clear";
    confirmClearBtn.disabled = !canCall || writeBusy;
    cancelClearBtn.disabled = writeBusy;

    if (state.keyFeedback) {
      if (state.keyFeedback.tone === "err") {
        keyInlineFeedbackEl.hidden = true;
        keyFeedbackEl.className = `status-line ${state.keyFeedback.tone}`;
        keyFeedbackEl.textContent = state.keyFeedback.text;
        keyFeedbackEl.hidden = false;
      } else {
        keyInlineFeedbackEl.className = `prov-inline-status ${state.keyFeedback.tone}`;
        keyInlineFeedbackEl.textContent = state.keyFeedback.text;
        keyInlineFeedbackEl.hidden = false;
        keyFeedbackEl.hidden = true;
      }
    } else {
      keyInlineFeedbackEl.hidden = true;
      keyFeedbackEl.hidden = true;
    }

    // Enabled models chips (hidden until configured, symmetric with OpenAI Codex)
    modSec.hidden = !apiKeyConfigured;
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

    // OpenAI Codex card
    const codex = snap?.config.providers.codex;
    const codexAuthenticated = codex?.authenticated === true;
    const codexPending =
      snap?.codex_login_pending === true ||
      (!codexAuthenticated && state.codexAuthorizeUrl !== null);
    const codexEnabledSet = new Set(codex?.enabled_models ?? DEFAULT_ENABLED_CODEX_MODELS);

    if (codexPending) {
      codexStatusTag.className = "tag tag-warn codex-status-tag";
      codexStatusDot.className = "sdot warn";
      codexStatusDot.hidden = false;
      codexStatusText.textContent = "Signing in\u2026";
      codexStatusTag.removeAttribute("title");
    } else if (codexAuthenticated) {
      const badgeText = formatCodexStatusBadge(codex?.plan_type, codex?.account_hint);
      codexStatusTag.className = "tag tag-ok codex-status-tag";
      codexStatusDot.className = "sdot ok";
      codexStatusDot.hidden = false;
      codexStatusText.textContent = badgeText;
      codexStatusTag.title = badgeText;
    } else {
      codexStatusTag.className = "tag codex-status-tag";
      codexStatusDot.hidden = true;
      codexStatusText.textContent = "Not signed in";
      codexStatusTag.removeAttribute("title");
    }

    codexActions.hidden = codexPending;
    codexSignInBtn.hidden = codexAuthenticated || codexPending;
    codexSignInBtn.textContent = state.codexBusy
      ? "Starting sign-in\u2026"
      : "Sign in with ChatGPT \u2197";
    codexSignInBtn.disabled = !canCall || writeBusy;

    codexTestBtn.hidden = !codexAuthenticated || codexPending;
    codexTestBtn.textContent = state.codexTesting ? "Testing\u2026" : "Test";
    codexTestBtn.disabled = !canCall || !codexAuthenticated || writeBusy || state.codexTesting;

    codexSignOutBtn.hidden = !codexAuthenticated || codexPending;
    codexSignOutBtn.disabled = !canCall || writeBusy;

    codexPendingWrap.hidden = !codexPending;
    const hasPendingUrl = state.codexAuthorizeUrl !== null;
    codexOpenPageBtn.hidden = !hasPendingUrl;
    codexOpenPageBtn.disabled = !hasPendingUrl;
    codexCopyLinkBtn.hidden = !hasPendingUrl;
    codexCopyLinkBtn.disabled = !hasPendingUrl;
    codexCancelLoginBtn.disabled = !canCall || writeBusy;

    if (codexFallbackDetails.open !== state.codexManualFallbackOpen) {
      codexFallbackDetails.open = state.codexManualFallbackOpen;
    }
    codexManualInput.disabled = !canCall || state.codexBusy;
    if (
      document.activeElement !== codexManualInput &&
      codexManualInput.value !== state.codexManualInputDraft
    ) {
      codexManualInput.value = state.codexManualInputDraft;
    }
    codexCompleteBtn.textContent = state.codexBusy ? "Completing\u2026" : "Complete sign-in";
    codexCompleteBtn.disabled =
      !canCall || writeBusy || state.codexManualInputDraft.trim().length === 0;

    codexSignOutConfirmRow.hidden = !(
      state.codexSignOutConfirmOpen &&
      codexAuthenticated &&
      !codexPending
    );
    codexConfirmSignOutBtn.textContent = state.codexBusy ? "Signing out\u2026" : "Confirm sign out";
    codexConfirmSignOutBtn.disabled = !canCall || writeBusy;
    codexCancelSignOutBtn.disabled = writeBusy;

    if (state.codexFeedback) {
      if (state.codexFeedback.tone === "err" || codexPending) {
        codexInlineFeedbackEl.hidden = true;
        codexFeedbackEl.className = `status-line ${state.codexFeedback.tone}`;
        codexFeedbackEl.textContent = state.codexFeedback.text;
        codexFeedbackEl.hidden = false;
      } else {
        codexInlineFeedbackEl.className = `prov-inline-status ${state.codexFeedback.tone}`;
        codexInlineFeedbackEl.textContent = state.codexFeedback.text;
        codexInlineFeedbackEl.hidden = false;
        codexFeedbackEl.hidden = true;
      }
    } else {
      codexInlineFeedbackEl.hidden = true;
      codexFeedbackEl.hidden = true;
    }

    codexModSec.hidden = !codexAuthenticated || codexPending;
    const codexEnabledCount = codexEnabledSet.size;
    codexModelCountEl.textContent = `${codexEnabledCount} enabled`;
    for (const [id, entry] of codexChipMap.entries()) {
      entry.btn.setAttribute("aria-checked", String(codexEnabledSet.has(id)));
      entry.btn.disabled = !canCall || writeBusy;
    }
    codexZeroModelsBox.hidden = codexEnabledCount > 0;
    codexRestoreDefaultsBtn.disabled = !canCall || writeBusy;

    if (codexEnabledCount > 0 && state.codexModelFeedback) {
      codexModelFeedbackEl.className = `status-line ${state.codexModelFeedback.tone}`;
      codexModelFeedbackEl.textContent = state.codexModelFeedback.text;
      codexModelFeedbackEl.hidden = false;
    } else {
      codexModelFeedbackEl.hidden = true;
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

  const focusKeyInput = () => {
    keyInput.focus();
  };

  const clearCodexManualInputDom = () => {
    codexManualInput.value = "";
  };

  return { root, update, clearKeyInputDom, focusKeyInput, clearCodexManualInputDom };
}
