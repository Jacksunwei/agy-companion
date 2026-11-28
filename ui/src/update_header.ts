import { copyTextWithFallback, el, supportsNativePopover } from "./dom.ts";
import {
  compareSemVer,
  type AppState,
  type HostPlatform,
  type SettingsSnapshot,
  type UpdateStatusSnapshot,
} from "./types.ts";

export const UPGRADE_POLL_INTERVAL_MS = 2_000;
export const UPGRADE_DISCONNECT_GRACE_MS = 10_000;

export interface UpdateHeaderActions {
  onCheckUpdate: () => void;
  onStartUpgrade: () => void;
  onReloadPanel: () => void;
  onToast: (message: string) => void;
}

export interface UpdateHeaderRenderState {
  connected: boolean;
  standalone: boolean;
  snapshot: SettingsSnapshot | null;
  checkingUpdate: boolean;
  holdingUpgrading: boolean;
  optimisticUpdate: UpdateStatusSnapshot | null;
}

export interface UpdateHeaderController {
  versionBtn: HTMLButtonElement;
  upgradeBtn: HTMLButtonElement;
  versionPopover: HTMLElement;
  upgradePopover: HTMLElement;
  update: (renderState: UpdateHeaderRenderState) => void;
  closePopovers: () => void;
  isAnyPopoverOpen: () => boolean;
  isVersionPopoverOpen: () => boolean;
  isUpgradePopoverOpen: () => boolean;
  destroy: () => void;
}

function formatVersionStatusLine(update: UpdateStatusSnapshot | null): string {
  if (!update) return "Up to date";
  switch (update.state) {
    case "available":
      return update.latest_version ? `v${update.latest_version} available` : "Update available";
    case "upgrading":
      return "Upgrading\u2026";
    case "upgraded":
      return update.latest_version
        ? `Updated to v${update.latest_version}`
        : "Updated";
    case "failed":
      return "Upgrade failed";
    case "disabled":
      return "Checks disabled";
    case "up_to_date":
    default:
      return "Up to date";
  }
}

function buildCopyCommandChip(
  command: string,
  onToast: (message: string) => void,
): HTMLElement {
  const chip = el("div", "cmd");
  const code = el("code", null, command);
  code.title = command;
  const copyBtn = el("button", "btn btn-sm", "Copy");
  copyBtn.type = "button";
  copyBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    void copyTextWithFallback(command).then((ok) => {
      if (ok) {
        copyBtn.textContent = "Copied";
        window.setTimeout(() => {
          copyBtn.textContent = "Copy";
        }, 1600);
      } else {
        onToast("Select and copy the upgrade command manually");
      }
    });
  });
  chip.append(code, copyBtn);
  return chip;
}

interface AnchoredPopoverHandle {
  node: HTMLElement;
  open: () => void;
  close: () => void;
  toggle: () => void;
  isOpen: () => boolean;
  destroy: () => void;
}

function createAnchoredPopover(
  popoverEl: HTMLElement,
  triggerEl: HTMLButtonElement,
  onOpenChange?: (isOpen: boolean) => void,
): AnchoredPopoverHandle {
  const supportsPopover = supportsNativePopover(popoverEl);
  if (!supportsPopover) {
    popoverEl.hidden = true;
  }

  let openState = false;

  const isNativeOpen = (): boolean =>
    typeof popoverEl.matches === "function" ? popoverEl.matches(":popover-open") : openState;

  const isOpen = (): boolean => (supportsPopover ? isNativeOpen() : openState);

  const setOpen = (next: boolean) => {
    if (supportsPopover) {
      const currentlyOpen = isNativeOpen();
      if (next && !currentlyOpen) {
        try {
          popoverEl.showPopover({ source: triggerEl });
        } catch {
          // Ignore disconnected node or unsupported state
        }
      } else if (!next && currentlyOpen) {
        try {
          popoverEl.hidePopover();
        } catch {
          // Ignore disconnected node or unsupported state
        }
      }
      openState = next;
    } else {
      openState = next;
      popoverEl.hidden = !openState;
    }
    triggerEl.setAttribute("aria-expanded", String(openState));
    onOpenChange?.(openState);
  };

  const onPopoverToggle = (e: Event) => {
    const toggleEvent = e as ToggleEvent;
    const nextOpen =
      typeof toggleEvent.newState === "string"
        ? toggleEvent.newState === "open"
        : isNativeOpen();
    openState = nextOpen;
    triggerEl.setAttribute("aria-expanded", String(openState));
    onOpenChange?.(openState);
  };

  const onDocumentClick = (e: MouseEvent) => {
    if (!openState) return;
    const target = e.target;
    if (target instanceof Node && (triggerEl.contains(target) || popoverEl.contains(target))) {
      return;
    }
    setOpen(false);
  };

  if (supportsPopover) {
    popoverEl.addEventListener("toggle", onPopoverToggle);
  } else {
    document.addEventListener("click", onDocumentClick);
  }

  return {
    node: popoverEl,
    open: () => {
      if (!isOpen()) setOpen(true);
    },
    close: () => {
      if (isOpen()) setOpen(false);
    },
    toggle: () => {
      setOpen(!isOpen());
    },
    isOpen,
    destroy: () => {
      if (supportsPopover) {
        popoverEl.removeEventListener("toggle", onPopoverToggle);
        if (isNativeOpen()) {
          try {
            popoverEl.hidePopover();
          } catch {
            // Ignore detached node
          }
        }
      } else {
        document.removeEventListener("click", onDocumentClick);
      }
      popoverEl.remove();
    },
  };
}

