import {
  el,
  formatAccountPlanSummary,
  formatRamMb,
  quitReopenSentence,
} from "./dom.ts";
import { DEFAULT_CONFIG, type AppState } from "./types.ts";

export interface FooterController {
  footer: HTMLElement;
  popover: HTMLElement;
  update: (state: AppState) => void;
  closePopover: () => void;
  isPopoverOpen: () => boolean;
  destroy: () => void;
}

export function createFooterController(container: HTMLElement): FooterController {
  const footer = el("footer", "status-bar");

  // Left trigger: stable DOM button (never destroyed across background polls)
  const trigger = el("button", "status-trigger");
  trigger.type = "button";
  trigger.setAttribute("aria-expanded", "false");
  trigger.setAttribute("aria-controls", "status-popover");

  const statusDot = el("span", "sdot ok");
  const statusLabel = el("span", null, "Live");
  trigger.append(statusDot, statusLabel);

  // Right indicator: quiet XX MB RAM when reachable & available (hidden offline)
  const ramEl = el("span", "footer-ram mono num");
  ramEl.hidden = true;

  footer.append(trigger, ramEl);

  // Anchored upward health popover (strictly proxy health rows ONLY)
  const popover = el("div", "footer-popover pop-left");
  popover.id = "status-popover";
  popover.setAttribute("role", "region");
  popover.setAttribute("aria-label", "Proxy health");
  popover.hidden = true;

  const popHdr = el("div", "pop-hdr");
  const popTitle = el("span", null, "Local proxy live");
  const popTag = el("span", "tag tag-ok", "Healthy");
  popHdr.append(popTitle, popTag);

  const kvList = el("div", "kv-list");

  const createKvRow = (keyLabel: string, valueClass: string) => {
    const row = el("div", "kv-row");
    const k = el("span", "kv-k", keyLabel);
    const v = el("span", valueClass ? `kv-v ${valueClass}` : "kv-v", "\u2014");
    row.append(k, v);
    kvList.append(row);
    return { row, valueEl: v };
  };

  const endpointRow = createKvRow("Endpoint", "mono num");
  const planRow = createKvRow("Antigravity plan", "");
  const versionRow = createKvRow("Proxy / CLI", "mono num");
  const memoryRow = createKvRow("Memory", "mono num");
  const issueRow = createKvRow("Issue", "warn");
  issueRow.row.hidden = true;

  popover.append(popHdr, kvList);

  let open = false;
  let hoverTimer: number | null = null;
  let hoverDismissed = false;
  let openedByMouseDown = false;

  const cancelHoverTimer = () => {
    if (hoverTimer !== null) {
      window.clearTimeout(hoverTimer);
      hoverTimer = null;
    }
  };

  const setOpen = (next: boolean) => {
    open = next;
    popover.hidden = !open;
    trigger.setAttribute("aria-expanded", String(open));
  };

  const openPopover = () => {
    cancelHoverTimer();
    if (!open) setOpen(true);
  };

  const closePopover = () => {
    cancelHoverTimer();
    if (open) setOpen(false);
  };

  const scheduleClose = () => {
    cancelHoverTimer();
    hoverTimer = window.setTimeout(() => {
      hoverTimer = null;
      if (open) setOpen(false);
    }, 160);
  };

  const handleBlur = (e: FocusEvent) => {
    const next = e.relatedTarget;
    if (next instanceof Node) {
      if (next === trigger || popover.contains(next)) return;
    }
    cancelHoverTimer();
    if (open) setOpen(false);
  };

  trigger.addEventListener("mouseenter", () => {
    if (hoverDismissed) return;
    openPopover();
  });
  trigger.addEventListener("mouseleave", () => {
    if (hoverDismissed) hoverDismissed = false;
    scheduleClose();
  });
  trigger.addEventListener("mousedown", () => {
    openedByMouseDown = !open;
  });
  trigger.addEventListener("focus", () => {
    openPopover();
  });
  trigger.addEventListener("blur", (e) => {
    handleBlur(e);
  });
  trigger.addEventListener("click", (e) => {
    e.stopPropagation();
    cancelHoverTimer();
    if (openedByMouseDown) {
      openedByMouseDown = false;
      hoverDismissed = false;
      if (!open) setOpen(true);
      return;
    }
    if (open) {
      setOpen(false);
      hoverDismissed = true;
    } else {
      hoverDismissed = false;
      setOpen(true);
    }
  });

  popover.addEventListener("mouseenter", cancelHoverTimer);
  popover.addEventListener("mouseleave", scheduleClose);
  popover.addEventListener("focusout", (e) => handleBlur(e));

  const onDocumentClick = (e: MouseEvent) => {
    if (!open) return;
    const target = e.target;
    if (target instanceof Node && (trigger.contains(target) || popover.contains(target))) {
      return;
    }
    closePopover();
  };
  document.addEventListener("click", onDocumentClick);

  container.append(footer, popover);

  const update = (state: AppState) => {
    const snap = state.snapshot;
    const host = snap?.config.proxy.host ?? DEFAULT_CONFIG.proxy.host;
    const port = snap?.config.proxy.port ?? DEFAULT_CONFIG.proxy.port;
    const endpointText = `${host}:${port}`;

    const reachable = snap ? snap.proxy.reachable : false;
    const versionSkew = snap ? snap.version_skew : false;
    const platform = snap?.platform ?? "macos";
    const li = snap?.launcher_integration ?? null;
    const isUnixDisabled =
      platform !== "windows" &&
      li !== null &&
      !li.enabled &&
      li.state !== "manual_required" &&
      li.state !== "unsupported";

    // Left trigger pill
    if (!snap) {
      trigger.className = "status-trigger";
      statusDot.className = "sdot";
      statusLabel.textContent = state.standalone ? "Preview" : "Connecting\u2026";
    } else if (!reachable) {
      trigger.className = "status-trigger is-warn";
      statusDot.className = "sdot warn";
      statusLabel.textContent = "Proxy offline \u00b7 Retrying\u2026";
    } else if (isUnixDisabled) {
      trigger.className = "status-trigger is-warn";
      statusDot.className = "sdot warn";
      statusLabel.textContent = "Proxy disabled";
    } else if (versionSkew) {
      trigger.className = "status-trigger is-warn";
      statusDot.className = "sdot warn";
      statusLabel.textContent = "Update pending";
    } else {
      trigger.className = "status-trigger";
      statusDot.className = "sdot ok";
      statusLabel.textContent = "Live";
    }

    // Right side RAM indicator (visible ONLY when proxy reachable & rss known)
    const rssBytes = snap?.proxy_rss_bytes ?? snap?.stats?.proxy_rss_bytes ?? null;
    if (snap && reachable && rssBytes !== null && rssBytes > 0) {
      ramEl.textContent = formatRamMb(rssBytes);
      ramEl.hidden = false;
    } else {
      ramEl.textContent = "";
      ramEl.hidden = true;
    }

    // Popover header & strictly health-only rows
    if (!snap) {
      popTitle.textContent = state.standalone ? "Preview mode" : "Connecting to proxy";
      popTag.className = "tag";
      popTag.textContent = state.standalone ? "Preview" : "Pending";
    } else if (!reachable) {
      popTitle.textContent = "Proxy offline";
      popTag.className = "tag tag-warn";
      popTag.textContent = "Retrying";
    } else if (isUnixDisabled) {
      popTitle.textContent = "Local proxy disabled";
      popTag.className = "tag tag-warn";
      popTag.textContent = "Disabled";
    } else if (versionSkew) {
      popTitle.textContent = "Update pending";
      popTag.className = "tag tag-warn";
      popTag.textContent = "Version skew";
    } else {
      popTitle.textContent = "Local proxy live";
      popTag.className = "tag tag-ok";
      popTag.textContent = "Healthy";
    }

    endpointRow.valueEl.textContent = endpointText;
    planRow.valueEl.textContent = snap
      ? formatAccountPlanSummary(snap.account_plan)
      : "Auto";

    const binVer = snap?.binary_version ?? "0.0.3";
    if (!snap) {
      versionRow.valueEl.textContent = `\u2014 / v${binVer}`;
    } else if (reachable) {
      const pVer = snap.proxy.version ? `v${snap.proxy.version}` : "v?";
      versionRow.valueEl.textContent = `${pVer} / v${binVer}`;
    } else {
      versionRow.valueEl.textContent = `Offline / v${binVer}`;
    }

    memoryRow.valueEl.textContent =
      snap && reachable && rssBytes !== null && rssBytes > 0
        ? formatRamMb(rssBytes)
        : "\u2014";

    // Issue row (ONLY shown when unhealthy / offline / version-skewed)
    let issueText: string | null = null;
    if (snap && !reachable) {
      issueText =
        snap.proxy.error?.trim() ||
        `Connection refused on ${endpointText} \u00b7 Auto-retrying`;
    } else if (snap && versionSkew) {
      const pVer = snap.proxy.version ? `v${snap.proxy.version}` : "unknown";
      issueText = `Proxy ${pVer} differs from CLI v${binVer} \u00b7 ${quitReopenSentence(platform)}`;
    } else if (snap && snap.proxy.error?.trim()) {
      issueText = snap.proxy.error.trim();
    }

    if (issueText) {
      issueRow.valueEl.textContent = issueText;
      issueRow.row.hidden = false;
    } else {
      issueRow.valueEl.textContent = "";
      issueRow.row.hidden = true;
    }
  };

  const destroy = () => {
    cancelHoverTimer();
    document.removeEventListener("click", onDocumentClick);
    footer.remove();
    popover.remove();
  };

  return {
    footer,
    popover,
    update,
    closePopover,
    isPopoverOpen: () => open,
    destroy,
  };
}
