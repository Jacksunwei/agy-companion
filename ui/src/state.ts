import {
  CURATED_MODEL_CATALOG,
  type ActiveTab,
  type CatalogRefreshReminder,
  type CuratedModelInfo,
  type SettingsSnapshot,
} from "./types.ts";

export const HEALTHY_POLL_MS = 10_000;
export const CODEX_PENDING_POLL_MS = 2_000;
export const UNHEALTHY_BACKOFF_MS: ReadonlyArray<number> = [3_000, 5_000, 10_000];
export const CATALOG_POLL_INTERVAL_MS = 300_000;
export const FEEDBACK_AUTO_DISMISS_MS = 4_000;
const CATALOG_OVERDUE_GRACE_MS = 60_000;

export interface AutoDismissTimer {
  schedule: (delayMs: number, onDismiss: () => void) => void;
  cancel: () => void;
}

export function createAutoDismissTimer(
  scheduleFn: (cb: () => void, ms: number) => number = (cb, ms) => window.setTimeout(cb, ms),
  clearFn: (id: number) => void = (id) => window.clearTimeout(id),
): AutoDismissTimer {
  let timerId: number | null = null;
  let epoch = 0;

  const cancel = () => {
    epoch += 1;
    if (timerId !== null) {
      clearFn(timerId);
      timerId = null;
    }
  };

  const schedule = (delayMs: number, onDismiss: () => void) => {
    cancel();
    if (!Number.isFinite(delayMs) || delayMs <= 0) return;
    const myEpoch = epoch;
    timerId = scheduleFn(() => {
      if (epoch !== myEpoch) return;
      timerId = null;
      onDismiss();
    }, delayMs);
  };

  return { schedule, cancel };
}

export function hasConfiguredByomProvider(snapshot: SettingsSnapshot): boolean {
  return (
    snapshot.config.providers.gemini.api_key_configured ||
    snapshot.config.providers.codex.authenticated
  );
}

export function chooseInitialTab(snapshot: SettingsSnapshot): ActiveTab {
  const lsTelemetryActive = snapshot.ls_telemetry_active === true;
  const hybridActive = snapshot.launcher_integration?.hybrid_active === true;
  const byomConfigured = hasConfiguredByomProvider(snapshot);
  return lsTelemetryActive || hybridActive || byomConfigured ? "monitor" : "settings";
}

export function nextTabForKey(
  tabIds: ReadonlyArray<ActiveTab>,
  current: ActiveTab,
  key: string,
): ActiveTab | null {
  if (tabIds.length === 0) return null;
  const idx = Math.max(0, tabIds.indexOf(current));
  if (key === "ArrowRight") {
    return tabIds[(idx + 1) % tabIds.length];
  }
  if (key === "ArrowLeft") {
    return tabIds[(idx - 1 + tabIds.length) % tabIds.length];
  }
  if (key === "Home") {
    return tabIds[0];
  }
  if (key === "End") {
    return tabIds[tabIds.length - 1];
  }
  return null;
}

export function shouldApplyRefresh(
  startedEpoch: number,
  currentEpoch: number,
  writeInFlight: boolean,
): boolean {
  return startedEpoch === currentEpoch && !writeInFlight;
}

export function nextPollDelayMs(
  reachable: boolean,
  unhealthyStreak: number,
  codexLoginPending = false,
): number {
  if (codexLoginPending) {
    return CODEX_PENDING_POLL_MS;
  }
  if (reachable && unhealthyStreak === 0) {
    return HEALTHY_POLL_MS;
  }
  const idx = Math.min(
    Math.max(0, unhealthyStreak - 1),
    UNHEALTHY_BACKOFF_MS.length - 1,
  );
  return UNHEALTHY_BACKOFF_MS[idx];
}

export function toggleEnabledModel(
  current: ReadonlyArray<string>,
  modelId: string,
  catalog: ReadonlyArray<Pick<CuratedModelInfo, "id">> = CURATED_MODEL_CATALOG,
): string[] {
  const nextSet = new Set(current);
  if (nextSet.has(modelId)) {
    nextSet.delete(modelId);
  } else {
    nextSet.add(modelId);
  }
  const ordered: string[] = [];
  for (const m of catalog) {
    if (nextSet.has(m.id)) {
      ordered.push(m.id);
      nextSet.delete(m.id);
    }
  }
  for (const extra of nextSet) {
    ordered.push(extra);
  }
  return ordered;
}

export function needsSettingsAttention(snapshot: SettingsSnapshot | null): boolean {
  if (snapshot === null) return false;
  const li = snapshot.launcher_integration;
  const byomConfigured = hasConfiguredByomProvider(snapshot);
  if (snapshot.ls_telemetry_active && !byomConfigured) {
    return li?.state === "manual_required";
  }
  const unixDisabled =
    snapshot.platform !== "windows" &&
    li !== null &&
    !li.enabled &&
    li.state !== "unsupported";
  return (
    !byomConfigured ||
    unixDisabled ||
    li?.state === "manual_required" ||
    li?.hybrid_active !== true
  );
}

export function createCatalogRefreshReminder(
  prevSnap: SettingsSnapshot | null,
  nextSnap: SettingsSnapshot,
  nowMs: number,
): CatalogRefreshReminder | null {
  if (!nextSnap.proxy.reachable || nextSnap.launcher_integration?.hybrid_active !== true) {
    return null;
  }
  const baselineCatalogRefreshAtMs =
    nextSnap.last_catalog_refresh_at_ms ?? prevSnap?.last_catalog_refresh_at_ms ?? null;
  return {
    baselineCatalogRefreshAtMs,
    changedAtMs: nowMs,
  };
}

export function reconcileCatalogRefreshReminder(
  reminder: CatalogRefreshReminder | null,
  snap: SettingsSnapshot,
): CatalogRefreshReminder | null {
  if (reminder === null) return null;
  if (!snap.proxy.reachable || snap.launcher_integration?.hybrid_active !== true) {
    return null;
  }
  const current = snap.last_catalog_refresh_at_ms;
  if (reminder.baselineCatalogRefreshAtMs !== null) {
    if (current !== null && current > reminder.baselineCatalogRefreshAtMs) {
      return null;
    }
  } else if (current !== null) {
    return null;
  }
  return reminder;
}

export function formatCatalogRefreshWaitHint(
  lastRefreshAtMs: number | null,
  nowMs: number,
): string {
  if (
    lastRefreshAtMs === null ||
    !Number.isFinite(lastRefreshAtMs) ||
    lastRefreshAtMs <= 0 ||
    !Number.isFinite(nowMs)
  ) {
    return "up to ~5m";
  }
  const elapsed = nowMs - lastRefreshAtMs;
  if (elapsed < 0) {
    return "up to ~5m";
  }
  if (elapsed < CATALOG_POLL_INTERVAL_MS) {
    const remaining = CATALOG_POLL_INTERVAL_MS - elapsed;
    if (remaining <= 60_000) {
      return "<1m";
    }
    const mins = Math.min(5, Math.max(1, Math.round(remaining / 60_000)));
    return `~${mins}m`;
  }
  if (elapsed <= CATALOG_POLL_INTERVAL_MS + CATALOG_OVERDUE_GRACE_MS) {
    return "<1m";
  }
  return "up to ~5m";
}
