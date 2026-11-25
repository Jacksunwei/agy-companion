import {
  App,
  applyDocumentTheme,
  applyHostStyleVariables,
  type McpUiHostContext,
} from "@modelcontextprotocol/ext-apps";
import "./style.css";

type HostPlatform = "macos" | "linux" | "windows";
type ActiveTab = "monitor" | "settings";
type StatusTone = "ok" | "warn" | "err" | "info";
type CloudCodeModeId = "auto" | "daily" | "prod" | "custom";
type IntegrationState =
  | "active"
  | "enabled"
  | "manual_required"
  | "disabled"
  | "unsupported";

interface ProxyConfig {
  host: string;
  port: number;
}

interface CloudCodeConfig {
  upstream: string;
}

interface RedactedGeminiConfig {
  base_url: string;
  default_model: string;
  timeout_secs: number;
  api_key_configured: boolean;
  api_key_hint: string | null;
}

interface RedactedProvidersConfig {
  gemini: RedactedGeminiConfig;
}

interface RedactedConfig {
  schema_version: number;
  proxy: ProxyConfig;
  cloudcode: CloudCodeConfig;
  providers: RedactedProvidersConfig;
}

interface ProxyHealthPayload {
  reachable: boolean;
  status: string | null;
  version: string | null;
  protocol_version: number | null;
  error: string | null;
}

interface UsageStatsSnapshot {
  requests: number;
  prompt_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_hit_ratio: number;
}

interface CloudCodeStatsSnapshot {
  mode: string;
  resolved_upstream: string | null;
  last_catalog_refresh_at_ms: number | null;
  last_catalog_included_proxy_models: boolean | null;
}

interface StatsSnapshot {
  usage: UsageStatsSnapshot;
  cloudcode: CloudCodeStatsSnapshot;
}

interface CloudCodeStatusPayload {
  mode: string;
  configured_upstream: string;
  resolved_upstream: string | null;
}