export function createUpdateHeaderController(
  brandEl: HTMLElement,
  rootEl: HTMLElement,
  actions: UpdateHeaderActions,
): UpdateHeaderController {
  // Clickable header version pill (replaces static span.hdr-ver)
  const versionBtn = el("button", "hdr-ver");
  versionBtn.type = "button";
  versionBtn.hidden = true;
  versionBtn.setAttribute("aria-expanded", "false");
  versionBtn.setAttribute("aria-controls", "version-popover");
  versionBtn.setAttribute("aria-label", "Version and update status");

  // Contextual upgrade pill (rendered directly beside version pill ONLY when active)
  const upgradeBtn = el("button", "hdr-upgrade-pill");
  upgradeBtn.type = "button";
  upgradeBtn.hidden = true;
  upgradeBtn.setAttribute("aria-expanded", "false");
  upgradeBtn.setAttribute("aria-controls", "upgrade-popover");

  brandEl.append(versionBtn, upgradeBtn);

  // Version info + manual check popover
  const versionPopover = el("div", "hdr-popover hdr-ver-popover");
  versionPopover.id = "version-popover";
  versionPopover.setAttribute("popover", "auto");
  versionPopover.setAttribute("role", "region");
  versionPopover.setAttribute("aria-label", "Companion version");

  const verPopMeta = el("div", "hdr-pop-meta");
  const verPopTitle = el("span", "hdr-pop-title mono num", "");
  const verPopStatus = el("span", "hdr-pop-status", "Up to date");
  verPopMeta.append(verPopTitle, verPopStatus);

  const checkBtn = el("button", "btn btn-sm hdr-pop-action", "Check for updates");
  checkBtn.type = "button";
  checkBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    actions.onCheckUpdate();
  });

  versionPopover.append(verPopMeta, checkBtn);

  // Upgrade detail / fallback / failure popover
  const upgradePopover = el("div", "hdr-popover hdr-upgrade-popover");
  upgradePopover.id = "upgrade-popover";
  upgradePopover.setAttribute("popover", "auto");
  upgradePopover.setAttribute("role", "region");
  upgradePopover.setAttribute("aria-label", "Upgrade details");

  rootEl.append(versionPopover, upgradePopover);

  const verPopHandle = createAnchoredPopover(versionPopover, versionBtn);
  const upgPopHandle = createAnchoredPopover(upgradePopover, upgradeBtn);

  let latestRenderState: UpdateHeaderRenderState | null = null;
  let lastEffectiveState: string | null = null;
  let lastUpgradePopoverSig: string | null = null;

  function resolveEffectiveUpdate(renderState: UpdateHeaderRenderState): UpdateStatusSnapshot | null {
    const base = renderState.optimisticUpdate ?? renderState.snapshot?.update ?? null;
    if (renderState.holdingUpgrading) {
      if (base) {
        return { ...base, state: "upgrading" };
      }
      return {
        state: "upgrading",
        channel: "npx",
        current_version: renderState.snapshot?.binary_version || "",
        latest_version: null,
        can_one_click_upgrade: true,
        fallback_command: "npx -y agy-companion@latest install",
        checked_at_ms: null,
        error: null,
      };
    }
    return base;
  }

  versionBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    upgPopHandle.close();
    verPopHandle.toggle();
  });

  upgradeBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (!latestRenderState) return;
    const effective = resolveEffectiveUpdate(latestRenderState);
    if (!effective) return;
    const platform: HostPlatform = latestRenderState.snapshot?.platform ?? "macos";

    if (effective.state === "available") {
      if (effective.can_one_click_upgrade) {
        verPopHandle.close();
        upgPopHandle.close();
        actions.onStartUpgrade();
      } else {
        verPopHandle.close();
        upgPopHandle.toggle();
      }
      return;
    }

    if (effective.state === "upgraded") {
      if (platform !== "windows") {
        verPopHandle.close();
        upgPopHandle.close();
        actions.onReloadPanel();
      } else {
        verPopHandle.close();
        upgPopHandle.toggle();
      }
      return;
    }

    if (effective.state === "failed") {
      verPopHandle.close();
      upgPopHandle.toggle();
    }
  });

  function renderUpgradePopover(
    effective: UpdateStatusSnapshot | null,
    platform: HostPlatform,
    canInteract: boolean,
  ): void {
    if (!effective) {
      upgradePopover.replaceChildren();
      lastUpgradePopoverSig = null;
      return;
    }

    const sig = JSON.stringify([
      effective.state,
      effective.channel,
      effective.latest_version,
      effective.can_one_click_upgrade,
      effective.fallback_command,
      effective.error,
      platform,
      canInteract,
    ]);
    if (sig === lastUpgradePopoverSig) return;
    lastUpgradePopoverSig = sig;

    if (effective.state === "available" && !effective.can_one_click_upgrade) {
      const msg = el(
        "p",
        "hdr-pop-msg",
        `${effective.channel} not found on PATH. Run manually:`,
      );
      const chip = buildCopyCommandChip(effective.fallback_command, actions.onToast);
      upgradePopover.replaceChildren(msg, chip);
      return;
    }

    if (effective.state === "upgraded" && platform === "windows") {
      const ver = effective.latest_version ? `v${effective.latest_version}` : "the latest version";
      const msg = el("p", "hdr-pop-msg", `Proxy updated to ${ver}.`);
      upgradePopover.replaceChildren(msg);
      return;
    }

    if (effective.state === "failed") {
      const errMsg = el(
        "p",
        "hdr-pop-msg is-warn",
        effective.error?.trim() || "Upgrade failed.",
      );
      const chip = buildCopyCommandChip(effective.fallback_command, actions.onToast);
      const nodes: HTMLElement[] = [errMsg, chip];
      if (effective.can_one_click_upgrade) {
        const actRow = el("div", "hdr-pop-act-row");
        const retryBtn = el("button", "btn btn-primary btn-sm", "Retry");
        retryBtn.type = "button";
        retryBtn.disabled = !canInteract;
        retryBtn.addEventListener("click", (e) => {
          e.stopPropagation();
          upgPopHandle.close();
          actions.onStartUpgrade();
        });
        actRow.append(retryBtn);
        nodes.push(actRow);
      }
      upgradePopover.replaceChildren(...nodes);
      return;
    }

    upgradePopover.replaceChildren();
  }

  function update(renderState: UpdateHeaderRenderState): void {
    latestRenderState = renderState;
    const snap = renderState.snapshot;
    const platform: HostPlatform = snap?.platform ?? "macos";
    const effective = resolveEffectiveUpdate(renderState);
    const canInteract = renderState.connected && !renderState.standalone;

    // 1. Version pill
    const verText = snap?.binary_version || effective?.current_version || "";
    versionBtn.textContent = verText;
    versionBtn.hidden = !verText;

    const channelLabel = effective?.channel ?? null;
    verPopTitle.textContent = verText
      ? channelLabel
        ? `v${verText} \u00b7 ${channelLabel}`
        : `v${verText}`
      : "";
    verPopStatus.textContent = formatVersionStatusLine(effective);

    checkBtn.textContent = renderState.checkingUpdate ? "Checking\u2026" : "Check for updates";
    checkBtn.disabled =
      !canInteract || renderState.checkingUpdate || effective?.state === "upgrading";

    // 2. Contextual Upgrade Pill
    const prevEffectiveState = lastEffectiveState;
    lastEffectiveState = effective?.state ?? null;

    if (
      !effective ||
      effective.state === "up_to_date" ||
      effective.state === "disabled"
    ) {
      upgradeBtn.hidden = true;
      upgradeBtn.replaceChildren();
      upgradeBtn.title = "";
      upgPopHandle.close();
      renderUpgradePopover(null, platform, canInteract);
      return;
    }

    upgradeBtn.hidden = false;

    if (effective.state === "available") {
      upgradeBtn.className = "hdr-upgrade-pill is-available";
      upgradeBtn.disabled = !canInteract;
      const targetLabel = effective.latest_version
        ? `\u2191 Upgrade to ${effective.latest_version}`
        : "\u2191 Upgrade";
      upgradeBtn.textContent = targetLabel;
      upgradeBtn.title = effective.fallback_command;
      renderUpgradePopover(effective, platform, canInteract);
    } else if (effective.state === "upgrading") {
      upgradeBtn.className = "hdr-upgrade-pill is-upgrading";
      upgradeBtn.disabled = true;
      const spinner = el("span", "hdr-spinner");
      spinner.setAttribute("aria-hidden", "true");
      upgradeBtn.replaceChildren(spinner, el("span", null, "Upgrading\u2026"));
      upgradeBtn.title = `Upgrading via ${effective.channel}\u2026`;
      upgPopHandle.close();
      renderUpgradePopover(null, platform, canInteract);
    } else if (effective.state === "upgraded") {
      upgradeBtn.className = "hdr-upgrade-pill is-upgraded";
      upgradeBtn.disabled = !canInteract;
      if (platform === "windows") {
        const label = effective.latest_version
          ? `\u2713 Updated to ${effective.latest_version}`
          : "\u2713 Updated";
        upgradeBtn.textContent = label;
        upgradeBtn.title = effective.latest_version
          ? `Proxy updated to ${effective.latest_version}`
          : "Proxy updated";
        renderUpgradePopover(effective, platform, canInteract);
      } else {
        upgradeBtn.textContent = "\u2713 Reload panel";
        upgradeBtn.title = "Reload panel to load the updated version";
        upgPopHandle.close();
        renderUpgradePopover(null, platform, canInteract);
      }
    } else if (effective.state === "failed") {
      upgradeBtn.className = "hdr-upgrade-pill is-failed";
      upgradeBtn.disabled = false;
      upgradeBtn.textContent = "! Upgrade failed";
      upgradeBtn.title = effective.error?.trim() || "Upgrade failed";
      renderUpgradePopover(effective, platform, canInteract);
      if (prevEffectiveState !== "failed") {
        verPopHandle.close();
        upgPopHandle.open();
      }
    }
  }

  const closePopovers = () => {
    verPopHandle.close();
    upgPopHandle.close();
  };

  const destroy = () => {
    verPopHandle.destroy();
    upgPopHandle.destroy();
    versionBtn.remove();
    upgradeBtn.remove();
  };

  return {
    versionBtn,
    upgradeBtn,
    versionPopover,
    upgradePopover,
    update,
    closePopovers,
    isAnyPopoverOpen: () => verPopHandle.isOpen() || upgPopHandle.isOpen(),
    isVersionPopoverOpen: () => verPopHandle.isOpen(),
    isUpgradePopoverOpen: () => upgPopHandle.isOpen(),
    destroy,
  };
}

