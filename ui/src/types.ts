export type HostPlatform = "macos" | "linux" | "windows";
export type ActiveTab = "monitor" | "usage" | "settings";
export type UsageRange = "1d" | "7d" | "30d";
export type UsageUnit = "usd" | "tok";
export type UsageGroup = "model" | "provider";
export type StatusTone = "ok" | "warn" | "err" | "info";

export type AccountPlanMode = "auto" | "personal" | "organization" | "custom";
export type ResolvedAccountPlan = "personal" | "organization";
export type AccountPlanChoice = "auto" | "personal" | "organization";

export type IntegrationState =
  | "active"
  | "enabled"
  | "manual_required"
  | "disabled"
  | "unsupported";

export interface AccountPlanStatus {
  mode: AccountPlanMode;
  resolved: ResolvedAccountPlan | null;
}

export interface ProxyConfig {
  host: string;
  port: number;
}

export interface RedactedGeminiConfig {
  base_url: string;
  default_model: string;
  enabled_models: string[];
  timeout_secs: number;
  api_key_configured: boolean;
  api_key_hint: string | null;
}

export interface RedactedCodexConfig {
  base_url: string;
  default_model: string;
  enabled_models: string[];
  timeout_secs: number;
  authenticated: boolean;
  account_hint: string | null;
  plan_type: string | null;
  expires_at_ms: number | null;
}

export interface RedactedConfig {
  schema_version: number;
  proxy: ProxyConfig;
  providers: {
    gemini: RedactedGeminiConfig;
    codex: RedactedCodexConfig;
  };
  launcher_auto_start: boolean | null;
}

export interface ProxyHealthSummary {
  reachable: boolean;
  status: string | null;
  version: string | null;
  protocol_version: number | null;
  error: string | null;
}

export interface UsageStatsSnapshot {
  requests: number;
  prompt_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_hit_ratio: number;
}

export interface RecentCallSnapshot {
  timestamp_ms: number;
  provider: string;
  source: string;
  model: string;
  input_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  latency_ms: number;
  cold_start: boolean;
  cache_break: boolean;
}

export interface ModelSessionStats {
  provider: string;
  source: string;
  model: string;
  requests: number;
  input_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  total_latency_ms: number;
}

export interface StatsSnapshot {
  usage: UsageStatsSnapshot;
  proxy_rss_bytes: number | null;
  max_context_tokens: number | null;
  recent_calls: RecentCallSnapshot[];
  by_model: ModelSessionStats[];
}

export interface LauncherIntegrationStatus {
  state: IntegrationState;
  enabled: boolean;
  hybrid_active: boolean;
  shell: string | null;
  rc_path: string | null;
  env_script_path: string | null;
  binary_path: string | null;
  symlink_target: string | null;
  manual_snippet: string | null;
  message: string | null;
}

export interface ModelPreset {
  id: string;
  display_name: string;
  short_label: string;
}

export interface SettingsSnapshot {
  config: RedactedConfig;
  binary_version: string;
  proxy: ProxyHealthSummary;
  version_skew: boolean;
  stats: StatsSnapshot | null;
  launcher_integration: LauncherIntegrationStatus | null;
  supported_models: ModelPreset[];
  supported_codex_models: ModelPreset[];
  codex_login_pending: boolean;
  platform: HostPlatform;
  account_plan: AccountPlanStatus;
  proxy_rss_bytes: number | null;
  ls_telemetry_active: boolean;
  last_catalog_refresh_at_ms: number | null;
}

export interface UsageBucket {
  label: string;
  requests: number;
  input_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
}

export interface UsageBreakdownRow {
  key: string;
  provider: string;
  source: string | null;
  model_count: number | null;
  requests: number;
  input_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  avg_latency_ms: number;
}

export interface UsageSummary {
  range: UsageRange;
  since_utc: string;
  totals: UsageBucket;
  buckets: UsageBucket[];
  by_model: UsageBreakdownRow[];
  by_provider: UsageBreakdownRow[];
}

export type ApiKeyPatch = "clear" | { set: string };

