import { BRAND_LOGO_CHILDREN, type SvgNodeSpec } from "./brand_logo.ts";
import type { AccountPlanStatus, CuratedModelInfo, HostPlatform } from "./types.ts";

const SVG_NS = "http://www.w3.org/2000/svg";

export type IconKind =
  | "warn"
  | "info"
  | "err"
  | "check"
  | "chev"
  | "close"
  | "pulse"
  | "bars"
  | "gear";

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string | null,
  text?: string | number | null,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

export function svgEl(tag: string, attrs?: Record<string, string | number>): SVGElement {
  const node = document.createElementNS(SVG_NS, tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      node.setAttribute(k, String(v));
    }
  }
  return node;
}

function buildSvgTree(spec: SvgNodeSpec): SVGElement {
  const [tag, attrs, children] = spec;
  const node = svgEl(tag, attrs);
  if (children) {
    for (const child of children) {
      node.append(buildSvgTree(child));
    }
  }
  return node;
}

export function createBrandMarkSvg(): SVGElement {
  const svg = svgEl("svg", {
    viewBox: "0 0 128 128",
    fill: "none",
    "aria-hidden": "true",
    focusable: "false",
  });
  for (const child of BRAND_LOGO_CHILDREN) {
    svg.append(buildSvgTree(child));
  }
  return svg;
}

export function createIcon(kind: IconKind): SVGElement {
  const svg = svgEl("svg", {
    viewBox: "0 0 16 16",
    "aria-hidden": "true",
    focusable: "false",
  });
  if (kind === "warn") {
    svg.append(
      svgEl("path", { d: "M8 2.2 L14.2 13.2 H1.8 Z" }),
      svgEl("path", { d: "M8 6.2 V9.4" }),
      svgEl("circle", { cx: 8, cy: 11.5, r: 0.6, fill: "currentColor" }),
    );
  } else if (kind === "info") {
    svg.append(
      svgEl("circle", { cx: 8, cy: 8, r: 5.8 }),
      svgEl("path", { d: "M8 7.2 V10.8" }),
      svgEl("circle", { cx: 8, cy: 5.1, r: 0.6, fill: "currentColor" }),
    );
  } else if (kind === "err") {
    svg.append(
      svgEl("circle", { cx: 8, cy: 8, r: 5.8 }),
      svgEl("path", { d: "M5.8 5.8 L10.2 10.2 M10.2 5.8 L5.8 10.2" }),
    );
  } else if (kind === "check") {
    svg.append(svgEl("path", { d: "M3 8.5 L6.4 11.8 L13 4.5" }));
  } else if (kind === "chev") {
    svg.append(svgEl("path", { d: "M6 3.5 L10.5 8 L6 12.5" }));
  } else if (kind === "close") {
    svg.append(svgEl("path", { d: "M4.5 4.5 L11.5 11.5 M11.5 4.5 L4.5 11.5" }));
  } else if (kind === "pulse") {
    svg.append(svgEl("path", { d: "M1.5 8.5 H4.4 L6.2 3.2 L9.3 12.8 L11.4 8.5 H14.5" }));
  } else if (kind === "bars") {
    svg.append(
      svgEl("path", { d: "M3.5 13 V8.5" }),
      svgEl("path", { d: "M8 13 V3.5" }),
      svgEl("path", { d: "M12.5 13 V6.2" }),
    );
  } else if (kind === "gear") {
    svg.append(
      svgEl("circle", { cx: 8, cy: 8, r: 2.4 }),
      svgEl("path", {
        d: "M8 2.2 V3.7 M8 12.3 V13.8 M2.2 8 H3.7 M12.3 8 H13.8 M3.9 3.9 L5 5 M11 11 L12.1 12.1 M12.1 3.9 L11 5 M5 11 L3.9 12.1",
      }),
    );
  }
  return svg;
}

export function formatCompactTokens(raw: number): string {
  const n = Number.isFinite(raw) && raw > 0 ? Math.round(raw) : 0;
  if (n >= 1_000_000_000) {
    return `${(n / 1_000_000_000).toFixed(1).replace(/\.0$/, "")}B`;
  }
  if (n >= 1_000_000) {
    return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  }
  if (n >= 10_000) {
    return `${Math.round(n / 1000)}K`;
  }
  if (n >= 1000) {
    return `${(n / 1000).toFixed(1).replace(/\.0$/, "")}K`;
  }
  return String(n);
}

export function formatRamMb(bytes: number): string {
  const mb = Math.max(1, Math.round(bytes / (1024 * 1024)));
  return `${mb} MB RAM`;
}

export function formatLatencyMs(rawMs: number): string {
  const ms = Number.isFinite(rawMs) && rawMs >= 0 ? rawMs : 0;
  if (ms >= 1000) {
    return `${(ms / 1000).toFixed(1)}s`;
  }
  return `${Math.round(ms)}ms`;
}

export function formatUsd(usd: number): string {
  if (!Number.isFinite(usd) || usd <= 0) return "\u2248 $0.00";
  if (usd < 0.01) return "\u2248 <$0.01";
  return `\u2248 $${usd.toFixed(2)}`;
}

export function formatUsdCompact(usd: number): string {
  if (!Number.isFinite(usd) || usd <= 0) return "\u2248 $0";
  if (usd >= 10) return `\u2248 $${Math.round(usd)}`;
  if (usd < 0.01) return "\u2248 <$0.01";
  return `\u2248 $${usd.toFixed(2)}`;
}

