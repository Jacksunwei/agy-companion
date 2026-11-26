import {
  CURATED_MODEL_CATALOG,
  type ActiveTab,
  type CuratedModelInfo,
  type SettingsSnapshot,
} from "./types.ts";

export const HEALTHY_POLL_MS = 10_000;
export const UNHEALTHY_BACKOFF_MS: ReadonlyArray<number> = [3_000, 5_000, 10_000];

export function chooseInitialTab(snapshot: SettingsSnapshot): ActiveTab {
  const hybridActive = snapshot.launcher_integration?.hybrid_active === true;
  const apiKeyConfigured = snapshot.config.providers.gemini.api_key_configured;
  return hybridActive || apiKeyConfigured ? "monitor" : "settings";
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
): number {
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
  const unixDisabled =
    snapshot.platform !== "windows" &&
    li !== null &&
    !li.enabled &&
    li.state !== "unsupported";
  return (
    !snapshot.config.providers.gemini.api_key_configured ||
    unixDisabled ||
    li?.state === "manual_required" ||
    li?.hybrid_active !== true
  );
}