export type TestConnectionProvider = "gemini" | "codex";

export interface CodexSettingsPatch {
  base_url?: string;
  default_model?: string;
  enabled_models?: string[];
}

export interface SaveSettingsArgs {
  providers?: {
    gemini?: {
      api_key?: ApiKeyPatch;
      enabled_models?: string[];
    };
    codex?: CodexSettingsPatch;
  };
  account_plan?: AccountPlanChoice;
}

export interface TestConnectionPayload {
  ok: boolean;
  message: string;
}

export interface StartCodexLoginPayload {
  authorize_url: string;
  callback_listening: boolean;
  manual_fallback: boolean;
}

export interface ToolCallResultLike {
  content?: Array<{ type?: string; text?: string }>;
  structuredContent?: unknown;
  isError?: boolean;
}

export interface InlineFeedback {
  tone: StatusTone;
  text: string;
}

export interface CatalogRefreshReminder {
  baselineCatalogRefreshAtMs: number | null;
  changedAtMs: number;
}

export interface CuratedModelInfo {
  id: string;
  label: string;
  displayName: string;
  defaultOn: boolean;
  desc: string;
  meta: string;
}

export interface ProviderDescriptor {
  id: "gemini";
  title: string;
  keyPortalUrl: string;
  keyPortalDisplay: string;
  models: ReadonlyArray<CuratedModelInfo>;
}

export interface AppState {
  connected: boolean;
  standalone: boolean;
  hydratedFromInitialResult: boolean;
  initialTabApplied: boolean;
  tabPinnedByUser: boolean;
  activeTab: ActiveTab;
  snapshot: SettingsSnapshot | null;
  snapshotEpoch: number;
  loadError: string | null;

  refreshing: boolean;
  saving: boolean;
  testing: boolean;
  clearing: boolean;
  updatingIntegration: boolean;
  unhealthyStreak: number;

  apiKeyDraft: string;
  geminiKeyEditOpen: boolean;
  clearConfirmOpen: boolean;
  disableProxyConfirmOpen: boolean;
  advOpen: boolean;
  keyFeedback: InlineFeedback | null;
  modelFeedback: InlineFeedback | null;
  advFeedback: InlineFeedback | null;
  catalogRefreshReminder: CatalogRefreshReminder | null;

  codexBusy: boolean;
  codexTesting: boolean;
  codexAuthorizeUrl: string | null;
  codexManualFallbackOpen: boolean;
  codexManualInputDraft: string;
  codexSignOutConfirmOpen: boolean;
  codexFeedback: InlineFeedback | null;
  codexModelFeedback: InlineFeedback | null;

  usageRange: UsageRange;
  usageUnit: UsageUnit;
  usageGroup: UsageGroup;
  usageSummary: UsageSummary | null;
  usageLoading: boolean;
  usageError: string | null;
  usageEpoch: number;

  toastMessage: string | null;
}

export const PLACEHOLDER_API_KEY = "agy-companion-local";

export const DEFAULT_ENABLED_MODELS: ReadonlyArray<string> = [
  "gemini-4-carbon",
  "gemini-4-neon",
  "gemini-4-hydrogen",
];

export const CURATED_MODEL_CATALOG: ReadonlyArray<CuratedModelInfo> = [
  {
    id: "gemini-4-carbon",
    label: "4 Carbon",
    displayName: "Gemini 4 Carbon",
    defaultOn: true,
    desc: "Best for deep verification, hard debugging, security audits, and long-horizon tasks.",
    meta: "Strongest reasoning \u00b7 Deliberate \u00b7 Paid API tier",
  },
  {
    id: "gemini-4-neon",
    label: "4 Neon",
    displayName: "Gemini 4 Neon",
    defaultOn: true,
    desc: "Best for complex multi-file coding, refactoring, and analysis with faster turnaround than Carbon.",
    meta: "Frontier quality \u00b7 Balanced speed \u00b7 Paid API tier",
  },
  {
    id: "gemini-4-hydrogen",
    label: "4 Hydrogen",
    displayName: "Gemini 4 Hydrogen",
    defaultOn: true,
    desc: "Best for fast everyday coding, routine edits, and parallel agent workflows at lower cost.",
    meta: "Fast & efficient \u00b7 Free & Paid API tiers",
  },
  {
    id: "gemini-4-argon",
    label: "4 Argon",
    displayName: "Gemini 4 Argon",
    defaultOn: false,
    desc: "Best for efficient Gemini 4 coding, math, and science with lower token spend per turn than Carbon.",
    meta: "Efficient frontier \u00b7 Paid API tier",
  },
  {
    id: "gemini-3.8-flash",
    label: "3.8 Flash",
    displayName: "Gemini 3.8 Flash",
    defaultOn: false,
    desc: "Best for cost-efficient previous-gen Flash coding, multi-file edits, and steady tool execution.",
    meta: "Previous-gen Flash \u00b7 1M in / 64K out",
  },
];