interface LauncherIntegrationStatus {
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

interface ModelPreset {
  id: string;
  display_name: string;
  short_label: string;
}

interface SettingsSnapshot {
  config: RedactedConfig;
  binary_version: string;
  proxy: ProxyHealthPayload;
  version_skew: boolean;
  stats: StatsSnapshot | null;
  launcher_integration: LauncherIntegrationStatus | null;
  cloudcode: CloudCodeStatusPayload;
  supported_models: ModelPreset[];
  platform: HostPlatform;
}

type ApiKeyPatch = "clear" | { set: string };

interface ProxySettingsPatch {
  host?: string;
  port?: number;
}

interface CloudCodeSettingsPatch {
  upstream?: string;
}

interface GeminiSettingsPatch {
  api_key?: ApiKeyPatch;
  base_url?: string;
  default_model?: string;
}

interface ProvidersSettingsPatch {
  gemini?: GeminiSettingsPatch;
}

interface SaveSettingsArgs {
  proxy?: ProxySettingsPatch;
  cloudcode?: CloudCodeSettingsPatch;
  providers?: ProvidersSettingsPatch;
}

interface TestConnectionPayload {
  ok: boolean;
  message: string;
}

interface ToolCallResultLike {
  content?: Array<{ type?: string; text?: string }>;
  structuredContent?: unknown;
  isError?: boolean;
}

interface InlineFeedback {
  tone: StatusTone;
  text: string;
}

interface PriorityBannerSpec {
  key: string;
  tone: "info" | "warn" | "err";
  icon: "info" | "warn" | "err";
  title: string;
  sub: string;
  action?: {
    label: string;
    narrowOnly?: boolean;
    requiresCall?: boolean;
    disabled?: boolean;
    onClick: () => void;
  };
}

const CATALOG_REFRESH_INTERVAL_MS = 5 * 60 * 1000;
const PLACEHOLDER_API_KEY = "agy-companion-local";
const SVG_NS = "http://www.w3.org/2000/svg";

const DEFAULT_CONFIG: RedactedConfig = {
  schema_version: 1,
  proxy: {
    host: "127.0.0.1",
    port: 24141,
  },
  cloudcode: {
    upstream: "auto",
  },
  providers: {
    gemini: {
      base_url: "https://generativelanguage.googleapis.com",
      default_model: "gemini-4-hydrogen",
      timeout_secs: 900,
      api_key_configured: false,
      api_key_hint: null,
    },
  },
};

const DEFAULT_MODEL_PRESETS: ReadonlyArray<ModelPreset> = [
  { id: "gemini-4-carbon", display_name: "Gemini 4 Carbon", short_label: "4 Carbon" },
  { id: "gemini-4-neon", display_name: "Gemini 4 Neon", short_label: "4 Neon" },
  { id: "gemini-4-hydrogen", display_name: "Gemini 4 Hydrogen", short_label: "4 Hydrogen" },
  { id: "gemini-4-argon", display_name: "Gemini 4 Argon", short_label: "4 Argon" },
  { id: "gemini-3.8-flash", display_name: "Gemini 3.8 Flash", short_label: "3.8 Flash" },
];

const CLOUDCODE_PRESETS: ReadonlyArray<{
  id: CloudCodeModeId;
  value: string;
  title: string;
  hint: string;
}> = [
  {
    id: "auto",
    value: "auto",
    title: "Auto",
    hint: "Detects Personal AI Pro or Workspace automatically",
  },
  {
    id: "daily",
    value: "https://daily-cloudcode-pa.googleapis.com",
    title: "Personal Google AI Pro",
    hint: "daily-cloudcode-pa.googleapis.com",
  },
  {
    id: "prod",
    value: "https://cloudcode-pa.googleapis.com",
    title: "Workspace / PayGo",
    hint: "cloudcode-pa.googleapis.com",
  },
  {
    id: "custom",
    value: "__custom__",
    title: "Custom",
    hint: "Specify a custom CloudCode endpoint",
  },
];

interface AppState {
  connected: boolean;
  standalone: boolean;
  loadError: string | null;
  hydratedFromInitialResult: boolean;
  snapshotEpoch: number;
  snapshot: SettingsSnapshot | null;
  savedPort: number;
  portChangedSinceSave: boolean;
  tab: ActiveTab;
  tabPinnedByUser: boolean;
  initialTabApplied: boolean;
  clearConfirmOpen: boolean;
  customModelOpen: boolean;
  cloudCodeSelection: CloudCodeModeId;
  activeCalls: number;
  refreshing: boolean;
  saving: boolean;
  savingKey: boolean;
  clearing: boolean;
  testing: boolean;
  updatingIntegration: boolean;
  launcherFeedbackFromSnapshot: boolean;
  lastSaveVerified: boolean;
  catalogTimerId: number | null;
  keyFeedback: InlineFeedback | null;
  modelFeedback: InlineFeedback | null;
  dockFeedback: InlineFeedback | null;
  advFeedback: InlineFeedback | null;
}

const state: AppState = {
  connected: false,
  standalone: false,
  loadError: null,
  hydratedFromInitialResult: false,
  snapshotEpoch: 0,
  snapshot: null,
  savedPort: DEFAULT_CONFIG.proxy.port,
  portChangedSinceSave: false,
  tab: "settings",
  tabPinnedByUser: false,
  initialTabApplied: false,
  clearConfirmOpen: false,
  customModelOpen: false,
  cloudCodeSelection: "auto",
  activeCalls: 0,
  refreshing: false,
  saving: false,
  savingKey: false,
  clearing: false,
  testing: false,
  updatingIntegration: false,
  launcherFeedbackFromSnapshot: false,
  lastSaveVerified: false,
  catalogTimerId: null,
  keyFeedback: null,
  modelFeedback: null,
  dockFeedback: null,
  advFeedback: null,
};

/* =========================================================================
 * TYPE GUARDS & SNAPSHOT PARSERS
 * ========================================================================= */

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function asNullableString(value: unknown): string | null | undefined {
  if (value === null) return null;
  return typeof value === "string" ? value : undefined;
}

function asNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function asNullableNumber(value: unknown): number | null | undefined {
  if (value === null) return null;
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function asBoolean(value: unknown): boolean | null {
  return typeof value === "boolean" ? value : null;
}

function asOptionalNullableString(value: unknown): string | null {
  if (value === undefined || value === null) return null;
  return typeof value === "string" ? value : null;
}

function asOptionalNullableNumber(value: unknown): number | null {
  if (value === undefined || value === null) return null;
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function asOptionalNullableBoolean(value: unknown): boolean | null {
  if (value === undefined || value === null) return null;
  return typeof value === "boolean" ? value : null;
}

function detectClientPlatform(): HostPlatform {
  const ua = typeof navigator !== "undefined" ? navigator.userAgent.toLowerCase() : "";
  if (ua.includes("win")) return "windows";
  if (ua.includes("mac")) return "macos";
  return "linux";
}

function normalizeHostPlatform(raw: unknown): HostPlatform {
  if (raw === "macos" || raw === "linux" || raw === "windows") {
    return raw;
  }
  return detectClientPlatform();
}

function asIntegrationState(value: unknown): IntegrationState | null {
  if (
    value === "active" ||
    value === "enabled" ||
    value === "manual_required" ||
    value === "disabled" ||
    value === "unsupported"
  ) {
    return value;
  }
  return null;
}

function parseRedactedConfig(raw: unknown): RedactedConfig | null {
  if (!isRecord(raw)) return null;
  const schemaVersion = asNumber(raw.schema_version);
  if (
    schemaVersion === null ||
    !isRecord(raw.proxy) ||
    !isRecord(raw.cloudcode) ||
    !isRecord(raw.providers) ||
    !isRecord(raw.providers.gemini)
  ) {
    return null;
  }

  const host = asString(raw.proxy.host);
  const port = asNumber(raw.proxy.port);
  const upstream = asString(raw.cloudcode.upstream);
  const gem = raw.providers.gemini;
  const baseUrl = asString(gem.base_url);
  const defaultModel = asString(gem.default_model);
  const timeoutSecs = asNumber(gem.timeout_secs);
  const apiKeyConfigured = asBoolean(gem.api_key_configured);
  const apiKeyHint = asNullableString(gem.api_key_hint);

  if (
    host === null ||
    port === null ||
    upstream === null ||
    baseUrl === null ||
    defaultModel === null ||
    timeoutSecs === null ||
    apiKeyConfigured === null ||
    apiKeyHint === undefined
  ) {
    return null;
  }

  return {
    schema_version: schemaVersion,
    proxy: {
      host,
      port,
    },
    cloudcode: {
      upstream,
    },
    providers: {
      gemini: {
        base_url: baseUrl,
        default_model: defaultModel,
        timeout_secs: timeoutSecs,
        api_key_configured: apiKeyConfigured,
        api_key_hint: apiKeyHint,
      },
    },
  };
}

function parseProxyHealth(raw: unknown): ProxyHealthPayload | null {
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

  let cloudcode: CloudCodeStatsSnapshot = {
    mode: "",
    resolved_upstream: null,
    last_catalog_refresh_at_ms: null,
    last_catalog_included_proxy_models: null,
  };
  if (raw.cloudcode !== undefined) {
    if (!isRecord(raw.cloudcode)) return null;
    cloudcode = {
      mode: asString(raw.cloudcode.mode) ?? "",
      resolved_upstream: asOptionalNullableString(raw.cloudcode.resolved_upstream),
      last_catalog_refresh_at_ms: asOptionalNullableNumber(
        raw.cloudcode.last_catalog_refresh_at_ms,
      ),
      last_catalog_included_proxy_models: asOptionalNullableBoolean(
        raw.cloudcode.last_catalog_included_proxy_models,
      ),
    };
  }

  return {
    usage: {
      requests,
      prompt_tokens: promptTokens,
      cached_tokens: cachedTokens,
      output_tokens: outputTokens,
      reasoning_tokens: reasoningTokens,
      cache_hit_ratio: cacheHitRatio,
    },
    cloudcode,
  };
}

function parseCloudCodeStatus(
  raw: unknown,
  config: RedactedConfig,
  stats: StatsSnapshot | null,
): CloudCodeStatusPayload {
  if (isRecord(raw)) {
    const mode = asString(raw.mode);
    const configured = asString(raw.configured_upstream);
    const rawResolved = asOptionalNullableString(raw.resolved_upstream);
    const resolved =
      rawResolved && rawResolved.trim().toLowerCase() !== "auto" ? rawResolved : null;
    if (mode !== null && configured !== null) {
      return {
        mode,
        configured_upstream: configured,
        resolved_upstream: resolved,
      };
    }
  }

  const configuredUpstream = config.cloudcode.upstream;
  const isAuto = configuredUpstream.trim().toLowerCase() === "auto";
  const mode = stats?.cloudcode.mode || (isAuto ? "auto" : "fixed");
  const rawResolved =
    stats?.cloudcode.resolved_upstream ||
    (mode === "fixed" ? configuredUpstream : null);
  const resolved =
    rawResolved && rawResolved.trim().toLowerCase() !== "auto" ? rawResolved : null;
  return {
    mode,
    configured_upstream: configuredUpstream,
    resolved_upstream: resolved,
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

function parseModelPresets(raw: unknown): ModelPreset[] {
  if (!Array.isArray(raw)) {
    return [...DEFAULT_MODEL_PRESETS];
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
  return presets.length > 0 ? presets : [...DEFAULT_MODEL_PRESETS];
}

function activeModelPresets(snap: SettingsSnapshot | null): ReadonlyArray<ModelPreset> {
  return snap && snap.supported_models.length > 0 ? snap.supported_models : DEFAULT_MODEL_PRESETS;
}

function parseSettingsSnapshot(raw: unknown): SettingsSnapshot | null {
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
  const cloudcode = parseCloudCodeStatus(raw.cloudcode, config, stats);
  const supportedModels = parseModelPresets(raw.supported_models);
  const platform = normalizeHostPlatform(raw.platform);

  return {
    config,
    binary_version: binaryVersion,
    proxy,
    version_skew: versionSkew,
    stats,
    launcher_integration: launcherIntegration,
    cloudcode,
    supported_models: supportedModels,
    platform,
  };
}

function parseTestConnectionPayload(raw: unknown): TestConnectionPayload | null {
  if (!isRecord(raw)) return null;
  const ok = asBoolean(raw.ok);
  const message = asString(raw.message);
  if (ok === null || message === null) return null;
  return { ok, message };
}

function extractToolText(result: ToolCallResultLike): string {
  if (!Array.isArray(result.content)) return "";
  return result.content
    .filter((block) => block.type === "text" && typeof block.text === "string")
    .map((block) => block.text ?? "")
    .join("\n")
    .trim();
}

function extractStructuredPayload(result: ToolCallResultLike): unknown {
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

function extractToolErrorText(result: ToolCallResultLike): string {
  if (isRecord(result.structuredContent)) {
    const structuredError = asString(result.structuredContent.error);
    if (structuredError) return structuredError;
  }
  const text = extractToolText(result);
  return text || "The MCP tool reported an error.";
}

function formatCaughtError(err: unknown): string {
  if (err instanceof Error && err.message.trim().length > 0) {
    return err.message;
  }
  return "Request failed. Check the companion MCP server connection.";
}

/* =========================================================================
 * VALIDATION & DOMAIN FORMATTERS
 * ========================================================================= */

function parseAsciiU32Prefix(s: string): { value: number; rest: string } | null {
  const m = /^([0-9]+)([\s\S]*)$/.exec(s);
  if (!m) return null;
  const value = Number(m[1]);
  if (!Number.isSafeInteger(value) || value > 0xffff_ffff) return null;
  return { value, rest: m[2] };
}

function isSupportedGeminiModel(model: string): boolean {
  if (!model.startsWith("gemini-")) {
    return false;
  }
  const majorParsed = parseAsciiU32Prefix(model.slice("gemini-".length));
  if (!majorParsed) {
    return false;
  }
  const { value: major, rest: afterMajor } = majorParsed;
  if (major >= 4) {
    return (
      (afterMajor.startsWith("-") || afterMajor.startsWith(".")) &&
      afterMajor.length > 1
    );
  }
  if (major === 3) {
    if (!afterMajor.startsWith(".")) {
      return false;
    }
    const minorParsed = parseAsciiU32Prefix(afterMajor.slice(1));
    if (!minorParsed || minorParsed.value < 8) {
      return false;
    }
    const afterMinor = minorParsed.rest;
    return (
      afterMinor.length === 0 ||
      ((afterMinor.startsWith("-") || afterMinor.startsWith(".")) &&
        afterMinor.length > 1)
    );
  }
  return false;
}

function validateEndpointUrl(raw: string, label: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) {
    return `${label} cannot be empty.`;
  }
  if (trimmed.includes("\\") || trimmed.includes("..")) {
    return `${label} contains invalid path characters.`;
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return `${label} must be a valid URL.`;
  }
  if (parsed.username || parsed.password) {
    return `${label} cannot include embedded credentials.`;
  }
  if (parsed.protocol === "https:") {
    return null;
  }
  if (
    parsed.protocol === "http:" &&
    (parsed.hostname === "127.0.0.1" || parsed.hostname === "localhost")
  ) {
    return null;
  }
  return `${label} must use https:// (http:// is allowed only for 127.0.0.1 or localhost).`;
}

function validateApiKey(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) {
    return "Paste a Gemini API key first.";
  }
  if (trimmed.toLowerCase() === PLACEHOLDER_API_KEY) {
    return "Placeholder key cannot be saved. Paste a real Gemini API key.";
  }
  if (!/^[\x21-\x7e]+$/.test(trimmed)) {
    return "API key must contain printable ASCII characters with no spaces.";
  }
  return null;
}

function classifyCloudCodeMode(configuredUrl: string): CloudCodeModeId {
  const normalized = configuredUrl.trim().replace(/\/+$/, "").toLowerCase();
  if (!normalized || normalized === "auto") return "auto";
  if (normalized === "https://daily-cloudcode-pa.googleapis.com") return "daily";
  if (normalized === "https://cloudcode-pa.googleapis.com") return "prod";
  return "custom";
}

function fmtCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  if (n >= 10_000) return `${Math.round(n / 1000)}K`;
  if (n >= 1000) return `${(n / 1000).toFixed(1).replace(/\.0$/, "")}K`;
  return String(n);
}

function fmtExact(n: number): string {
  return Number(n).toLocaleString("en-US");
}

function fmtCountdownMs(ms: number): string {
  const totalSeconds = Math.max(0, Math.ceil(ms / 1000));
  const mins = Math.floor(totalSeconds / 60);
  const secs = totalSeconds % 60;
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

function shortUpstreamName(url: string | null): string | null {
  if (!url) return null;
  const lower = url.toLowerCase();
  if (lower.includes("daily-cloudcode-pa")) return "AI Pro";
  if (lower.includes("cloudcode-pa.googleapis.com")) return "Workspace";
  try {
    return new URL(url).hostname;
  } catch {
    return url;
  }
}

function formatCloudCodeShort(snap: SettingsSnapshot): string {
  if (!snap.proxy.reachable) return "Proxy offline";
  const cc = snap.cloudcode;
  const resolved = shortUpstreamName(cc.resolved_upstream);
  if (cc.mode === "auto") {
    return resolved ? `Auto \u2192 ${resolved}` : "Auto";
  }
  return shortUpstreamName(cc.configured_upstream) || "Custom";
}

function quitChord(platform: HostPlatform): string {
  return platform === "macos" ? "Quit (\u2318Q) and reopen" : "Quit and reopen";
}

function platformLabel(platform: HostPlatform): string {
  if (platform === "macos") return "macOS";
  if (platform === "windows") return "Windows";
  return "Linux";
}

function buildLaunchCommand(port: number): string {
  const safePort =
    Number.isInteger(port) && port >= 1 && port <= 65535 ? port : DEFAULT_CONFIG.proxy.port;
  return `agy-companion launch --port ${safePort}`;
}

function buildCustomModelsSnippet(host: string, port: number, model: string): string {
  const safeHost = host.trim() || DEFAULT_CONFIG.proxy.host;
  const safePort =
    Number.isInteger(port) && port >= 1 && port <= 65535 ? port : DEFAULT_CONFIG.proxy.port;
  const safeModel = model.trim() || DEFAULT_CONFIG.providers.gemini.default_model;
  return JSON.stringify(
    {
      modelProvider: "gemini",
      customModelsConfig: {
        customModels: [
          {
            modelName: `gemini-api://http://${safeHost}:${safePort}/models/${safeModel}`,
            label: `Antigravity Companion (${safeModel})`,
          },
        ],
      },
    },
    null,
    2,
  );
}

/* =========================================================================
 * SAFE DOM & SVG HELPERS (S5 COMPLIANT)
 * ========================================================================= */

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string | null,
  text?: string | number | null,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function svgEl(tag: string, attrs?: Record<string, string | number>): SVGElement {
  const node = document.createElementNS(SVG_NS, tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      node.setAttribute(k, String(v));
    }
  }
  return node;
}

function createBrandMark(): SVGElement {
  const svg = svgEl("svg", {
    viewBox: "0 0 128 128",
    "aria-hidden": "true",
    focusable: "false",
  });
  const bg = svgEl("rect", {
    x: 6,
    y: 6,
    width: 116,
    height: 116,
    rx: 28,
    fill: "#1A73E8",
  });
  const orbit = svgEl("ellipse", {
    cx: 64,
    cy: 64,
    rx: 40,
    ry: 16,
    transform: "rotate(-28 64 64)",
    fill: "none",
    stroke: "#CFE3FF",
    "stroke-width": 4.5,
    "stroke-opacity": 0.85,
  });
  const spark = svgEl("path", {
    d: "M64 32 C64 51 77 64 96 64 C77 64 64 77 64 96 C64 77 51 64 32 64 C51 64 64 51 64 32 Z",
    fill: "#FFFFFF",
  });
  const sat = svgEl("circle", { cx: 99, cy: 45, r: 6.5, fill: "#FFFFFF" });
  svg.append(bg, orbit, spark, sat);
  return svg;
}

type IconKind = "monitor" | "settings" | "refresh" | "chev" | "info" | "warn" | "err";

function createIcon(kind: IconKind): SVGElement {
  const svg = svgEl("svg", {
    viewBox: "0 0 24 24",
    "aria-hidden": "true",
    focusable: "false",
  });
  if (kind === "monitor") {
    svg.append(svgEl("polyline", { points: "22 12 18 12 15 20 9 4 6 12 2 12" }));
  } else if (kind === "settings") {
    svg.append(
      svgEl("circle", { cx: 12, cy: 12, r: 3 }),
      svgEl("path", {
        d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9c.26.6.85 1 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z",
      }),
    );
  } else if (kind === "refresh") {
    svg.append(
      svgEl("path", { d: "M21 12a9 9 0 1 1-2.64-6.36" }),
      svgEl("polyline", { points: "21 3 21 9 15 9" }),
    );
  } else if (kind === "chev") {
    svg.append(svgEl("polyline", { points: "9 18 15 12 9 6" }));
  } else if (kind === "info") {
    svg.append(
      svgEl("circle", { cx: 12, cy: 12, r: 9 }),
      svgEl("line", { x1: 12, y1: 11, x2: 12, y2: 16 }),
      svgEl("circle", { cx: 12, cy: 8, r: 0.6, fill: "currentColor" }),
    );
  } else if (kind === "warn") {
    svg.append(
      svgEl("path", {
        d: "M10.3 3.9 1.8 18.5a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
      }),
      svgEl("line", { x1: 12, y1: 9.5, x2: 12, y2: 13.5 }),
      svgEl("circle", { cx: 12, cy: 17, r: 0.6, fill: "currentColor" }),
    );
  } else if (kind === "err") {
    svg.append(
      svgEl("circle", { cx: 12, cy: 12, r: 9 }),
      svgEl("line", { x1: 15, y1: 9, x2: 9, y2: 15 }),
      svgEl("line", { x1: 9, y1: 9, x2: 15, y2: 15 }),
    );
  }
  return svg;
}

async function copyWithFallback(
  text: string,
  button: HTMLButtonElement,
  selectTarget?: HTMLElement,
): Promise<void> {
  let copied = false;

  // Tier 1: Clipboard API
  if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
    } catch {
      copied = false;
    }
  }

  // Tier 2: Off-screen readonly textarea + execCommand
  if (!copied) {
    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "true");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    textarea.style.pointerEvents = "none";
    document.body.appendChild(textarea);
    try {
      textarea.focus();
      textarea.select();
      copied = document.execCommand("copy");
    } catch {
      copied = false;
    } finally {
      textarea.remove();
    }
  }

  const idleLabel = button.getAttribute("data-idle-label") ?? button.textContent ?? "Copy";
  if (copied) {
    button.textContent = "Copied";
    window.setTimeout(() => {
      if (button.textContent === "Copied") {
        button.textContent = idleLabel;
      }
    }, 1400);
    return;
  }

  // Tier 3: Select snippet range for manual Cmd/Ctrl+C
  if (selectTarget) {
    const selection = window.getSelection();
    if (selection) {
      const range = document.createRange();
      range.selectNodeContents(selectTarget);
      selection.removeAllRanges();
      selection.addRange(range);
    }
  }
  button.textContent = "Press \u2318/Ctrl+C";
  window.setTimeout(() => {
    if (button.textContent === "Press \u2318/Ctrl+C") {
      button.textContent = idleLabel;
    }
  }, 2400);
}

/* =========================================================================
 * UI REFS & STATIC MOUNT
 * ========================================================================= */

interface CloudCodeRowRef {
  id: CloudCodeModeId;
  value: string;
  title: string;
  row: HTMLDivElement;
  radio: HTMLInputElement;
  resolvedTag: HTMLSpanElement | null;
}

interface UiRefs {
  root: HTMLElement;
  verPill: HTMLSpanElement;
  monitorTabBtn: HTMLButtonElement;
  settingsTabBtn: HTMLButtonElement;
  settingsAttnDot: HTMLSpanElement;
  banners: HTMLDivElement;
  bannerActionBtn: HTMLButtonElement | null;
  bannerActionRequiresCall: boolean;
  activeBannerKey: string | null;
  activeBannerTitleEl: HTMLElement | null;

