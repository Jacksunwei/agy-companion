import {
  copyTextWithFallback,
  createIcon,
  el,
  quitReopenCapitalized,
  quitReopenSentence,
} from "./dom.ts";
import { hasConfiguredByomProvider } from "./state.ts";
import type { AppState } from "./types.ts";

export interface BannerActions {
  onRecheck: () => void;
  onEnableProxy: () => void;
  onToast: (message: string) => void;
}

export function buildBannerNode(
  state: AppState,
  actions: BannerActions,
): HTMLElement | null {
  if (state.standalone) {
    const b = el("div", "banner info");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("info"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, "Preview mode"),
      el(
        "span",
        "sub",
        "Open Antigravity Companion from Antigravity to manage live settings and local proxy telemetry.",
      ),
    );
    b.append(ic, txt);
    return b;
  }

  if (state.loadError && !state.snapshot) {
    const b = el("div", "banner warn");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("warn"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, "Unable to load companion state"),
      el("span", "sub", state.loadError),
    );
    const act = el("div", "b-act");
    const row = el("div", "b-act-row");
    const retryBtn = el("button", "btn btn-sm", "Re-check");
    retryBtn.type = "button";
    retryBtn.disabled = state.refreshing;
    retryBtn.addEventListener("click", () => actions.onRecheck());
    row.append(retryBtn);
    act.append(row);
    b.append(ic, txt, act);
    return b;
  }

  const snap = state.snapshot;
  if (!snap) return null;

  const platform = snap.platform;
  const li = snap.launcher_integration;
  const writeBusy =
    state.saving || state.clearing || state.updatingIntegration || state.codexBusy;

  // Priority 1: Symlinked or read-only shell profile requiring one manual line
  if (li && li.state === "manual_required") {
    const b = el("div", "banner warn");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("warn"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, "One manual step needed"),
      el(
        "span",
        "sub",
        `Your shell profile is symlinked. Add this line, then ${quitReopenSentence(platform)}:`,
      ),
    );

    const act = el("div", "b-act");
    const snippet =
      li.manual_snippet?.trim() ||
      '[ -r "$HOME/.local/share/agy-companion/env.sh" ] && . "$HOME/.local/share/agy-companion/env.sh"';
    const chip = el("div", "cmd");
    const code = el("code", null, snippet);
    code.title = snippet;
    const copyBtn = el("button", "btn btn-sm", "Copy");
    copyBtn.type = "button";
    copyBtn.addEventListener("click", () => {
      void copyTextWithFallback(snippet).then((ok) => {
        if (ok) {
          copyBtn.textContent = "Copied";
          window.setTimeout(() => {
            copyBtn.textContent = "Copy";
          }, 1600);
        } else {
          actions.onToast("Select and copy the shell snippet manually");
        }
      });
    });
    chip.append(code, copyBtn);

    const row = el("div", "b-act-row");
    const recheckBtn = el("button", "btn btn-sm", "Re-check");
    recheckBtn.type = "button";
    recheckBtn.disabled = state.refreshing || writeBusy;
    recheckBtn.addEventListener("click", () => actions.onRecheck());
    row.append(recheckBtn);

    act.append(chip, row);
    b.append(ic, txt, act);
    return b;
  }

  // Priority 2: Local proxy unreachable
  if (!snap.proxy.reachable) {
    const b = el("div", "banner warn");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("warn"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, "Local proxy offline"),
      el(
        "span",
        "sub",
        `Retrying automatically in background. If this persists, ${quitReopenSentence(platform)}.`,
      ),
    );
    b.append(ic, txt);
    return b;
  }

  // When live Antigravity LS telemetry is active and no BYOM provider is configured,
  // built-in monitoring works without proxy injection — avoid nagging about proxy
  // opt-out or reopening Antigravity for custom models the user hasn't configured.
  const byomConfigured = hasConfiguredByomProvider(snap);
  if (snap.ls_telemetry_active && !byomConfigured) {
    return null;
  }

  // Priority 3: Explicit Unix opt-out (local proxy hook disabled)
  if (
    platform !== "windows" &&
    li &&
    !li.enabled &&
    li.state !== "unsupported"
  ) {
    const b = el("div", "banner warn");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("warn"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, "Local proxy is disabled"),
      el(
        "span",
        "sub",
        `Your enabled models won't appear in Antigravity. Re-enable the proxy, then ${quitReopenSentence(platform)}.`,
      ),
    );
    const act = el("div", "b-act");
    const row = el("div", "b-act-row");
    const enableBtn = el(
      "button",
      "btn btn-primary btn-sm",
      state.updatingIntegration ? "Re-enabling\u2026" : "Re-enable proxy",
    );
    enableBtn.type = "button";
    enableBtn.disabled = !state.connected || writeBusy;
    enableBtn.addEventListener("click", () => actions.onEnableProxy());
    row.append(enableBtn);
    act.append(row);
    b.append(ic, txt, act);
    return b;
  }

  // Priority 4: Enabled (or Windows) but Antigravity has not reconnected yet
  const hybridActive = li?.hybrid_active === true;
  if (platform === "windows" && !hybridActive) {
    const b = el("div", "banner warn");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("warn"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, "Reopen Antigravity via Companion shortcut"),
      el(
        "span",
        "sub",
        'Close Antigravity and launch it from the "Antigravity (Companion)" Start Menu shortcut to use your enabled models.',
      ),
    );
    b.append(ic, txt);
    return b;
  }

  if (platform !== "windows" && li && li.enabled && !hybridActive) {
    const b = el("div", "banner warn");
    b.setAttribute("role", "alert");
    const ic = el("span", "ic");
    ic.append(createIcon("warn"));
    const txt = el("div", "txt");
    txt.append(
      el("b", null, quitReopenCapitalized(platform)),
      el(
        "span",
        "sub",
        `The local proxy is enabled. ${quitReopenCapitalized(platform)} to use your enabled models.`,
      ),
    );
    b.append(ic, txt);
    return b;
  }

  return null;
}