export const DEFAULT_MODEL_PRESETS: ReadonlyArray<ModelPreset> =
  CURATED_MODEL_CATALOG.map((m) => ({
    id: m.id,
    display_name: m.displayName,
    short_label: m.label,
  }));

export const LIVE_PROVIDERS: ReadonlyArray<ProviderDescriptor> = [
  {
    id: "gemini",
    title: "Google AI Studio",
    keyPortalUrl: "https://aistudio.google.com/api-keys",
    keyPortalDisplay: "aistudio.google.com/api-keys",
    models: CURATED_MODEL_CATALOG,
  },
];

export const DEFAULT_ENABLED_CODEX_MODELS: ReadonlyArray<string> = [
  "gpt-6.2-astra",
  "gpt-6.2-sol",
];

export const CURATED_CODEX_MODEL_CATALOG: ReadonlyArray<CuratedModelInfo> = [
  {
    id: "gpt-6.2-astra",
    label: "6.2 Astra",
    displayName: "GPT-6.2 Astra",
    defaultOn: true,
    desc: "Best for complex coding, hard debugging, architecture, and deep professional work.",
    meta: "1.05M context \u00b7 Low / Medium / High / XHigh \u00b7 ChatGPT subscription",
  },
  {
    id: "gpt-6.2-sol",
    label: "6.2 Sol",
    displayName: "GPT-6.2 Sol",
    defaultOn: true,
    desc: "Best as the fast, go-to model for everyday coding and large-context repositories.",
    meta: "1.05M context \u00b7 Low / Medium / High / XHigh \u00b7 ChatGPT subscription",
  },
  {
    id: "gpt-6.1-terra",
    label: "6.1 Terra",
    displayName: "GPT-6.1 Terra",
    defaultOn: false,
    desc: "Best for visual understanding, screenshots, and efficient computer-use workflows.",
    meta: "1.05M context \u00b7 Low / Medium / High \u00b7 ChatGPT subscription",
  },
  {
    id: "gpt-6-luna",
    label: "6 Luna",
    displayName: "GPT-6 Luna",
    defaultOn: false,
    desc: "Best for fast, lightweight edits and preserving ChatGPT subscription quota.",
    meta: "1.05M context \u00b7 Low / Medium / High \u00b7 ChatGPT subscription",
  },
];

export const DEFAULT_CODEX_MODEL_PRESETS: ReadonlyArray<ModelPreset> =
  CURATED_CODEX_MODEL_CATALOG.map((m) => ({
    id: m.id,
    display_name: m.displayName,
    short_label: m.label,
  }));

export const DEFAULT_CODEX_CONFIG: RedactedCodexConfig = {
  base_url: "https://chatgpt.com/backend-api/codex",
  default_model: "gpt-6.2-sol",
  enabled_models: [...DEFAULT_ENABLED_CODEX_MODELS],
  timeout_secs: 900,
  authenticated: false,
  account_hint: null,
  plan_type: null,
  expires_at_ms: null,
};