  // Monitor column
  colMonitor: HTMLDivElement;

  // Settings column — Gemini API Key
  keyStatusWrap: HTMLSpanElement;
  apiKeyInput: HTMLInputElement;
  saveKeyBtn: HTMLButtonElement;
  testBtn: HTMLButtonElement;
  clearKeyBtn: HTMLButtonElement;
  clearConfirmRow: HTMLDivElement;
  confirmClearBtn: HTMLButtonElement;
  cancelClearBtn: HTMLButtonElement;
  keyFeedbackLine: HTMLDivElement;

  // Settings column — Default Model
  modelCurrentVal: HTMLSpanElement;
  modelPillStrip: HTMLDivElement;
  renderedModelPresetsKey: string;
  modelPresetBtns: Array<{ id: string; btn: HTMLButtonElement }>;
  onSelectModelPreset: ((modelId: string) => void) | null;
  customModelToggleBtn: HTMLButtonElement;
  customModelRow: HTMLDivElement;
  customModelInput: HTMLInputElement;
  customModelApplyBtn: HTMLButtonElement;
  modelFeedbackLine: HTMLDivElement;

  // Settings column — Dock auto-start
  dockSwitch: HTMLInputElement;
  dockTag: HTMLSpanElement;
  dockSub: HTMLDivElement;
  dockMeta: HTMLDivElement;
  manualWrap: HTMLDivElement;
  manualLead: HTMLDivElement;
  manualChip: HTMLDivElement;
  manualCode: HTMLElement;
  manualCopyBtn: HTMLButtonElement;
  recheckBtn: HTMLButtonElement;
  dockFeedbackLine: HTMLDivElement;

  // Settings column — Advanced <details>
  advancedDetails: HTMLDetailsElement;
  cloudCodeRows: CloudCodeRowRef[];
  cloudCodeCustomRow: HTMLDivElement;
  cloudCodeCustomInput: HTMLInputElement;
  cloudCodeCustomApplyBtn: HTMLButtonElement;
  baseUrlInput: HTMLInputElement;
  listenPortInput: HTMLInputElement;
  saveEndpointBtn: HTMLButtonElement;
  advFeedbackLine: HTMLDivElement;
  launchCmdCode: HTMLElement;
  launchCopyBtn: HTMLButtonElement;
  cliJsonPre: HTMLPreElement;
  cliJsonCopyBtn: HTMLButtonElement;

  // Monitor quick Dock switch (recreated per renderMonitor, synced on busy updates)
  monitorDockSwitch: HTMLInputElement | null;