const MONTH_NAMES = [
  "Jan",
  "Feb",
  "Mar",
  "Apr",
  "May",
  "Jun",
  "Jul",
  "Aug",
  "Sep",
  "Oct",
  "Nov",
  "Dec",
] as const;

export function formatShortDateUtc(isoOrYmd: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})/.exec(isoOrYmd.trim());
  if (!match) return isoOrYmd;
  const monthIdx = Number(match[2]) - 1;
  const day = Number(match[3]);
  const monthName = MONTH_NAMES[monthIdx];
  if (!monthName || !Number.isFinite(day)) return isoOrYmd;
  return `${monthName} ${day}`;
}

export function formatTimeHHMM(timestampMs: number): string {
  if (!Number.isFinite(timestampMs) || timestampMs <= 0) return "";
  const d = new Date(timestampMs);
  if (Number.isNaN(d.getTime())) return "";
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

export function quitReopenSentence(platform: HostPlatform): string {
  if (platform === "macos") {
    return "quit (\u2318Q) and reopen Antigravity";
  }
  if (platform === "windows") {
    return 'close Antigravity and launch it from the "Antigravity (Companion)" Start Menu shortcut';
  }
  return "quit and reopen Antigravity";
}

export function quitReopenCapitalized(platform: HostPlatform): string {
  if (platform === "macos") {
    return "Quit (\u2318Q) and reopen Antigravity";
  }
  if (platform === "windows") {
    return 'Close Antigravity and launch it from the "Antigravity (Companion)" Start Menu shortcut';
  }
  return "Quit and reopen Antigravity";
}

export function formatCatalogRefreshCopy(
  platform: HostPlatform,
  waitHint: string,
): string {
  if (platform === "macos") {
    return `Quit (\u2318Q) & reopen Antigravity, or wait ${waitHint} for the model list to update.`;
  }
  return `Quit & reopen Antigravity, or wait ${waitHint} for the model list to update.`;
}

export function formatAccountPlanSummary(plan: AccountPlanStatus): string {
  if (plan.mode === "auto") {
    if (plan.resolved === "organization") return "Auto (Organization)";
    if (plan.resolved === "personal") return "Auto (Personal)";
    return "Auto";
  }
  if (plan.mode === "personal") return "Personal";
  if (plan.mode === "organization") return "Organization";
  return "Custom";
}

export function formatResolvedPlanLabel(
  resolved: AccountPlanStatus["resolved"],
): string | null {
  if (resolved === "organization") return "Organization";
  if (resolved === "personal") return "Personal";
  return null;
}

export async function copyTextWithFallback(text: string): Promise<boolean> {
  if (!text) return false;
  try {
    if (
      typeof navigator !== "undefined" &&
      navigator.clipboard &&
      typeof navigator.clipboard.writeText === "function"
    ) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Fall through to offscreen textarea copy
  }

  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "-9999px";
    ta.style.left = "-9999px";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.focus();
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

export interface TooltipController {
  show: (anchorBtn: HTMLElement, model: CuratedModelInfo) => void;
  scheduleHide: () => void;
  hideNow: () => void;
  destroy: () => void;
}

export function createTooltipController(container: HTMLElement): TooltipController {
  const tip = el("div", "model-tooltip");
  tip.id = "model-tooltip";
  tip.setAttribute("role", "tooltip");
  container.append(tip);

  let hideTimer: number | null = null;

  const cancelHide = () => {
    if (hideTimer !== null) {
      window.clearTimeout(hideTimer);
      hideTimer = null;
    }
  };

  const hideNow = () => {
    cancelHide();
    tip.classList.remove("visible");
  };

  const scheduleHide = () => {
    cancelHide();
    hideTimer = window.setTimeout(() => {
      hideTimer = null;
      tip.classList.remove("visible");
    }, 120);
  };

  tip.addEventListener("mouseenter", cancelHide);
  tip.addEventListener("mouseleave", scheduleHide);

  const show = (anchorBtn: HTMLElement, model: CuratedModelInfo) => {
    cancelHide();

    const titleRow = el("div", "tip-title");
    titleRow.append(el("span", null, model.label), el("span", "tip-id", model.id));
    const desc = el("p", "tip-desc", model.desc);
    const meta = el("div", "tip-meta", model.meta);
    tip.replaceChildren(titleRow, desc, meta);

    tip.style.left = "0px";
    tip.style.top = "0px";
    tip.classList.add("visible");

    const frameRect = container.getBoundingClientRect();
    const chipRect = anchorBtn.getBoundingClientRect();
    const tipRect = tip.getBoundingClientRect();

    const pad = 10;
    const chipCenterX = chipRect.left - frameRect.left + chipRect.width / 2;
    let left = chipCenterX - tipRect.width / 2;
    const maxLeft = Math.max(pad, frameRect.width - tipRect.width - pad);
    left = Math.max(pad, Math.min(left, maxLeft));

    const headerClearance = 48;
    const footerClearance = 42;
    const chipTopRel = chipRect.top - frameRect.top;
    const chipBottomRel = chipRect.bottom - frameRect.top;
    let top = chipTopRel - tipRect.height - 8;
    if (top < headerClearance) {
      top = chipBottomRel + 8;
    }
    const maxTop = frameRect.height - footerClearance - tipRect.height;
    if (top > maxTop) {
      top = Math.max(headerClearance, maxTop);
    }

    tip.style.left = `${Math.round(left)}px`;
    tip.style.top = `${Math.round(top)}px`;
  };

  const destroy = () => {
    cancelHide();
    tip.remove();
  };

  return { show, scheduleHide, hideNow, destroy };
}