export interface UpgradePollerDeps {
  pollUpdateStatus: () => Promise<
    { ok: true; data: UpdateStatusSnapshot } | { ok: false; error: string }
  >;
  getPlatform: () => HostPlatform;
  onUpdateStatus: (status: UpdateStatusSnapshot) => void;
  onHoldingChange: (holding: boolean) => void;
  onDisconnectTimeout: () => void;
  reloadPage: () => void;
  nowMs?: () => number;
  scheduleFn?: (cb: () => void, ms: number) => number;
  clearFn?: (id: number) => void;
  pollIntervalMs?: number;
  disconnectGraceMs?: number;
}

export interface UpgradePoller {
  notifyUpgradeStarted: (status: UpdateStatusSnapshot) => void;
  syncSettingsSnapshot: (snap: SettingsSnapshot) => void;
  isPolling: () => boolean;
  isHoldingUpgrading: () => boolean;
  didAutoReload: () => boolean;
  destroy: () => void;
}

function versionReachedOrExceeded(current: string, target: string | null): boolean {
  if (!target) return false;
  const cmp = compareSemVer(current, target);
  if (cmp !== null) return cmp >= 0;
  return current.trim() === target.trim();
}

export function createUpgradePoller(deps: UpgradePollerDeps): UpgradePoller {
  const nowMs = deps.nowMs ?? (() => Date.now());
  const scheduleFn = deps.scheduleFn ?? ((cb, ms) => window.setTimeout(cb, ms));
  const clearFn = deps.clearFn ?? ((id) => window.clearTimeout(id));
  const pollIntervalMs = deps.pollIntervalMs ?? UPGRADE_POLL_INTERVAL_MS;
  const disconnectGraceMs = deps.disconnectGraceMs ?? UPGRADE_DISCONNECT_GRACE_MS;

  let timerId: number | null = null;
  let inFlight = false;
  let destroyed = false;
  let initiatedFromThisTab = false;
  let targetVersion: string | null = null;
  let reloadedOnce = false;
  let firstDisconnectAtMs: number | null = null;
  let holdingUpgrading = false;

  const setHolding = (next: boolean) => {
    if (holdingUpgrading !== next) {
      holdingUpgrading = next;
      deps.onHoldingChange(next);
    }
  };

  const stopTimer = () => {
    if (timerId !== null) {
      clearFn(timerId);
      timerId = null;
    }
  };

  const triggerAutoReloadOnce = (): boolean => {
    if (reloadedOnce || deps.getPlatform() === "windows") return false;
    reloadedOnce = true;
    stopTimer();
    setHolding(false);
    deps.reloadPage();
    return true;
  };

  const shouldAutoReloadOnUnix = (
    currentVersion: string,
    updateState: UpdateStatusSnapshot["state"] | null,
  ): boolean => {
    if (!initiatedFromThisTab || reloadedOnce || deps.getPlatform() === "windows") {
      return false;
    }
    if (updateState === "upgraded") {
      return true;
    }
    return versionReachedOrExceeded(currentVersion, targetVersion);
  };

  const scheduleNext = () => {
    if (destroyed || timerId !== null) return;
    timerId = scheduleFn(() => {
      timerId = null;
      void runPoll();
    }, pollIntervalMs);
  };

  const runPoll = async (): Promise<void> => {
    if (destroyed || inFlight) return;
    inFlight = true;
    const outcome = await deps.pollUpdateStatus();
    inFlight = false;
    if (destroyed) return;

    if (!outcome.ok) {
      const now = nowMs();
      if (firstDisconnectAtMs === null) {
        firstDisconnectAtMs = now;
      }
      if (now - firstDisconnectAtMs < disconnectGraceMs) {
        setHolding(true);
        scheduleNext();
      } else {
        firstDisconnectAtMs = null;
        setHolding(false);
        stopTimer();
        deps.onDisconnectTimeout();
      }
      return;
    }

    firstDisconnectAtMs = null;
    const status = outcome.data;
    if (status.latest_version && !targetVersion) {
      targetVersion = status.latest_version;
    }

    if (shouldAutoReloadOnUnix(status.current_version, status.state)) {
      triggerAutoReloadOnce();
      return;
    }

    if (status.state === "upgrading") {
      setHolding(true);
      deps.onUpdateStatus(status);
      scheduleNext();
      return;
    }

    setHolding(false);
    stopTimer();
    deps.onUpdateStatus(status);
  };

  const notifyUpgradeStarted = (status: UpdateStatusSnapshot) => {
    if (destroyed) return;
    if (status.latest_version) {
      targetVersion = status.latest_version;
    }
    if (status.state === "upgrading") {
      initiatedFromThisTab = true;
      firstDisconnectAtMs = null;
      setHolding(true);
      scheduleNext();
    } else if (shouldAutoReloadOnUnix(status.current_version, status.state)) {
      triggerAutoReloadOnce();
    }
  };

  const syncSettingsSnapshot = (snap: SettingsSnapshot) => {
    if (destroyed) return;
    const upd = snap.update ?? null;
    if (upd?.state === "upgrading") {
      initiatedFromThisTab = true;
      if (upd.latest_version) {
        targetVersion = upd.latest_version;
      }
      firstDisconnectAtMs = null;
      setHolding(true);
      scheduleNext();
      return;
    }

    const effectiveVersion = upd?.current_version || snap.binary_version;
    if (shouldAutoReloadOnUnix(effectiveVersion, upd?.state ?? null)) {
      triggerAutoReloadOnce();
    }
  };

  return {
    notifyUpgradeStarted,
    syncSettingsSnapshot,
    isPolling: () => timerId !== null || inFlight,
    isHoldingUpgrading: () => holdingUpgrading,
    didAutoReload: () => reloadedOnce,
    destroy: () => {
      destroyed = true;
      stopTimer();
      setHolding(false);
    },
  };
}

export function buildUpdateHeaderRenderState(
  state: AppState,
  checkingUpdate: boolean,
  holdingUpgrading: boolean,
  optimisticUpdate: UpdateStatusSnapshot | null,
): UpdateHeaderRenderState {
  return {
    connected: state.connected,
    standalone: state.standalone,
    snapshot: state.snapshot,
    checkingUpdate,
    holdingUpgrading,
    optimisticUpdate,
  };
}