  // Pinned footer
  footerDot: HTMLSpanElement;
  endpointTag: HTMLSpanElement;
  footerRoute: HTMLSpanElement;
  refreshBtn: HTMLButtonElement;
}

function mountUi(root: HTMLElement): UiRefs {
  root.classList.add("companion-root");
  root.setAttribute("data-tab", state.tab);

  const pane = el("div", "pane");

  /* --- Zone 1: Pinned Header (44px) --- */
  const hdr = el("header", "hdr");
  const brand = el("div", "brand");
  const brandMark = el("span", "brand-mark");
  brandMark.append(createBrandMark());
  const brandName = el("h1", "brand-name", "Antigravity Companion");
  const verPill = el("span", "ver num", "v0.1.0");
  brand.append(brandMark, brandName, verPill);

  const hdrActions = el("div", "hdr-actions hdr-tabs");
  const monitorTabBtn = el("button", "hdr-btn");
  monitorTabBtn.type = "button";
  monitorTabBtn.title = "Monitor";
  monitorTabBtn.setAttribute("aria-label", "Monitor");
  monitorTabBtn.setAttribute("aria-pressed", String(state.tab === "monitor"));
  monitorTabBtn.append(createIcon("monitor"));

  const settingsTabBtn = el("button", "hdr-btn");
  settingsTabBtn.type = "button";
  settingsTabBtn.title = "Settings";
  settingsTabBtn.setAttribute("aria-label", "Settings");
  settingsTabBtn.setAttribute("aria-pressed", String(state.tab === "settings"));
  settingsTabBtn.append(createIcon("settings"));
  const settingsAttnDot = el("span", "attn-dot");
  settingsAttnDot.hidden = true;
  settingsTabBtn.append(settingsAttnDot);

  hdrActions.append(monitorTabBtn, settingsTabBtn);
  hdr.append(brand, hdrActions);

  /* --- Zone 2: Scrollable Body --- */
  const scroll = el("div", "pane-scroll");
  const wrap = el("div", "content-wrap");
  const banners = el("div", "banners");
  const workbench = el("div", "workbench");
  const colConfig = el("div", "col-config");
  const colMonitor = el("div", "col-monitor");

  /* --- Config Card 1: Gemini API Key + Default Model --- */
  const gemCard = el("section", "card");
  gemCard.setAttribute("aria-label", "Gemini configuration");

  const kh = el("div", "sec-h");
  const keyLabel = el("label", "sec-t", "Gemini API key");
  keyLabel.setAttribute("for", "api-key");
  const keyStatusWrap = el("span", "sec-right");
  keyStatusWrap.append(el("span", "tag", "Not set"));
  kh.append(keyLabel, keyStatusWrap);
  gemCard.append(kh);

  const apiKeyInput = el("input", "inp mono");
  apiKeyInput.id = "api-key";
  apiKeyInput.type = "password";
  apiKeyInput.autocomplete = "off";
  apiKeyInput.spellcheck = false;
  apiKeyInput.setAttribute("autocapitalize", "off");
  apiKeyInput.placeholder = "Paste Gemini API key";
  gemCard.append(apiKeyInput);

  const keyActRow = el("div", "act-row");
  const saveKeyBtn = el("button", "btn btn-primary", "Save & Verify");
  saveKeyBtn.type = "button";
  saveKeyBtn.disabled = true;

  const testBtn = el("button", "btn", "Test");
  testBtn.type = "button";
  testBtn.disabled = true;

  const clearKeyBtn = el("button", "lnk-danger", "Clear");
  clearKeyBtn.type = "button";
  clearKeyBtn.hidden = true;

  keyActRow.append(saveKeyBtn, testBtn, clearKeyBtn);
  gemCard.append(keyActRow);

  const clearConfirmRow = el("div", "confirm-row");
  clearConfirmRow.setAttribute("role", "group");
  clearConfirmRow.setAttribute("aria-label", "Clear API key confirmation");
  clearConfirmRow.hidden = true;
  clearConfirmRow.append(el("span", null, "Remove saved API key?"));
  const confirmClearBtn = el("button", "btn btn-danger-solid btn-sm", "Remove");
  confirmClearBtn.type = "button";
  const cancelClearBtn = el("button", "btn btn-sm", "Cancel");
  cancelClearBtn.type = "button";
  clearConfirmRow.append(confirmClearBtn, cancelClearBtn);
  gemCard.append(clearConfirmRow);

  const keyFeedbackLine = el("div", "status-line");
  keyFeedbackLine.setAttribute("role", "status");
  keyFeedbackLine.setAttribute("aria-live", "polite");
  keyFeedbackLine.hidden = true;
  gemCard.append(keyFeedbackLine);

  // Default Model sub-section
  const modSec = el("div", "divider");
  const mh = el("div", "sec-h");
  const modelCurrentVal = el(
    "span",
    "sec-right sec-val mono",
    DEFAULT_CONFIG.providers.gemini.default_model,
  );
  modelCurrentVal.title = DEFAULT_CONFIG.providers.gemini.default_model;
  mh.append(el("span", "sec-t", "Default model"), modelCurrentVal);
  modSec.append(mh);

  const modelPillStrip = el("div", "pill-strip");
  const modelPresetBtns: Array<{ id: string; btn: HTMLButtonElement }> = [];
  const customModelToggleBtn = el("button", "m-pill", "Custom");
  customModelToggleBtn.type = "button";
  customModelToggleBtn.setAttribute("aria-pressed", "false");
  modelPillStrip.append(customModelToggleBtn);
  modSec.append(modelPillStrip);

  const customModelRow = el("div", "custom-model-row");
  customModelRow.hidden = true;
  const customModelInput = el("input", "inp mono");
  customModelInput.id = "default-model-custom";
  customModelInput.type = "text";
  customModelInput.autocomplete = "off";
  customModelInput.spellcheck = false;
  customModelInput.placeholder = DEFAULT_CONFIG.providers.gemini.default_model;
  customModelInput.value = DEFAULT_CONFIG.providers.gemini.default_model;
  customModelInput.setAttribute("aria-label", "Custom default model ID");
  const customModelApplyBtn = el("button", "btn btn-sm", "Apply");
  customModelApplyBtn.type = "button";
  customModelRow.append(customModelInput, customModelApplyBtn);
  modSec.append(customModelRow);

  const modelFeedbackLine = el("div", "status-line");
  modelFeedbackLine.setAttribute("role", "status");
  modelFeedbackLine.setAttribute("aria-live", "polite");
  modelFeedbackLine.hidden = true;
  modSec.append(modelFeedbackLine);

  gemCard.append(modSec);

  /* --- Config Card 2: Dock auto-start --- */
  const dockCard = el("section", "card");
  dockCard.setAttribute("aria-label", "Dock auto-start");

  const dockOpt = el("div", "opt");
  const dockSw = el("label", "sw");
  const dockSwitch = el("input");
  dockSwitch.type = "checkbox";
  dockSwitch.setAttribute("role", "switch");
  dockSwitch.setAttribute("aria-label", "Dock auto-start");
  const dockTrk = el("span", "trk");
  dockSw.append(dockSwitch, dockTrk);

  const dockTitle = el("span", "opt-title", "Dock auto-start");
  const dockTag = el("span", "tag", "Off");
  const dockSub = el("div", "opt-sub", "Hybrid Mode on app launch");
  const dockMeta = el("div", "opt-meta");
  dockMeta.hidden = true;
  dockOpt.append(dockSw, dockTitle, dockTag, dockSub, dockMeta);
  dockCard.append(dockOpt);

  const manualWrap = el("div", "manual-wrap");
  manualWrap.hidden = true;
  const manualLead = el("div", "manual-lead");
  const manualChip = el("div", "cmd");
  const manualCode = el("code");
  const manualCopyBtn = el("button", "btn btn-sm", "Copy");
  manualCopyBtn.type = "button";
  manualCopyBtn.setAttribute("data-idle-label", "Copy");
  manualChip.append(manualCode, manualCopyBtn);
  const recheckRow = el("div", "act-row");
  const recheckBtn = el("button", "btn btn-sm", "Re-check");
  recheckBtn.type = "button";
  recheckRow.append(recheckBtn);
  manualWrap.append(manualLead, manualChip, recheckRow);
  dockCard.append(manualWrap);

  const dockFeedbackLine = el("div", "status-line");
  dockFeedbackLine.setAttribute("role", "status");
  dockFeedbackLine.setAttribute("aria-live", "polite");
  dockFeedbackLine.hidden = true;
  dockCard.append(dockFeedbackLine);

  /* --- Config Card 3: Collapsed Advanced <details> Drawer --- */
  const advancedDetails = el("details", "adv-details");
  advancedDetails.setAttribute("aria-label", "Advanced settings");

  const advSum = el("summary", "adv-sum");
  const advChev = createIcon("chev");
  advChev.setAttribute("class", "adv-chev");
  advSum.append(
    advChev,
    el("span", "adv-title", "Advanced"),
    el("span", "adv-hint", "Routing, port, CLI"),
  );
  advancedDetails.append(advSum);

  const advBody = el("div", "adv-body");

  // CloudCode routing .pr-row radio cards
  const ccWrap = el("div");
  ccWrap.append(el("span", "field-lbl", "CloudCode routing"));
  const prList = el("div", "pr");
  prList.setAttribute("role", "radiogroup");
  prList.setAttribute("aria-label", "CloudCode routing");

  const cloudCodeRows: CloudCodeRowRef[] = [];
  const cloudCodeCustomRow = el("div", "pr-custom-row");
  cloudCodeCustomRow.hidden = true;
  const cloudCodeCustomInput = el("input", "inp mono");
  cloudCodeCustomInput.id = "cloudcode-custom-url";
  cloudCodeCustomInput.type = "url";
  cloudCodeCustomInput.autocomplete = "off";
  cloudCodeCustomInput.spellcheck = false;
  cloudCodeCustomInput.placeholder = "https://cloudcode-pa.googleapis.com";
  cloudCodeCustomInput.setAttribute("aria-label", "Custom CloudCode endpoint URL");
  const cloudCodeCustomApplyBtn = el("button", "btn btn-sm", "Apply");
  cloudCodeCustomApplyBtn.type = "button";
  cloudCodeCustomRow.append(cloudCodeCustomInput, cloudCodeCustomApplyBtn);

  for (const opt of CLOUDCODE_PRESETS) {
    const row = el("div", opt.id === "auto" ? "pr-row sel" : "pr-row");
    const rWrap = el("span", "pr-radio");
    const rInp = el("input", "pr-inp");
    rInp.type = "radio";
    rInp.name = "cloudcode-route";
    rInp.id = `cc-route-${opt.id}`;
    rInp.value = opt.id;
    rInp.checked = opt.id === "auto";
    const rDot = el("span", "pr-dot");
    rWrap.append(rInp, rDot);

    const body = el("div", "pr-body");
    const titleRow = el("div", "pr-t");
    const titleLbl = el("label", null, opt.title);
    titleLbl.setAttribute("for", rInp.id);
    titleRow.append(titleLbl);

    let resolvedTag: HTMLSpanElement | null = null;
    if (opt.id === "auto") {
      resolvedTag = el("span", "tag tag-brand");
      resolvedTag.hidden = true;
      titleRow.append(resolvedTag);
    }

    body.append(titleRow, el("div", "pr-h", opt.hint));
    if (opt.id === "custom") {
      body.append(cloudCodeCustomRow);
    }

    row.append(rWrap, body);
    prList.append(row);
    cloudCodeRows.push({
      id: opt.id,
      value: opt.value,
      title: opt.title,
      row,
      radio: rInp,
      resolvedTag,
    });
  }
  ccWrap.append(prList);
  advBody.append(ccWrap);

  // Upstream Base URL + Listen Port
  const epWrap = el("div");
  const epGrid = el("div", "ep-grid");

  const fUrl = el("div", "ep-field");
  const urlLbl = el("label", null, "Upstream base URL");
  urlLbl.setAttribute("for", "base-url");
  const baseUrlInput = el("input", "inp mono");
  baseUrlInput.id = "base-url";
  baseUrlInput.type = "url";
  baseUrlInput.autocomplete = "off";
  baseUrlInput.spellcheck = false;
  baseUrlInput.value = DEFAULT_CONFIG.providers.gemini.base_url;
  fUrl.append(urlLbl, baseUrlInput);

  const fPort = el("div", "ep-field");
  const portLbl = el("label", null, "Listen port");
  portLbl.setAttribute("for", "listen-port");
  const listenPortInput = el("input", "inp mono num");
  listenPortInput.id = "listen-port";
  listenPortInput.type = "number";
  listenPortInput.min = "1";
  listenPortInput.max = "65535";
  listenPortInput.step = "1";
  listenPortInput.setAttribute("inputmode", "numeric");
  listenPortInput.value = String(DEFAULT_CONFIG.proxy.port);
  fPort.append(portLbl, listenPortInput);

  epGrid.append(fUrl, fPort);
  epWrap.append(epGrid);

  const advAct = el("div", "act-row");
  const saveEndpointBtn = el("button", "btn btn-sm", "Save endpoint");
  saveEndpointBtn.type = "button";
  advAct.append(saveEndpointBtn);
  epWrap.append(advAct);

  const advFeedbackLine = el("div", "status-line");
  advFeedbackLine.setAttribute("role", "status");
  advFeedbackLine.setAttribute("aria-live", "polite");
  advFeedbackLine.hidden = true;
  epWrap.append(advFeedbackLine);
  advBody.append(epWrap);

  // CLI & Manual Setup
  const cliSec = el("div", "divider cli-stack");
  cliSec.append(el("div", "sec-t", "CLI & manual setup"));

  const initialLaunchCmd = buildLaunchCommand(DEFAULT_CONFIG.proxy.port);
  const launchChip = el("div", "cmd");
  const launchCmdCode = el("code", null, initialLaunchCmd);
  launchCmdCode.title = initialLaunchCmd;
  const launchCopyBtn = el("button", "btn btn-sm", "Copy");
  launchCopyBtn.type = "button";
  launchCopyBtn.setAttribute("data-idle-label", "Copy");
  launchChip.append(launchCmdCode, launchCopyBtn);

  const jsonBlock = el("div", "cmd-block");
  const jbHdr = el("div", "cmd-block-hdr");
  jbHdr.append(el("span", null, "customModelsConfig (agy CLI)"));
  const cliJsonCopyBtn = el("button", "btn btn-sm", "Copy JSON");
  cliJsonCopyBtn.type = "button";
  cliJsonCopyBtn.setAttribute("data-idle-label", "Copy JSON");
  jbHdr.append(cliJsonCopyBtn);
  const cliJsonPre = el(
    "pre",
    null,
    buildCustomModelsSnippet(
      DEFAULT_CONFIG.proxy.host,
      DEFAULT_CONFIG.proxy.port,
      DEFAULT_CONFIG.providers.gemini.default_model,
    ),
  );
  jsonBlock.append(jbHdr, cliJsonPre);

  cliSec.append(launchChip, jsonBlock);
  advBody.append(cliSec);

  advancedDetails.append(advBody);
  colConfig.append(gemCard, dockCard, advancedDetails);

  workbench.append(colConfig, colMonitor);
  wrap.append(banners, workbench);
  scroll.append(wrap);

  /* --- Zone 3: Pinned Status Footer (32px) --- */
  const footer = el("footer", "status-bar");
  const statusItem = el("span", "status-item");
  const footerDot = el("span", "sdot");
  const endpointTag = el("span", "mono num", `${DEFAULT_CONFIG.proxy.host}:${DEFAULT_CONFIG.proxy.port}`);
  statusItem.append(footerDot, endpointTag);

  const footerSep = el("span", "status-sep", "\u00b7");
  const footerRoute = el("span", "status-route", "Connecting\u2026");

  const refreshBtn = el("button", "refresh-btn");
  refreshBtn.type = "button";
  refreshBtn.title = "Refresh";
  refreshBtn.setAttribute("aria-label", "Refresh status");
  refreshBtn.append(createIcon("refresh"));

  footer.append(statusItem, footerSep, footerRoute, refreshBtn);
  pane.append(hdr, scroll, footer);
  root.replaceChildren(pane);

  return {
    root,
    verPill,
    monitorTabBtn,
    settingsTabBtn,
    settingsAttnDot,
    banners,
    bannerActionBtn: null,
    bannerActionRequiresCall: false,
    activeBannerKey: null,
    activeBannerTitleEl: null,
    colMonitor,
    keyStatusWrap,
    apiKeyInput,
    saveKeyBtn,
    testBtn,
    clearKeyBtn,
    clearConfirmRow,
    confirmClearBtn,
    cancelClearBtn,
    keyFeedbackLine,
    modelCurrentVal,
    modelPillStrip,
    renderedModelPresetsKey: "",
    modelPresetBtns,
    onSelectModelPreset: null,
    customModelToggleBtn,
    customModelRow,
    customModelInput,
    customModelApplyBtn,
    modelFeedbackLine,
    dockSwitch,
    dockTag,
    dockSub,
    dockMeta,
    manualWrap,
    manualLead,
    manualChip,
    manualCode,
    manualCopyBtn,
    recheckBtn,
    dockFeedbackLine,
    advancedDetails,
    cloudCodeRows,
    cloudCodeCustomRow,
    cloudCodeCustomInput,
    cloudCodeCustomApplyBtn,
    baseUrlInput,
    listenPortInput,
    saveEndpointBtn,
    advFeedbackLine,
    launchCmdCode,
    launchCopyBtn,
    cliJsonPre,
    cliJsonCopyBtn,
    monitorDockSwitch: null,
    footerDot,
    endpointTag,
    footerRoute,
    refreshBtn,
  };
}

/* =========================================================================
 * RENDERERS
 * ========================================================================= */

function setInlineFeedback(node: HTMLDivElement, fb: InlineFeedback | null): void {
  if (!fb) {
    node.hidden = true;
    node.className = "status-line";
    node.textContent = "";
    return;
  }
  node.hidden = false;
  node.className = fb.tone === "info" ? "status-line" : `status-line ${fb.tone}`;
  node.textContent = fb.text;
}

function needsSettingsAttention(snap: SettingsSnapshot | null): boolean {
  if (!snap) return false;
  const li = snap.launcher_integration;
  return (
    !snap.config.providers.gemini.api_key_configured ||
    Boolean(li && (li.state === "manual_required" || li.state === "enabled")) ||
    state.portChangedSinceSave
  );
}

function renderHeader(refs: UiRefs): void {
  const snap = state.snapshot;
  if (snap) {
    refs.verPill.textContent = `v${snap.binary_version}`;
  }
  refs.root.setAttribute("data-tab", state.tab);
  refs.monitorTabBtn.setAttribute("aria-pressed", String(state.tab === "monitor"));
  refs.settingsTabBtn.setAttribute("aria-pressed", String(state.tab === "settings"));
  refs.settingsAttnDot.hidden = !needsSettingsAttention(snap);
}

function stopCatalogCountdown(): void {
  if (state.catalogTimerId !== null) {
    window.clearInterval(state.catalogTimerId);
    state.catalogTimerId = null;
  }
}

function describeDockState(snap: SettingsSnapshot | null): {
  tagClass: string;
  tagText: string;
  subText: string;
  checked: boolean;
  supported: boolean;
} {
  const li = snap?.launcher_integration ?? null;
  if (!li) {
    return {
      tagClass: "tag",
      tagText: snap ? "Unavailable" : "Off",
      subText: snap ? "Not supported by this companion version" : "Hybrid Mode on app launch",
      checked: false,
      supported: false,
    };
  }
  switch (li.state) {
    case "unsupported":
      return {
        tagClass: "tag",
        tagText: "CLI only",
        subText: "Use agy-companion launch on Windows",
        checked: false,
        supported: false,
      };
    case "manual_required":
      return {
        tagClass: "tag tag-warn",
        tagText: "Manual",
        subText: "Dotfile is a symlink \u2014 manual line needed",
        checked: li.enabled,
        supported: true,
      };
    case "active":
      return {
        tagClass: "tag tag-ok",
        tagText: "Active",
        subText: "Hybrid Mode active on launch",
        checked: true,
        supported: true,
      };
    case "enabled":
      return {
        tagClass: "tag tag-warn",
        tagText: "Restart",
        subText: `${quitChord(snap ? snap.platform : detectClientPlatform())} to activate`,
        checked: true,
        supported: true,
      };
    case "disabled":
      return {
        tagClass: "tag",
        tagText: "Off",
        subText: "Hybrid Mode on app launch",
        checked: false,
        supported: true,
      };
  }
}

function updateCliSnippets(refs: UiRefs): void {
  const host = state.snapshot?.config.proxy.host ?? DEFAULT_CONFIG.proxy.host;
  const parsedPort = Number.parseInt(refs.listenPortInput.value.trim(), 10);
  const port =
    Number.isInteger(parsedPort) && parsedPort >= 1 && parsedPort <= 65535
      ? parsedPort
      : state.savedPort;
  const model =
    state.snapshot?.config.providers.gemini.default_model ??
    DEFAULT_CONFIG.providers.gemini.default_model;

  const launchCmd = buildLaunchCommand(port);
  refs.launchCmdCode.textContent = launchCmd;
  refs.launchCmdCode.title = launchCmd;

  refs.cliJsonPre.textContent = buildCustomModelsSnippet(host, port, model);
}

function updateKeyButtons(refs: UiRefs): void {
  const anyWrite = state.saving || state.clearing || state.updatingIntegration;
  const canCall = state.connected && state.activeCalls < 4;
  const configured = state.snapshot?.config.providers.gemini.api_key_configured ?? false;
  const hasDraftKey = refs.apiKeyInput.value.trim().length > 0;

  if (state.savingKey && state.testing) {
    refs.saveKeyBtn.textContent = "Verifying\u2026";
  } else if (state.savingKey) {
    refs.saveKeyBtn.textContent = "Saving\u2026";
  } else {
    refs.saveKeyBtn.textContent = hasDraftKey || !configured ? "Save & Verify" : "Save";
  }
  refs.saveKeyBtn.disabled = !canCall || anyWrite || state.testing || !hasDraftKey;

  refs.testBtn.textContent = state.testing && !state.savingKey ? "Testing\u2026" : "Test";
  refs.testBtn.disabled = !canCall || state.testing || anyWrite || !configured || hasDraftKey;
  refs.testBtn.title = hasDraftKey
    ? "Click Save & Verify to save and test the new key"
    : configured
      ? "Test the saved API key against Gemini"
      : "Save an API key first";

  refs.clearKeyBtn.disabled = !canCall || anyWrite || state.testing;
  refs.confirmClearBtn.disabled = !canCall || anyWrite || state.testing;
  refs.cancelClearBtn.disabled = anyWrite;
}

function updateBusyControls(refs: UiRefs): void {
  const anyWrite = state.saving || state.clearing || state.updatingIntegration;
  const canCall = state.connected && state.activeCalls < 4;

  refs.refreshBtn.disabled = !canCall || state.refreshing || anyWrite;
  refs.refreshBtn.classList.toggle("spinning", state.refreshing);
  if (refs.bannerActionBtn && refs.bannerActionRequiresCall) {
    refs.bannerActionBtn.disabled = !canCall || state.refreshing || anyWrite;
  }

  updateKeyButtons(refs);

  for (const item of refs.modelPresetBtns) {
    item.btn.disabled = !canCall || anyWrite;
  }
  refs.customModelToggleBtn.disabled = !canCall || anyWrite;
  refs.customModelInput.disabled = !canCall || anyWrite;
  refs.customModelApplyBtn.disabled = !canCall || anyWrite;

  const dockInfo = describeDockState(state.snapshot);
  const dockDisabled = !canCall || anyWrite || !dockInfo.supported;
  refs.dockSwitch.disabled = dockDisabled;
  if (refs.monitorDockSwitch) {
    refs.monitorDockSwitch.disabled = dockDisabled;
  }
  refs.recheckBtn.disabled = !canCall || state.refreshing || anyWrite;

  for (const row of refs.cloudCodeRows) {
    row.radio.disabled = !canCall || anyWrite;
    row.row.classList.toggle("disabled", !canCall || anyWrite);
  }
  refs.cloudCodeCustomInput.disabled = !canCall || anyWrite;
  refs.cloudCodeCustomApplyBtn.disabled = !canCall || anyWrite;
  refs.baseUrlInput.disabled = !canCall || anyWrite;
  refs.listenPortInput.disabled = !canCall || anyWrite;
  refs.saveEndpointBtn.disabled = !canCall || anyWrite;
}

function syncModelPresetPills(refs: UiRefs, presets: ReadonlyArray<ModelPreset>): void {
  const key = presets.map((p) => `${p.id}\0${p.short_label}\0${p.display_name}`).join("\n");
  if (refs.renderedModelPresetsKey === key) {
    return;
  }
  const nextBtns: Array<{ id: string; btn: HTMLButtonElement }> = [];
  for (const preset of presets) {
    const pbtn = el("button", "m-pill", preset.short_label);
    pbtn.type = "button";
    pbtn.title = `${preset.display_name} (${preset.id})`;
    pbtn.setAttribute("aria-pressed", "false");
    pbtn.addEventListener("click", () => {
      refs.onSelectModelPreset?.(preset.id);
    });
    nextBtns.push({ id: preset.id, btn: pbtn });
  }
  refs.modelPresetBtns = nextBtns;
  refs.renderedModelPresetsKey = key;
  refs.modelPillStrip.replaceChildren(
    ...nextBtns.map((item) => item.btn),
    refs.customModelToggleBtn,
  );
}

function renderSettings(refs: UiRefs, syncFormInputs: boolean, syncDegradedFeedback = true): void {
  const snap = state.snapshot;
  const cfg = snap?.config ?? DEFAULT_CONFIG;
  const gem = cfg.providers.gemini;

  // API key status tag & write-only placeholder
  if (gem.api_key_configured) {
    const hintText = gem.api_key_hint || "Set";
    const tag = el("span", "tag tag-ok mono");
    tag.title = hintText;
    tag.append(el("span", "sdot ok"), el("span", "tag-txt", hintText));
    refs.keyStatusWrap.replaceChildren(tag);
    refs.apiKeyInput.placeholder = "Paste new key to replace";
    refs.clearKeyBtn.hidden = false;
  } else {
    refs.keyStatusWrap.replaceChildren(el("span", "tag", "Not set"));
    refs.apiKeyInput.placeholder = "Paste Gemini API key";
    refs.clearKeyBtn.hidden = true;
    state.clearConfirmOpen = false;
    state.lastSaveVerified = false;
  }
  refs.clearConfirmRow.hidden = !(state.clearConfirmOpen && gem.api_key_configured);
  setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);