export const DEFAULT_CONFIG: RedactedConfig = {
  schema_version: 1,
  proxy: {
    host: "127.0.0.1",
    port: 24141,
  },
  providers: {
    gemini: {
      base_url: "https://generativelanguage.googleapis.com",
      default_model: "gemini-4-hydrogen",
      enabled_models: [...DEFAULT_ENABLED_MODELS],
      timeout_secs: 900,
      api_key_configured: false,
      api_key_hint: null,
    },
    codex: {
      ...DEFAULT_CODEX_CONFIG,
      enabled_models: [...DEFAULT_ENABLED_CODEX_MODELS],
    },
  },
  launcher_auto_start: null,
};

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function asString(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function asNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function asBoolean(v: unknown): boolean | null {
  return typeof v === "boolean" ? v : null;
}

function asNullableString(v: unknown): string | null | undefined {
  if (v === null) return null;
  return typeof v === "string" ? v : undefined;
}

function asOptionalNullableString(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function asNullableNumber(v: unknown): number | null | undefined {
  if (v === null) return null;
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
}

function asOptionalNullableNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function asOptionalNullableBoolean(v: unknown): boolean | null {
  return typeof v === "boolean" ? v : null;
}

export function normalizeHostPlatform(raw: unknown): HostPlatform {
  if (raw === "macos" || raw === "linux" || raw === "windows") {
    return raw;
  }
  const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
  if (/Windows/i.test(ua)) return "windows";
  if (/Mac|Darwin/i.test(ua)) return "macos";
  return "linux";
}

function asIntegrationState(raw: unknown): IntegrationState | null {
  if (
    raw === "active" ||
    raw === "enabled" ||
    raw === "manual_required" ||
    raw === "disabled" ||
    raw === "unsupported"
  ) {
    return raw;
  }
  return null;
}

function parseAccountPlanStatus(raw: unknown): AccountPlanStatus {
  if (!isRecord(raw)) {
    return { mode: "auto", resolved: null };
  }
  const modeRaw = raw.mode;
  const mode: AccountPlanMode =
    modeRaw === "auto" ||
    modeRaw === "personal" ||
    modeRaw === "organization" ||
    modeRaw === "custom"
      ? modeRaw
      : "auto";
  const resRaw = raw.resolved;
  const resolved: ResolvedAccountPlan | null =
    resRaw === "personal" || resRaw === "organization" ? resRaw : null;
  return { mode, resolved };
}

function parseRedactedCodexConfig(raw: unknown): RedactedCodexConfig | null {
  if (raw === undefined) {
    return {
      ...DEFAULT_CODEX_CONFIG,
      enabled_models: [...DEFAULT_ENABLED_CODEX_MODELS],
    };
  }
  if (!isRecord(raw)) return null;

  const baseUrl = asString(raw.base_url);
  const defaultModel = asString(raw.default_model);
  const timeoutSecs = asNumber(raw.timeout_secs);
  const authenticated = asBoolean(raw.authenticated);
  const accountHint = asNullableString(raw.account_hint);
  const planType = asNullableString(raw.plan_type);
  const expiresAtMs = asNullableNumber(raw.expires_at_ms);

  if (
    baseUrl === null ||
    defaultModel === null ||
    timeoutSecs === null ||
    authenticated === null ||
    accountHint === undefined ||
    planType === undefined ||
    expiresAtMs === undefined
  ) {
    return null;
  }

  let enabledModels: string[];
  if (Array.isArray(raw.enabled_models)) {
    enabledModels = raw.enabled_models
      .filter((item): item is string => typeof item === "string")
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
  } else if (raw.enabled_models === undefined) {
    enabledModels = [...DEFAULT_ENABLED_CODEX_MODELS];
  } else {
    return null;
  }

  return {
    base_url: baseUrl,
    default_model: defaultModel,
    enabled_models: enabledModels,
    timeout_secs: timeoutSecs,
    authenticated,
    account_hint: accountHint,
    plan_type: planType,
    expires_at_ms: expiresAtMs,
  };
}

function parseRedactedConfig(raw: unknown): RedactedConfig | null {
  if (!isRecord(raw) || !isRecord(raw.proxy) || !isRecord(raw.providers)) {
    return null;
  }
  const geminiRaw = raw.providers.gemini;
  if (!isRecord(geminiRaw)) return null;

  const codex = parseRedactedCodexConfig(raw.providers.codex);
  if (codex === null) return null;

  const schemaVersion = asNumber(raw.schema_version) ?? 1;
  const host = asString(raw.proxy.host);
  const port = asNumber(raw.proxy.port);
  const baseUrl = asString(geminiRaw.base_url);
  const defaultModel = asString(geminiRaw.default_model);
  const timeoutSecs = asNumber(geminiRaw.timeout_secs);
  const apiKeyConfigured = asBoolean(geminiRaw.api_key_configured);
  const apiKeyHint = asNullableString(geminiRaw.api_key_hint);

  if (
    host === null ||
    port === null ||
    baseUrl === null ||
    defaultModel === null ||
    timeoutSecs === null ||
    apiKeyConfigured === null ||
    apiKeyHint === undefined
  ) {
    return null;
  }

  let enabledModels: string[];
  if (Array.isArray(geminiRaw.enabled_models)) {
    enabledModels = geminiRaw.enabled_models
      .filter((item): item is string => typeof item === "string")
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
  } else {
    enabledModels = [...DEFAULT_ENABLED_MODELS];
  }

  return {
    schema_version: schemaVersion,
    proxy: { host, port },
    providers: {
      gemini: {
        base_url: baseUrl,
        default_model: defaultModel,
        enabled_models: enabledModels,
        timeout_secs: timeoutSecs,
        api_key_configured: apiKeyConfigured,
        api_key_hint: apiKeyHint,
      },
      codex,
    },
    launcher_auto_start: asOptionalNullableBoolean(raw.launcher_auto_start),
  };
}

function parseProxyHealth(raw: unknown): ProxyHealthSummary | null {
  if (!isRecord(raw)) return null;
  const reachable = asBoolean(raw.reachable);
  const status = asNullableString(raw.status);
  const version = asNullableString(raw.version);
  const protocolVersion = asNullableNumber(raw.protocol_version);
  const error = asNullableString(raw.error);

  if (
    reachable === null ||
    status === undefined ||
    version === undefined ||
    protocolVersion === undefined ||
    error === undefined
  ) {
    return null;
  }

  return {
    reachable,
    status,
    version,
    protocol_version: protocolVersion,
    error,
  };
}

function parseRecentCalls(raw: unknown): RecentCallSnapshot[] {
  if (!Array.isArray(raw)) return [];
  const calls: RecentCallSnapshot[] = [];
  for (const item of raw) {
    if (!isRecord(item)) continue;
    const timestampMs = asNumber(item.timestamp_ms);
    const provider = asString(item.provider);
    const source = asString(item.source);
    const model = asString(item.model);
    const inputTokens = asNumber(item.input_tokens);
    const cachedTokens = asNumber(item.cached_tokens);
    const outputTokens = asNumber(item.output_tokens);
    const reasoningTokens = asNumber(item.reasoning_tokens) ?? 0;
    const latencyMs = asNumber(item.latency_ms) ?? 0;
    const coldStart = asBoolean(item.cold_start) ?? false;
    const cacheBreak = asBoolean(item.cache_break) ?? false;

    if (
      timestampMs === null ||
      provider === null ||
      source === null ||
      model === null ||
      inputTokens === null ||
      cachedTokens === null ||
      outputTokens === null
    ) {
      continue;
    }
    calls.push({
      timestamp_ms: timestampMs,
      provider,
      source,
      model,
      input_tokens: inputTokens,
      cached_tokens: cachedTokens,
      output_tokens: outputTokens,
      reasoning_tokens: reasoningTokens,
      latency_ms: latencyMs,
      cold_start: coldStart,
      cache_break: cacheBreak,
    });
  }
  return calls;
}

function parseByModel(raw: unknown): ModelSessionStats[] {
  if (!Array.isArray(raw)) return [];
  const rows: ModelSessionStats[] = [];
  for (const item of raw) {
    if (!isRecord(item)) continue;
    const provider = asString(item.provider);
    const source = asString(item.source);
    const model = asString(item.model);
    const requests = asNumber(item.requests);
    const inputTokens = asNumber(item.input_tokens);
    const cachedTokens = asNumber(item.cached_tokens);
    const outputTokens = asNumber(item.output_tokens);
    const reasoningTokens = asNumber(item.reasoning_tokens) ?? 0;
    const totalLatencyMs = asNumber(item.total_latency_ms) ?? 0;

    if (
      provider === null ||
      source === null ||
      model === null ||
      requests === null ||
      inputTokens === null ||
      cachedTokens === null ||
      outputTokens === null
    ) {
      continue;
    }
    rows.push({
      provider,
      source,
      model,
      requests,
      input_tokens: inputTokens,
      cached_tokens: cachedTokens,
      output_tokens: outputTokens,
      reasoning_tokens: reasoningTokens,
      total_latency_ms: totalLatencyMs,
    });
  }
  return rows;
}

function parseStatsSnapshot(raw: unknown): StatsSnapshot | null {
  if (!isRecord(raw) || !isRecord(raw.usage)) return null;
  const u = raw.usage;
  const requests = asNumber(u.requests);
  const promptTokens = asNumber(u.prompt_tokens);
  const cachedTokens = asNumber(u.cached_tokens);
  const outputTokens = asNumber(u.output_tokens);
  const reasoningTokens = asNumber(u.reasoning_tokens);
  const cacheHitRatio = asNumber(u.cache_hit_ratio);

  if (
    requests === null ||
    promptTokens === null ||
    cachedTokens === null ||
    outputTokens === null ||
    reasoningTokens === null ||
    cacheHitRatio === null
  ) {
    return null;
  }

  const rawMaxCtx = asOptionalNullableNumber(raw.max_context_tokens);
  const maxContextTokens = rawMaxCtx !== null && rawMaxCtx > 0 ? rawMaxCtx : null;

  return {
    usage: {
      requests,
      prompt_tokens: promptTokens,
      cached_tokens: cachedTokens,
      output_tokens: outputTokens,
      reasoning_tokens: reasoningTokens,
      cache_hit_ratio: cacheHitRatio,
    },
    proxy_rss_bytes: asOptionalNullableNumber(raw.proxy_rss_bytes),
    max_context_tokens: maxContextTokens,
    recent_calls: parseRecentCalls(raw.recent_calls),
    by_model: parseByModel(raw.by_model),
  };
}

function parseLauncherIntegration(raw: unknown): LauncherIntegrationStatus | null {
  if (!isRecord(raw)) return null;
  const integrationState = asIntegrationState(raw.state);
  const enabled = asBoolean(raw.enabled);
  const hybridActive = asBoolean(raw.hybrid_active);
  if (integrationState === null || enabled === null || hybridActive === null) {
    return null;
  }

  return {
    state: integrationState,
    enabled,
    hybrid_active: hybridActive,
    shell: asOptionalNullableString(raw.shell),
    rc_path: asOptionalNullableString(raw.rc_path),
    env_script_path: asOptionalNullableString(raw.env_script_path),
    binary_path: asOptionalNullableString(raw.binary_path),
    symlink_target: asOptionalNullableString(raw.symlink_target),
    manual_snippet: asOptionalNullableString(raw.manual_snippet),
    message: asOptionalNullableString(raw.message),
  };
}

function parseModelPresetsWithFallback(
  raw: unknown,
  fallback: ReadonlyArray<ModelPreset>,
): ModelPreset[] {
  if (!Array.isArray(raw)) {
    return [...fallback];
  }
  const presets: ModelPreset[] = [];
  for (const item of raw) {
    if (!isRecord(item)) continue;
    const id = asString(item.id)?.trim() ?? "";
    const displayName = asString(item.display_name)?.trim() ?? "";
    const shortLabel = asString(item.short_label)?.trim() ?? "";
    if (id && displayName && shortLabel) {
      presets.push({ id, display_name: displayName, short_label: shortLabel });
    }
  }
  return presets.length > 0 ? presets : [...fallback];
}

export function parseSettingsSnapshot(raw: unknown): SettingsSnapshot | null {
  if (!isRecord(raw)) return null;
  const config = parseRedactedConfig(raw.config);
  const binaryVersion = asString(raw.binary_version);
  const proxy = parseProxyHealth(raw.proxy);
  const versionSkew = asBoolean(raw.version_skew);
  if (!config || binaryVersion === null || !proxy || versionSkew === null) {
    return null;
  }

  let stats: StatsSnapshot | null = null;
  if (raw.stats !== null && raw.stats !== undefined) {
    stats = parseStatsSnapshot(raw.stats);
    if (stats === null) {
      return null;
    }
  }

  const launcherIntegration =
    raw.launcher_integration !== undefined && raw.launcher_integration !== null
      ? parseLauncherIntegration(raw.launcher_integration)
      : null;
  const supportedModels = parseModelPresetsWithFallback(
    raw.supported_models,
    DEFAULT_MODEL_PRESETS,
  );
  const supportedCodexModels = parseModelPresetsWithFallback(
    raw.supported_codex_models,
    DEFAULT_CODEX_MODEL_PRESETS,
  );
  let codexLoginPending = false;
  if (raw.codex_login_pending !== undefined) {
    const parsedPending = asBoolean(raw.codex_login_pending);
    if (parsedPending === null) return null;
    codexLoginPending = parsedPending;
  }
  const platform = normalizeHostPlatform(raw.platform);
  const accountPlan = parseAccountPlanStatus(raw.account_plan);
  const topRss = asOptionalNullableNumber(raw.proxy_rss_bytes);
  const proxyRssBytes = topRss !== null ? topRss : (stats?.proxy_rss_bytes ?? null);
  const lsTelemetryActive = asBoolean(raw.ls_telemetry_active) ?? false;
  const lastCatalogRefreshAtMs = asOptionalNullableNumber(raw.last_catalog_refresh_at_ms);

  return {
    config,
    binary_version: binaryVersion,
    proxy,
    version_skew: versionSkew,
    stats,
    launcher_integration: launcherIntegration,
    supported_models: supportedModels,
    supported_codex_models: supportedCodexModels,
    codex_login_pending: codexLoginPending,
    platform,
    account_plan: accountPlan,
    proxy_rss_bytes: proxyRssBytes,
    ls_telemetry_active: lsTelemetryActive,
    last_catalog_refresh_at_ms: lastCatalogRefreshAtMs,
  };
}

function parseUsageBucket(raw: unknown): UsageBucket | null {
  if (!isRecord(raw)) return null;
  const label = asString(raw.label);
  const requests = asNumber(raw.requests);
  const inputTokens = asNumber(raw.input_tokens);
  const cachedTokens = asNumber(raw.cached_tokens);
  const outputTokens = asNumber(raw.output_tokens);
  const reasoningTokens = asNumber(raw.reasoning_tokens) ?? 0;

  if (
    label === null ||
    requests === null ||
    inputTokens === null ||
    cachedTokens === null ||
    outputTokens === null
  ) {
    return null;
  }

  return {
    label,
    requests,
    input_tokens: inputTokens,
    cached_tokens: cachedTokens,
    output_tokens: outputTokens,
    reasoning_tokens: reasoningTokens,
  };
}

function parseUsageBreakdownRow(raw: unknown): UsageBreakdownRow | null {
  if (!isRecord(raw)) return null;
  const key = asString(raw.key);
  const provider = asString(raw.provider);
  const requests = asNumber(raw.requests);
  const inputTokens = asNumber(raw.input_tokens);
  const cachedTokens = asNumber(raw.cached_tokens);
  const outputTokens = asNumber(raw.output_tokens);
  const reasoningTokens = asNumber(raw.reasoning_tokens) ?? 0;
  const avgLatencyMs = asNumber(raw.avg_latency_ms) ?? 0;

  if (
    key === null ||
    provider === null ||
    requests === null ||
    inputTokens === null ||
    cachedTokens === null ||
    outputTokens === null
  ) {
    return null;
  }

  return {
    key,
    provider,
    source: asOptionalNullableString(raw.source),
    model_count: asOptionalNullableNumber(raw.model_count),
    requests,
    input_tokens: inputTokens,
    cached_tokens: cachedTokens,
    output_tokens: outputTokens,
    reasoning_tokens: reasoningTokens,
    avg_latency_ms: avgLatencyMs,
  };
}

export function parseUsageSummary(raw: unknown): UsageSummary | null {
  if (!isRecord(raw)) return null;
  const rangeRaw = raw.range;
  if (rangeRaw !== "1d" && rangeRaw !== "7d" && rangeRaw !== "30d") {
    return null;
  }
  const sinceUtc = asString(raw.since_utc);
  const totals = parseUsageBucket(raw.totals);
  if (sinceUtc === null || !totals) return null;

  const buckets: UsageBucket[] = [];
  if (Array.isArray(raw.buckets)) {
    for (const item of raw.buckets) {
      const b = parseUsageBucket(item);
      if (b) buckets.push(b);
    }
  }

  const byModel: UsageBreakdownRow[] = [];
  if (Array.isArray(raw.by_model)) {
    for (const item of raw.by_model) {
      const r = parseUsageBreakdownRow(item);
      if (r) byModel.push(r);
    }
  }

  const byProvider: UsageBreakdownRow[] = [];
  if (Array.isArray(raw.by_provider)) {
    for (const item of raw.by_provider) {
      const r = parseUsageBreakdownRow(item);
      if (r) byProvider.push(r);
    }
  }

  return {
    range: rangeRaw,
    since_utc: sinceUtc,
    totals,
    buckets,
    by_model: byModel,
    by_provider: byProvider,
  };
}

export function parseTestConnectionPayload(raw: unknown): TestConnectionPayload | null {
  if (!isRecord(raw)) return null;
  const ok = asBoolean(raw.ok);
  const message = asString(raw.message);
  if (ok === null || message === null) return null;
  return { ok, message };
}

export function parseStartCodexLoginPayload(raw: unknown): StartCodexLoginPayload | null {
  if (!isRecord(raw)) return null;
  const authorizeUrl = asString(raw.authorize_url)?.trim() ?? "";
  const callbackListening = asBoolean(raw.callback_listening);
  const manualFallback = asBoolean(raw.manual_fallback);
  if (!authorizeUrl || callbackListening === null || manualFallback === null) return null;
  return {
    authorize_url: authorizeUrl,
    callback_listening: callbackListening,
    manual_fallback: manualFallback,
  };
}

export function extractToolText(result: ToolCallResultLike): string {
  if (!Array.isArray(result.content)) return "";
  return result.content
    .filter((block) => block.type === "text" && typeof block.text === "string")
    .map((block) => block.text ?? "")
    .join("\n")
    .trim();
}

export function extractStructuredPayload(result: ToolCallResultLike): unknown {
  if (result.structuredContent !== undefined && result.structuredContent !== null) {
    return result.structuredContent;
  }
  const text = extractToolText(result);
  if (!text) return null;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return null;
  }
}

export function extractToolErrorText(result: ToolCallResultLike): string {
  if (isRecord(result.structuredContent)) {
    const structuredError = asString(result.structuredContent.error);
    if (structuredError) return structuredError;
  }
  const text = extractToolText(result);
  return text || "The companion API reported an error.";
}

export function formatCaughtError(err: unknown): string {
  if (err instanceof Error && err.message.trim().length > 0) {
    return err.message;
  }
  return "Request failed. Check the companion sidecar connection.";
}

export function validateApiKey(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) {
    return "Paste a Google AI Studio API key first.";
  }
  if (trimmed.toLowerCase() === PLACEHOLDER_API_KEY) {
    return "Placeholder key cannot be saved. Paste a real Gemini API key.";
  }
  if (!/^[\x21-\x7e]+$/.test(trimmed)) {
    return "API key must contain printable ASCII characters with no spaces.";
  }
  return null;
}