  // Default model pills
  const presets = activeModelPresets(snap);
  syncModelPresetPills(refs, presets);
  refs.modelCurrentVal.textContent = gem.default_model;
  refs.modelCurrentVal.title = gem.default_model;
  const isPreset = presets.some((p) => p.id === gem.default_model);
  const showCustomModel = state.customModelOpen || !isPreset;
  for (const item of refs.modelPresetBtns) {
    item.btn.setAttribute(
      "aria-pressed",
      String(!state.customModelOpen && gem.default_model === item.id),
    );
  }
  refs.customModelToggleBtn.setAttribute("aria-pressed", String(showCustomModel));
  refs.customModelRow.hidden = !showCustomModel;
  if (syncFormInputs) {
    refs.customModelInput.value = gem.default_model;
  }
  setInlineFeedback(refs.modelFeedbackLine, state.modelFeedback);

  // Dock auto-start card
  const li = snap?.launcher_integration ?? null;
  if (syncDegradedFeedback) {
    const isDegradedState4 =
      li !== null &&
      li.state === "disabled" &&
      li.message !== null &&
      li.message.trim().length > 0;
    if (isDegradedState4 && li.message) {
      state.launcherFeedbackFromSnapshot = true;
      state.dockFeedback = { tone: "warn", text: li.message };
    } else if (state.launcherFeedbackFromSnapshot) {
      state.launcherFeedbackFromSnapshot = false;
      state.dockFeedback = null;
    }
  }

  const dock = describeDockState(snap);
  refs.dockSwitch.checked = dock.checked;
  refs.dockTag.className = dock.tagClass;
  refs.dockTag.textContent = dock.tagText;
  refs.dockSub.textContent = dock.subText;

  if (li && li.shell && li.rc_path && li.state !== "manual_required") {
    refs.dockMeta.textContent = `${li.shell} \u00b7 ${li.rc_path}`;
    refs.dockMeta.hidden = false;
  } else {
    refs.dockMeta.textContent = "";
    refs.dockMeta.hidden = true;
  }

  if (li && li.state === "manual_required") {
    const rcLabel = li.rc_path || "your login-shell profile";
    refs.manualWrap.hidden = false;
    if (li.manual_snippet) {
      refs.manualLead.textContent = `Add this line to ${rcLabel}, then reopen Antigravity:`;
      refs.manualCode.textContent = li.manual_snippet;
      refs.manualCode.title = li.manual_snippet;
      refs.manualChip.hidden = false;
    } else {
      refs.manualLead.textContent =
        li.message || `Remove the managed agy-companion line from ${rcLabel}, then reopen Antigravity.`;
      refs.manualCode.textContent = "";
      refs.manualChip.hidden = true;
    }
  } else {
    refs.manualWrap.hidden = true;
  }
  setInlineFeedback(refs.dockFeedbackLine, state.dockFeedback);

  // CloudCode routing .pr-row radios
  if (syncFormInputs) {
    state.cloudCodeSelection = classifyCloudCodeMode(cfg.cloudcode.upstream);
    refs.cloudCodeCustomInput.value =
      state.cloudCodeSelection === "custom" ? cfg.cloudcode.upstream : "";
    refs.baseUrlInput.value = gem.base_url;
    refs.listenPortInput.value = String(cfg.proxy.port);
  }

  const resolvedShort =
    snap && snap.cloudcode.mode === "auto"
      ? shortUpstreamName(snap.cloudcode.resolved_upstream)
      : null;

  for (const row of refs.cloudCodeRows) {
    const isSel = state.cloudCodeSelection === row.id;
    row.radio.checked = isSel;
    row.row.classList.toggle("sel", isSel);
    if (row.resolvedTag) {
      if (resolvedShort) {
        const resolvedText = `Resolved: ${resolvedShort}`;
        row.resolvedTag.replaceChildren(el("span", "tag-txt", resolvedText));
        row.resolvedTag.title = resolvedText;
        row.resolvedTag.hidden = false;
      } else {
        row.resolvedTag.replaceChildren();
        row.resolvedTag.removeAttribute("title");
        row.resolvedTag.hidden = true;
      }
    }
  }
  refs.cloudCodeCustomRow.hidden = state.cloudCodeSelection !== "custom";
  setInlineFeedback(refs.advFeedbackLine, state.advFeedback);

  updateCliSnippets(refs);
  updateBusyControls(refs);
}

function renderMonitor(
  refs: UiRefs,
  onEditSettings: () => void,
  onToggleDock: (nextEnabled: boolean) => void,
): void {
  const snap = state.snapshot;
  const nodes: HTMLElement[] = [];

  // Card 1: Session cache telemetry
  const statsCard = el("section", "card");
  statsCard.setAttribute("aria-label", "Session cache");
  const sh = el("div", "sec-h");
  sh.append(el("span", "sec-t", "Session cache"));

  const right = el("span", "sec-right");
  if (!snap) {
    right.append(el("span", "tag", state.standalone ? "Preview" : "Loading"));
  } else if (!snap.proxy.reachable) {
    right.append(el("span", "tag tag-err", "Offline"));
  } else if (!snap.stats) {
    right.append(el("span", "tag", "Unavailable"));
  } else if (snap.stats.usage.requests > 0) {
    const liveTag = el("span", "tag tag-ok");
    liveTag.append(el("span", "sdot ok"), el("span", "tag-txt", "Live"));
    right.append(liveTag);
  } else {
    right.append(el("span", "tag", "Idle"));
  }
  sh.append(right);
  statsCard.append(sh);

  if (!snap || !snap.proxy.reachable || !snap.stats) {
    const noteText = !snap
      ? state.standalone
        ? "Open via Antigravity to view live session cache metrics."
        : "Connecting to local proxy\u2026"
      : !snap.proxy.reachable
        ? "Proxy is offline. Reopen Antigravity or start agy-companion serve."
        : "Session stats are currently unavailable. Click Refresh to retry.";
    statsCard.append(el("div", "quiet-note", noteText));
  } else {
    const st = snap.stats.usage;
    const hasReqs = st.requests > 0;
    const newIn = Math.max(0, st.prompt_tokens - st.cached_tokens);
    const totalOut = st.output_tokens + st.reasoning_tokens;
    const totalTok = st.cached_tokens + newIn + st.output_tokens + st.reasoning_tokens;
    const clampedPct = Math.max(0, Math.min(100, st.cache_hit_ratio * 100));

    const hero = el("div", "hero-row");
    const pctStr = hasReqs && st.prompt_tokens > 0 ? `${clampedPct.toFixed(1)}%` : "\u2014";
    hero.append(
      el("span", "hero-val num", pctStr),
      el(
        "span",
        "hero-sub num",
        hasReqs ? `cached of ${fmtCompact(st.prompt_tokens)} input` : "no requests yet",
      ),
    );
    statsCard.append(hero);

    const bar = el("div", "bar");
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", "Session token breakdown");
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", "100");
    bar.setAttribute("aria-valuenow", clampedPct.toFixed(1));
    if (totalTok > 0) {
      const segs = [
        { cls: "s-read", val: st.cached_tokens, label: "Cached" },
        { cls: "s-new", val: newIn, label: "New input" },
        { cls: "s-out", val: st.output_tokens, label: "Output" },
        { cls: "s-think", val: st.reasoning_tokens, label: "Reasoning" },
      ];
      for (const seg of segs) {
        if (seg.val <= 0) continue;
        const sp = el("span", seg.cls);
        sp.style.width = `${((seg.val / totalTok) * 100).toFixed(2)}%`;
        sp.title = `${seg.label}: ${fmtExact(seg.val)} tokens`;
        bar.append(sp);
      }
    }
    statsCard.append(bar);

    if (totalTok > 0) {
      const legend = el("div", "legend num");
      const items: Array<{ color: string; txt: string; raw: number }> = [
        { color: "var(--seg-read)", txt: `${fmtCompact(st.cached_tokens)} cached`, raw: st.cached_tokens },
        { color: "var(--seg-new)", txt: `${fmtCompact(newIn)} new`, raw: newIn },
        { color: "var(--seg-out)", txt: `${fmtCompact(st.output_tokens)} out`, raw: st.output_tokens },
      ];
      if (st.reasoning_tokens > 0) {
        items.push({
          color: "var(--seg-think)",
          txt: `${fmtCompact(st.reasoning_tokens)} thinking`,
          raw: st.reasoning_tokens,
        });
      }
      for (const item of items) {
        const sp = el("span");
        sp.title = `${fmtExact(item.raw)} tokens`;
        const sw = el("i");
        sw.style.background = item.color;
        sp.append(sw, document.createTextNode(item.txt));
        legend.append(sp);
      }
      statsCard.append(legend);
    }

    const tiles = el("div", "tiles");

    const tReqs = el("div", "tile");
    tReqs.title = `${fmtExact(st.requests)} proxied requests`;
    tReqs.append(
      el("div", "tile-l", "Requests"),
      el("div", "tile-v num", fmtCompact(st.requests)),
      el("div", "tile-s", hasReqs ? "this session" : "waiting"),
    );

    const tIn = el("div", "tile");
    tIn.title = `${fmtExact(st.prompt_tokens)} prompt tokens (${fmtExact(st.cached_tokens)} cached)`;
    tIn.append(
      el("div", "tile-l", "Input"),
      el("div", "tile-v num", fmtCompact(st.prompt_tokens)),
      el("div", "tile-s num", hasReqs ? `${fmtCompact(st.cached_tokens)} cached` : "\u2014"),
    );

    const tOut = el("div", "tile");
    tOut.title = `${fmtExact(st.output_tokens)} output + ${fmtExact(st.reasoning_tokens)} reasoning tokens`;
    tOut.append(
      el("div", "tile-l", "Output"),
      el("div", "tile-v num", fmtCompact(totalOut)),
      el(
        "div",
        "tile-s num",
        st.reasoning_tokens > 0
          ? `${fmtCompact(st.reasoning_tokens)} think`
          : hasReqs
            ? "text only"
            : "\u2014",
      ),
    );

    tiles.append(tReqs, tIn, tOut);
    statsCard.append(tiles);

    if (!hasReqs) {
      statsCard.append(
        el("div", "quiet-note", "No requests yet \u2014 select a (Proxy) model in Antigravity."),
      );
    }
  }
  nodes.push(statsCard);

  // Card 2 (narrow-only): Companion quick summary + 1-click Dock switch
  const cfg = snap?.config ?? DEFAULT_CONFIG;
  const compCard = el("section", "card narrow-only");
  compCard.setAttribute("aria-label", "Companion summary");
  const ch = el("div", "sec-h");
  ch.append(el("span", "sec-t", "Companion"));
  const editLnk = el("button", "lnk sec-right", "Edit");
  editLnk.type = "button";
  editLnk.addEventListener("click", onEditSettings);
  ch.append(editLnk);
  compCard.append(ch);

  const kv = el("div", "kv-list");

  const rKey = el("div", "kv-row");
  rKey.append(el("span", "kv-k", "API key"));
  const vKey = el("span", "kv-v");
  if (cfg.providers.gemini.api_key_configured) {
    const hintText = cfg.providers.gemini.api_key_hint || "Configured";
    vKey.title = hintText;
    vKey.append(el("span", "sdot ok"), el("span", "mono kv-trunc", hintText));
  } else {
    vKey.append(el("span", "sdot warn"), el("span", null, "Not set"));
  }
  rKey.append(vKey);

  const rModel = el("div", "kv-row");
  const vModel = el("span", "kv-v kv-text mono", cfg.providers.gemini.default_model);
  vModel.title = cfg.providers.gemini.default_model;
  rModel.append(el("span", "kv-k", "Default model"), vModel);

  const rRoute = el("div", "kv-row");
  const routeSummary = snap ? formatCloudCodeShort(snap) : "\u2014";
  const vRoute = el("span", "kv-v kv-text", routeSummary);
  vRoute.title = routeSummary;
  rRoute.append(el("span", "kv-k", "CloudCode"), vRoute);

  kv.append(rKey, rModel, rRoute);
  compCard.append(kv);

  const dock = describeDockState(snap);
  const dockWrap = el("div", "divider");
  const opt = el("div", "opt");
  const sw = el("label", "sw");
  const mSwitch = el("input");
  mSwitch.type = "checkbox";
  mSwitch.setAttribute("role", "switch");
  mSwitch.checked = dock.checked;
  mSwitch.setAttribute("aria-label", "Dock auto-start");
  mSwitch.addEventListener("change", () => {
    onToggleDock(mSwitch.checked);
  });
  sw.append(mSwitch, el("span", "trk"));
  opt.append(
    sw,
    el("span", "opt-title", "Dock auto-start"),
    el("span", dock.tagClass, dock.tagText),
    el("div", "opt-sub", dock.subText),
  );
  dockWrap.append(opt);
  compCard.append(dockWrap);
  refs.monitorDockSwitch = mSwitch;
  nodes.push(compCard);

  // Card 3 (wide-only): Connection diagnostics
  const connCard = el("section", "card wide-only");
  connCard.setAttribute("aria-label", "Connection");
  const cnh = el("div", "sec-h");
  cnh.append(el("span", "sec-t", "Connection"));
  connCard.append(cnh);

  const ckv = el("div", "kv-list");
  const cEnd = el("div", "kv-row");
  cEnd.append(
    el("span", "kv-k", "Endpoint"),
    el("span", "kv-v mono num", `${cfg.proxy.host}:${cfg.proxy.port}`),
  );
  const cRoute = el("div", "kv-row");
  const cRouteVal = el("span", "kv-v kv-text", routeSummary);
  cRouteVal.title = routeSummary;
  cRoute.append(el("span", "kv-k", "CloudCode"), cRouteVal);
  const cVer = el("div", "kv-row");
  cVer.append(
    el("span", "kv-k", "Proxy / CLI"),
    el(
      "span",
      "kv-v mono num",
      snap
        ? `${snap.proxy.version ? `v${snap.proxy.version}` : "offline"} / v${snap.binary_version}`
        : "\u2014",
    ),
  );
  const cPlat = el("div", "kv-row");
  cPlat.append(
    el("span", "kv-k", "Platform"),
    el("span", "kv-v", platformLabel(snap?.platform ?? detectClientPlatform())),
  );
  ckv.append(cEnd, cRoute, cVer, cPlat);
  connCard.append(ckv);
  nodes.push(connCard);

  refs.colMonitor.replaceChildren(...nodes);
}

function renderFooter(refs: UiRefs): void {
  const snapshot = state.snapshot;
  if (!snapshot) {
    refs.footerDot.className = "sdot";
    refs.footerRoute.textContent = state.standalone ? "Standalone" : "Connecting\u2026";
    return;
  }
  refs.endpointTag.textContent = `${snapshot.config.proxy.host}:${snapshot.config.proxy.port}`;
  refs.footerDot.className = !snapshot.proxy.reachable
    ? "sdot err"
    : snapshot.version_skew
      ? "sdot warn"
      : "sdot ok";
  refs.footerRoute.textContent = formatCloudCodeShort(snapshot);
}

function applyHostContext(refs: UiRefs, ctx: Partial<McpUiHostContext> | undefined): void {
  if (!ctx) return;
  if (ctx.theme) {
    applyDocumentTheme(ctx.theme);
    refs.root.setAttribute("data-theme", ctx.theme);
  }
  if (ctx.styles?.variables) {
    applyHostStyleVariables(ctx.styles.variables);
  }
}

/* =========================================================================
 * MAIN APPLICATION ENTRYPOINT
 * ========================================================================= */

async function main(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;

  const refs = mountUi(root);
  window.addEventListener("pagehide", stopCatalogCountdown);

  const app = new App({ name: "Antigravity Companion", version: "0.1.0" }, {});

  function openTab(next: ActiveTab): void {
    state.tab = next;
    state.tabPinnedByUser = true;
    renderHeader(refs);
    syncBannerAndCountdown();
  }

  function computePriorityBanner(): { banner: PriorityBannerSpec | null; needsTick: boolean } {
    const anyWrite = state.saving || state.clearing || state.updatingIntegration;
    const canCall = state.connected && state.activeCalls < 4;

    // Priority 1: Standalone preview or initial/refresh load error
    if (state.standalone) {
      return {
        banner: {
          key: "standalone",
          tone: "info",
          icon: "info",
          title: "Standalone preview",
          sub: "Open via Antigravity to read and save companion settings.",
        },
        needsTick: false,
      };
    }
    if (state.loadError) {
      return {
        banner: {
          key: `load-error:${state.loadError}`,
          tone: "err",
          icon: "err",
          title: "Failed to load settings",
          sub: state.loadError,
          action: {
            label: "Retry",
            requiresCall: true,
            disabled: !canCall || state.refreshing || anyWrite,
            onClick: () => {
              void refreshSettings(true);
            },
          },
        },
        needsTick: false,
      };
    }

    const snap = state.snapshot;
    if (!snap) {
      return { banner: null, needsTick: false };
    }

    // Priority 2: Port changed (evaluated before proxy offline since saving a new port probes it immediately)
    if (state.portChangedSinceSave) {
      return {
        banner: {
          key: `port-changed:${snap.config.proxy.port}:${snap.platform}`,
          tone: "warn",
          icon: "warn",
          title: "Restart Antigravity",
          sub: `Port updated to ${snap.config.proxy.port}. ${quitChord(snap.platform)} to apply.`,
          action: {
            label: "Refresh",
            requiresCall: true,
            disabled: !canCall || state.refreshing || anyWrite,
            onClick: () => {
              void refreshSettings(false);
            },
          },
        },
        needsTick: false,
      };
    }

    // Priority 3: Proxy offline
    if (!snap.proxy.reachable) {
      return {
        banner: {
          key: `proxy-offline:${snap.proxy.error ?? ""}`,
          tone: "err",
          icon: "err",
          title: "Proxy offline",
          sub: snap.proxy.error || "Unable to reach local proxy.",
          action: {
            label: "Retry",
            requiresCall: true,
            disabled: !canCall || state.refreshing || anyWrite,
            onClick: () => {
              void refreshSettings(false);
            },
          },
        },
        needsTick: false,
      };
    }

    // Priority 4: Version skew
    if (snap.version_skew) {
      return {
        banner: {
          key: `version-skew:${snap.proxy.version ?? ""}:${snap.binary_version}`,
          tone: "warn",
          icon: "warn",
          title: "Restart Antigravity",
          sub: `Proxy v${snap.proxy.version || "?"} differs from CLI v${snap.binary_version}.`,
        },
        needsTick: false,
      };
    }

    // Priority 5: API key needed (Day-0)
    if (!snap.config.providers.gemini.api_key_configured) {
      return {
        banner: {
          key: `api-key-needed:${state.tab}`,
          tone: "info",
          icon: "info",
          title: "Add a Gemini API key",
          sub: "Enable (Proxy) models in Antigravity.",
          action:
            state.tab === "monitor"
              ? {
                  label: "Open settings",
                  narrowOnly: true,
                  requiresCall: false,
                  onClick: () => openTab("settings"),
                }
              : undefined,
        },
        needsTick: false,
      };
    }

    // Priority 6: Dock restart needed
    const li = snap.launcher_integration;
    if (li && li.state === "enabled") {
      return {
        banner: {
          key: `dock-restart:${snap.platform}`,
          tone: "warn",
          icon: "warn",
          title: `${quitChord(snap.platform)} Antigravity`,
          sub: "Required once to activate Dock auto-start.",
        },
        needsTick: false,
      };
    }

    // Priority 7: Catalog refresh countdown
    if (
      snap.config.providers.gemini.api_key_configured &&
      snap.proxy.reachable &&
      snap.stats &&
      snap.stats.cloudcode.last_catalog_included_proxy_models === false
    ) {
      const prefix = state.lastSaveVerified ? "Key verified" : "Key saved";
      const lastMs = snap.stats.cloudcode.last_catalog_refresh_at_ms;
      if (lastMs === null) {
        return {
          banner: {
            key: `catalog-next:${prefix}:${snap.platform}`,
            tone: "info",
            icon: "info",
            title: `${prefix} \u2014 models appear on next refresh`,
            sub: `Or ${quitChord(snap.platform).toLowerCase()} Antigravity now.`,
          },
          needsTick: false,
        };
      }
      const remainingMs = Math.max(0, lastMs + CATALOG_REFRESH_INTERVAL_MS - Date.now());
      if (remainingMs > 0) {
        return {
          banner: {
            key: `catalog-countdown:${prefix}:${snap.platform}`,
            tone: "info",
            icon: "info",
            title: `${prefix} \u2014 models refresh in ~${fmtCountdownMs(remainingMs)}`,
            sub: `Or ${quitChord(snap.platform).toLowerCase()} Antigravity now.`,
          },
          needsTick: true,
        };
      }
      return {
        banner: {
          key: `catalog-due:${prefix}:${snap.platform}`,
          tone: "info",
          icon: "info",
          title: `${prefix} \u2014 model refresh due now`,
          sub: `Refresh to check, or ${quitChord(snap.platform).toLowerCase()} Antigravity.`,
          action: {
            label: "Refresh",
            requiresCall: true,
            disabled: !canCall || state.refreshing || anyWrite,
            onClick: () => {
              void refreshSettings(false);
            },
          },
        },
        needsTick: false,
      };
    }

    return { banner: null, needsTick: false };
  }

  function renderPriorityBanner(): boolean {
    const { banner, needsTick } = computePriorityBanner();
    if (!banner) {
      if (refs.activeBannerKey !== null || refs.banners.childNodes.length > 0) {
        refs.banners.replaceChildren();
      }
      refs.bannerActionBtn = null;
      refs.bannerActionRequiresCall = false;
      refs.activeBannerKey = null;
      refs.activeBannerTitleEl = null;
      return false;
    }

    if (
      needsTick &&
      refs.activeBannerKey === banner.key &&
      refs.activeBannerTitleEl &&
      refs.banners.contains(refs.activeBannerTitleEl)
    ) {
      const cardEl = refs.banners.firstElementChild;
      if (cardEl && cardEl.getAttribute("aria-live") !== "off") {
        cardEl.setAttribute("aria-live", "off");
      }
      if (refs.activeBannerTitleEl.textContent !== banner.title) {
        refs.activeBannerTitleEl.textContent = banner.title;
      }
      return true;
    }

    const card = el("div", `banner ${banner.tone}`);
    card.setAttribute("role", banner.tone === "err" ? "alert" : "status");
    const ic = el("span", "ic");
    ic.append(createIcon(banner.icon));
    const txt = el("div", "txt");
    const titleEl = el("b", null, banner.title);
    txt.append(titleEl);
    if (banner.sub) {
      txt.append(el("span", "sub", banner.sub));
    }
    card.append(ic, txt);

    refs.bannerActionBtn = null;
    refs.bannerActionRequiresCall = false;
    if (banner.action) {
      const act = el("div", banner.action.narrowOnly ? "b-act narrow-only" : "b-act");
      const btn = el("button", "btn btn-sm", banner.action.label);
      btn.type = "button";
      btn.disabled = Boolean(banner.action.disabled);
      btn.addEventListener("click", banner.action.onClick);
      act.append(btn);
      card.append(act);
      refs.bannerActionBtn = btn;
      refs.bannerActionRequiresCall = Boolean(banner.action.requiresCall);
    }

    refs.banners.replaceChildren(card);
    refs.activeBannerKey = banner.key;
    refs.activeBannerTitleEl = titleEl;
    return needsTick;
  }

  function syncBannerAndCountdown(): void {
    stopCatalogCountdown();
    const needsTick = renderPriorityBanner();
    if (!needsTick) return;
    state.catalogTimerId = window.setInterval(() => {
      if (!renderPriorityBanner()) {
        stopCatalogCountdown();
      }
    }, 1000);
  }

  function renderAll(syncFormInputs: boolean, syncDegradedFeedback = true): void {
    renderHeader(refs);
    renderMonitor(
      refs,
      () => openTab("settings"),
      (nextEnabled) => {
        void toggleLauncherIntegration(nextEnabled);
      },
    );
    renderSettings(refs, syncFormInputs, syncDegradedFeedback);
    renderFooter(refs);
    syncBannerAndCountdown();
  }

  function applySnapshot(snapshot: SettingsSnapshot, syncFormInputs: boolean): void {
    state.snapshot = snapshot;
    state.savedPort = snapshot.config.proxy.port;
    state.loadError = null;

    if (!state.initialTabApplied && !state.tabPinnedByUser) {
      state.tab = snapshot.config.providers.gemini.api_key_configured ? "monitor" : "settings";
      state.initialTabApplied = true;
    }

    renderAll(syncFormInputs);
  }

  async function callTool<T>(
    name: string,
    args: Record<string, unknown>,
    parse: (raw: unknown) => T | null,
  ): Promise<{ ok: true; data: T } | { ok: false; error: string }> {
    state.activeCalls += 1;
    updateBusyControls(refs);
    try {
      const result = (await app.callServerTool({
        name,
        arguments: args,
      })) as ToolCallResultLike;

      if (result.isError) {
        return { ok: false, error: extractToolErrorText(result) };
      }

      const payload = extractStructuredPayload(result);
      const parsed = parse(payload);
      if (parsed === null) {
        return { ok: false, error: `Unexpected response format from ${name}.` };
      }
      return { ok: true, data: parsed };
    } catch (err: unknown) {
      return { ok: false, error: formatCaughtError(err) };
    } finally {
      state.activeCalls = Math.max(0, state.activeCalls - 1);
      updateBusyControls(refs);
    }
  }

  async function refreshSettings(syncFormInputs: boolean): Promise<void> {
    if (
      !state.connected ||
      state.refreshing ||
      state.saving ||
      state.clearing ||
      state.updatingIntegration
    ) {
      return;
    }
    const epoch = state.snapshotEpoch;
    state.refreshing = true;
    updateBusyControls(refs);
    try {
      const outcome = await callTool("get_settings", {}, parseSettingsSnapshot);
      if (
        state.snapshotEpoch !== epoch ||
        state.saving ||
        state.clearing ||
        state.updatingIntegration
      ) {
        return;
      }
      if (!outcome.ok) {
        state.loadError = outcome.error;
        syncBannerAndCountdown();
        return;
      }
      state.loadError = null;
      state.dockFeedback = null;
      state.launcherFeedbackFromSnapshot = false;
      if (outcome.data.proxy.reachable) {
        state.portChangedSinceSave = false;
      }
      applySnapshot(outcome.data, syncFormInputs);
    } finally {
      state.refreshing = false;
      updateBusyControls(refs);
    }
  }

  async function savePartialSettings(
    args: SaveSettingsArgs,
    opts: {
      syncFormInputs: boolean;
      onSuccess: (snapshot: SettingsSnapshot) => Promise<void> | void;
      onError: (message: string) => void;
      isKeySave?: boolean;
    },
  ): Promise<void> {
    if (!state.connected || state.saving || state.clearing || state.updatingIntegration) {
      return;
    }
    state.snapshotEpoch += 1;
    state.saving = true;
    state.savingKey = Boolean(opts.isKeySave);
    updateBusyControls(refs);
    try {
      const outcome = await callTool(
        "save_settings",
        args as Record<string, unknown>,
        parseSettingsSnapshot,
      );
      if (!outcome.ok) {
        opts.onError(outcome.error);
        return;
      }
      applySnapshot(outcome.data, opts.syncFormInputs);
      await opts.onSuccess(outcome.data);
    } finally {
      state.saving = false;
      state.savingKey = false;
      updateBusyControls(refs);
    }
  }

  async function handleSaveKey(): Promise<void> {
    if (!state.connected || state.saving || state.clearing || state.updatingIntegration || state.testing) {
      return;
    }
    const raw = refs.apiKeyInput.value.trim();
    const validationError = validateApiKey(raw);
    if (validationError) {
      state.keyFeedback = { tone: "err", text: validationError };
      setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
      return;
    }

    state.keyFeedback = null;
    setInlineFeedback(refs.keyFeedbackLine, null);

    await savePartialSettings(
      { providers: { gemini: { api_key: { set: raw } } } },
      {
        syncFormInputs: false,
        isKeySave: true,
        onError: (msg) => {
          state.keyFeedback = { tone: "err", text: msg };
          setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
        },
        onSuccess: async (snapshot) => {
          refs.apiKeyInput.value = "";
          state.clearConfirmOpen = false;
          renderSettings(refs, false, false);

          if (!snapshot.config.providers.gemini.api_key_configured) {
            state.lastSaveVerified = false;
            return;
          }

          state.testing = true;
          state.keyFeedback = { tone: "info", text: "Saved \u00b7 Verifying key\u2026" };
          setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
          updateBusyControls(refs);
          try {
            const verify = await callTool("test_connection", {}, parseTestConnectionPayload);
            if (verify.ok && verify.data.ok) {
              state.lastSaveVerified = true;
              state.keyFeedback = { tone: "ok", text: "\u2713 Saved and verified" };
            } else {
              state.lastSaveVerified = false;
              const errMsg = verify.ok ? verify.data.message : verify.error;
              state.keyFeedback = { tone: "warn", text: `Saved, but check failed: ${errMsg}` };
            }
          } finally {
            state.testing = false;
            setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
            syncBannerAndCountdown();
            updateBusyControls(refs);
          }
        },
      },
    );
  }

  async function handleTestConnection(): Promise<void> {
    if (
      !state.connected ||
      state.testing ||
      state.saving ||
      state.clearing ||
      state.updatingIntegration
    ) {
      return;
    }
    if (
      !state.snapshot?.config.providers.gemini.api_key_configured ||
      refs.apiKeyInput.value.trim().length > 0
    ) {
      return;
    }

    state.testing = true;
    state.keyFeedback = { tone: "info", text: "Testing connection\u2026" };
    setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
    updateBusyControls(refs);
    try {
      const outcome = await callTool("test_connection", {}, parseTestConnectionPayload);
      if (!outcome.ok) {
        state.keyFeedback = { tone: "err", text: outcome.error };
        return;
      }
      if (outcome.data.ok) {
        state.lastSaveVerified = true;
        state.keyFeedback = {
          tone: "ok",
          text: "\u2713 Connected \u00b7 Gemini API reachable",
        };
        syncBannerAndCountdown();
      } else {
        state.lastSaveVerified = false;
        state.keyFeedback = { tone: "err", text: outcome.data.message };
      }
    } finally {
      state.testing = false;
      setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
      updateBusyControls(refs);
    }
  }

  async function handleClearKey(): Promise<void> {
    if (!state.connected || state.clearing || state.saving || state.updatingIntegration) {
      return;
    }
    state.snapshotEpoch += 1;
    state.clearing = true;
    state.keyFeedback = null;
    setInlineFeedback(refs.keyFeedbackLine, null);
    updateBusyControls(refs);
    try {
      const clearArgs: SaveSettingsArgs = {
        providers: { gemini: { api_key: "clear" } },
      };
      const outcome = await callTool(
        "save_settings",
        clearArgs as Record<string, unknown>,
        parseSettingsSnapshot,
      );
      if (!outcome.ok) {
        state.keyFeedback = { tone: "err", text: outcome.error };
        setInlineFeedback(refs.keyFeedbackLine, state.keyFeedback);
        return;
      }
      refs.apiKeyInput.value = "";
      state.clearConfirmOpen = false;
      state.lastSaveVerified = false;
      state.keyFeedback = { tone: "warn", text: "API key removed" };
      applySnapshot(outcome.data, false);
    } finally {
      state.clearing = false;
      updateBusyControls(refs);
    }
  }

  async function handleSaveDefaultModel(modelId: string, closeCustom: boolean): Promise<void> {
    const trimmed = modelId.trim();
    if (!trimmed) {
      state.modelFeedback = { tone: "err", text: "Default model cannot be empty." };
      setInlineFeedback(refs.modelFeedbackLine, state.modelFeedback);
      return;
    }
    if (!isSupportedGeminiModel(trimmed)) {
      state.modelFeedback = {
        tone: "err",
        text: "Only Gemini 3.8+ and Gemini 4+ models are supported.",
      };
      setInlineFeedback(refs.modelFeedbackLine, state.modelFeedback);
      return;
    }
    state.modelFeedback = null;
    setInlineFeedback(refs.modelFeedbackLine, null);

    await savePartialSettings(
      { providers: { gemini: { default_model: trimmed } } },
      {
        syncFormInputs: false,
        onError: (msg) => {
          state.modelFeedback = { tone: "err", text: msg };
          setInlineFeedback(refs.modelFeedbackLine, state.modelFeedback);
        },
        onSuccess: (snapshot) => {
          if (closeCustom) {
            state.customModelOpen = false;
          }
          refs.customModelInput.value = snapshot.config.providers.gemini.default_model;
          state.modelFeedback = {
            tone: "ok",
            text: `Saved \u00b7 ${snapshot.config.providers.gemini.default_model}`,
          };
          renderSettings(refs, false, false);
        },
      },
    );
  }

  async function toggleLauncherIntegration(nextEnabled: boolean): Promise<void> {
    if (!state.connected || state.saving || state.clearing || state.updatingIntegration) {
      renderAll(false, false);
      return;
    }
    const info = state.snapshot?.launcher_integration ?? null;
    if (!info || info.state === "unsupported") {
      renderAll(false, false);
      return;
    }

    state.snapshotEpoch += 1;
    state.updatingIntegration = true;
    state.launcherFeedbackFromSnapshot = false;
    state.dockFeedback = null;
    setInlineFeedback(refs.dockFeedbackLine, null);
    updateBusyControls(refs);
    try {
      const outcome = await callTool(
        "set_launcher_integration",
        { enabled: nextEnabled },
        parseSettingsSnapshot,
      );
      if (!outcome.ok) {
        state.dockFeedback = { tone: "err", text: outcome.error };
        setInlineFeedback(refs.dockFeedbackLine, state.dockFeedback);
        return;
      }

      const li = outcome.data.launcher_integration;
      switch (li?.state) {
        case "manual_required":
          state.dockFeedback = {
            tone: "warn",
            text:
              li.message ||
              (nextEnabled
                ? "Managed dotfile left untouched. Copy the line above to finish."
                : "Remove the managed agy-companion line from your dotfile manually."),
          };
          break;
        case "active":
          state.dockFeedback = { tone: "ok", text: "Dock auto-start active" };
          break;
        case "enabled":
          state.dockFeedback = {
            tone: "ok",
            text: `Enabled \u00b7 ${quitChord(outcome.data.platform)} Antigravity`,
          };
          break;
        case "unsupported":
          state.dockFeedback = {
            tone: "warn",
            text: li.message || "Dock auto-start is unavailable on this platform",
          };
          break;
        default:
          state.dockFeedback = { tone: "info", text: "Dock auto-start disabled" };
          break;
      }
      applySnapshot(outcome.data, false);
    } finally {
      state.updatingIntegration = false;
      renderAll(false, false);
      updateBusyControls(refs);
    }
  }

  async function handleSelectCloudCodePreset(
    modeId: CloudCodeModeId,
    urlValue: string,
    title: string,
  ): Promise<void> {
    if (!state.connected || state.saving || state.clearing || state.updatingIntegration) {
      renderSettings(refs, false, false);
      return;
    }
    state.cloudCodeSelection = modeId;
    if (modeId === "custom") {
      state.advFeedback = null;
      renderSettings(refs, false, false);
      refs.cloudCodeCustomInput.focus();
      return;
    }

    state.advFeedback = null;
    setInlineFeedback(refs.advFeedbackLine, null);
    await savePartialSettings(
      { cloudcode: { upstream: urlValue } },
      {
        syncFormInputs: false,
        onError: (msg) => {
          state.cloudCodeSelection = classifyCloudCodeMode(
            state.snapshot?.config.cloudcode.upstream ?? DEFAULT_CONFIG.cloudcode.upstream,
          );
          state.advFeedback = { tone: "err", text: msg };
          renderSettings(refs, false, false);
        },
        onSuccess: () => {
          state.advFeedback = { tone: "ok", text: `Saved \u00b7 ${title}` };
          renderSettings(refs, false, false);
        },
      },
    );
  }

  async function handleSaveCustomCloudCode(): Promise<void> {
    const raw = refs.cloudCodeCustomInput.value.trim();
    const urlError = validateEndpointUrl(raw, "CloudCode endpoint URL");
    if (urlError) {
      state.advFeedback = { tone: "err", text: urlError };
      setInlineFeedback(refs.advFeedbackLine, state.advFeedback);
      return;
    }
    state.advFeedback = null;
    setInlineFeedback(refs.advFeedbackLine, null);
    await savePartialSettings(
      { cloudcode: { upstream: raw } },
      {
        syncFormInputs: false,
        onError: (msg) => {
          state.advFeedback = { tone: "err", text: msg };
          setInlineFeedback(refs.advFeedbackLine, state.advFeedback);
        },
        onSuccess: (snapshot) => {
          state.cloudCodeSelection = classifyCloudCodeMode(snapshot.config.cloudcode.upstream);
          state.advFeedback = { tone: "ok", text: "Saved \u00b7 Custom CloudCode endpoint" };
          renderSettings(refs, false, false);
        },
      },
    );
  }

  async function handleSaveEndpoint(): Promise<void> {
    const baseUrl = refs.baseUrlInput.value.trim();
    const baseUrlErr = validateEndpointUrl(baseUrl, "Upstream base URL");
    if (baseUrlErr) {
      state.advFeedback = { tone: "err", text: baseUrlErr };
      setInlineFeedback(refs.advFeedbackLine, state.advFeedback);
      return;
    }

    const rawPort = refs.listenPortInput.value.trim();
    const port = Number.parseInt(rawPort, 10);
    if (!/^\d+$/.test(rawPort) || !Number.isInteger(port) || port < 1 || port > 65535) {
      state.advFeedback = { tone: "err", text: "Port must be between 1 and 65535." };
      setInlineFeedback(refs.advFeedbackLine, state.advFeedback);
      return;
    }

    const args: SaveSettingsArgs = {
      proxy: { port },
      providers: { gemini: { base_url: baseUrl } },
    };

    if (state.cloudCodeSelection === "custom") {
      const customUrl = refs.cloudCodeCustomInput.value.trim();
      if (customUrl.length > 0) {
        const ccErr = validateEndpointUrl(customUrl, "CloudCode endpoint URL");
        if (ccErr) {
          state.advFeedback = { tone: "err", text: ccErr };
          setInlineFeedback(refs.advFeedbackLine, state.advFeedback);
          return;
        }
        args.cloudcode = { upstream: customUrl };
      }
    }

    const previousPort = state.savedPort;
    state.advFeedback = null;
    setInlineFeedback(refs.advFeedbackLine, null);

    await savePartialSettings(args, {
      syncFormInputs: true,
      onError: (msg) => {
        state.advFeedback = { tone: "err", text: msg };
        setInlineFeedback(refs.advFeedbackLine, state.advFeedback);
      },
      onSuccess: (snapshot) => {
        const changed = snapshot.config.proxy.port !== previousPort;
        if (changed) {
          state.portChangedSinceSave = true;
        }
        state.advFeedback = {
          tone: "ok",
          text: changed
            ? "\u2713 Saved \u00b7 restart Antigravity to apply port"
            : "\u2713 Endpoint settings saved",
        };
        renderAll(false, false);
      },
    });
  }

  /* =======================================================================
   * REGISTER ALL MCP APP HANDLERS BEFORE CONNECTING
   * ======================================================================= */
  app.ontoolinput = () => {
    if (!state.hydratedFromInitialResult) {
      refs.footerRoute.textContent = "Loading\u2026";
    }
  };

  app.ontoolresult = (result) => {
    const typed = result as ToolCallResultLike;
    if (typed.isError) {
      state.loadError = extractToolErrorText(typed);
      syncBannerAndCountdown();
      return;
    }
    const payload = extractStructuredPayload(typed);
    const parsed = parseSettingsSnapshot(payload);
    if (parsed) {
      state.hydratedFromInitialResult = true;
      state.snapshotEpoch += 1;
      if (parsed.proxy.reachable) {
        state.portChangedSinceSave = false;
      }
      applySnapshot(parsed, true);
    }
  };

  app.onhostcontextchanged = (ctx) => {
    applyHostContext(refs, ctx);
  };

  /* =======================================================================
   * WIRE UI EVENT LISTENERS
   * ======================================================================= */
  refs.monitorTabBtn.addEventListener("click", () => openTab("monitor"));
  refs.settingsTabBtn.addEventListener("click", () => openTab("settings"));

  refs.refreshBtn.addEventListener("click", () => {
    void refreshSettings(false);
  });

  refs.apiKeyInput.addEventListener("input", () => {
    updateKeyButtons(refs);
  });
  refs.apiKeyInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      if (!refs.saveKeyBtn.disabled) {
        void handleSaveKey();
      }
    }
  });

  refs.saveKeyBtn.addEventListener("click", () => {
    void handleSaveKey();
  });

  refs.testBtn.addEventListener("click", () => {
    void handleTestConnection();
  });

  refs.clearKeyBtn.addEventListener("click", () => {
    state.clearConfirmOpen = !state.clearConfirmOpen;
    refs.clearConfirmRow.hidden = !state.clearConfirmOpen;
  });

  refs.cancelClearBtn.addEventListener("click", () => {
    state.clearConfirmOpen = false;
    refs.clearConfirmRow.hidden = true;
  });

  refs.confirmClearBtn.addEventListener("click", () => {
    void handleClearKey();
  });

  refs.onSelectModelPreset = (modelId: string) => {
    void handleSaveDefaultModel(modelId, true);
  };

  refs.customModelToggleBtn.addEventListener("click", () => {
    const cfgModel =
      state.snapshot?.config.providers.gemini.default_model ??
      DEFAULT_CONFIG.providers.gemini.default_model;
    const isPreset = activeModelPresets(state.snapshot).some((p) => p.id === cfgModel);
    if (!isPreset) {
      state.customModelOpen = true;
    } else {
      state.customModelOpen = !state.customModelOpen;
    }
    if (state.customModelOpen) {
      refs.customModelInput.value = cfgModel;
    }
    renderSettings(refs, false, false);
    if (state.customModelOpen) {
      refs.customModelInput.focus();
    }
  });

  refs.customModelApplyBtn.addEventListener("click", () => {
    void handleSaveDefaultModel(refs.customModelInput.value, false);
  });
  refs.customModelInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void handleSaveDefaultModel(refs.customModelInput.value, false);
    }
  });

  refs.dockSwitch.addEventListener("change", () => {
    void toggleLauncherIntegration(refs.dockSwitch.checked);
  });

  refs.manualCopyBtn.addEventListener("click", () => {
    void copyWithFallback(
      refs.manualCode.textContent ?? "",
      refs.manualCopyBtn,
      refs.manualCode,
    );
  });

  refs.recheckBtn.addEventListener("click", () => {
    void refreshSettings(false);
  });

  for (const row of refs.cloudCodeRows) {
    row.radio.addEventListener("change", () => {
      if (row.radio.checked) {
        void handleSelectCloudCodePreset(row.id, row.value, row.title);
      }
    });
    row.row.addEventListener("click", (event) => {
      const target = event.target as HTMLElement | null;
      if (
        row.radio.disabled ||
        !target ||
        target.closest(".pr-custom-row") ||
        target === row.radio ||
        target.tagName === "LABEL" ||
        state.cloudCodeSelection === row.id
      ) {
        return;
      }
      row.radio.checked = true;
      void handleSelectCloudCodePreset(row.id, row.value, row.title);
    });
  }

  refs.cloudCodeCustomApplyBtn.addEventListener("click", () => {
    void handleSaveCustomCloudCode();
  });
  refs.cloudCodeCustomInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void handleSaveCustomCloudCode();
    }
  });

  refs.listenPortInput.addEventListener("input", () => {
    updateCliSnippets(refs);
  });
  refs.baseUrlInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void handleSaveEndpoint();
    }
  });
  refs.listenPortInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void handleSaveEndpoint();
    }
  });

  refs.saveEndpointBtn.addEventListener("click", () => {
    void handleSaveEndpoint();
  });

  refs.launchCopyBtn.addEventListener("click", () => {
    void copyWithFallback(
      refs.launchCmdCode.textContent ?? "",
      refs.launchCopyBtn,
      refs.launchCmdCode,
    );
  });

  refs.cliJsonCopyBtn.addEventListener("click", () => {
    void copyWithFallback(
      refs.cliJsonPre.textContent ?? "",
      refs.cliJsonCopyBtn,
      refs.cliJsonPre,
    );
  });

  // Initial render before MCP handshake completes
  renderAll(true, false);

  /* =======================================================================
   * CONNECT TO MCP APP HOST (WITH STANDALONE PREVIEW FALLBACK)
   * ======================================================================= */
  try {
    await app.connect();
    state.connected = true;
    state.standalone = false;
    applyHostContext(refs, app.getHostContext());
    updateBusyControls(refs);

    window.setTimeout(() => {
      if (!state.hydratedFromInitialResult && !state.refreshing) {
        void refreshSettings(true);
      }
    }, 120);
  } catch {
    state.connected = false;
    state.standalone = true;
    renderAll(false, false);
  }
}

void main();
