import {
  App,
  applyDocumentTheme,
  applyHostStyleVariables,
  type McpUiHostContext,
} from "@modelcontextprotocol/ext-apps";
import "./style.css";

interface RedactedConfig {
  schema_version: number;
  listen_host: string;
  listen_port: number;
  gemini_base_url: string;
  gemini_default_model: string;
  gemini_timeout_secs: number;
  cloudcode_upstream_url: string;
  api_key_configured: boolean;
  api_key_hint: string | null;
}

interface ProxyHealthPayload {
  reachable: boolean;
  status: string | null;
  version: string | null;
  protocol_version: number | null;
  error: string | null;
}

interface ProxyStatsPayload {
  requests: number;
  prompt_tokens: number;
  cached_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_hit_ratio: number;
}

interface SettingsSnapshot {
  config: RedactedConfig;
  binary_version: string;
  proxy: ProxyHealthPayload;
  version_skew: boolean;
  stats: ProxyStatsPayload | null;
}

interface SaveSettingsArgs {
  api_key?: string;
  clear_api_key?: boolean;
  default_model?: string;
  base_url?: string;
  cloudcode_upstream_url?: string;
  listen_port?: number;
  listen_host?: string;
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

type BannerTone = "info" | "success" | "warning" | "danger";

const DEFAULT_CONFIG: RedactedConfig = {
  schema_version: 1,
  listen_host: "127.0.0.1",
  listen_port: 4141,
  gemini_base_url: "https://generativelanguage.googleapis.com",
  gemini_default_model: "gemini-4-hydrogen",
  gemini_timeout_secs: 900,
  cloudcode_upstream_url: "https://daily-cloudcode-pa.googleapis.com",
  api_key_configured: false,
  api_key_hint: null,
};

const QUICK_CLOUDCODE_UPSTREAMS = [
  {
    label: "Personal Google AI Pro",
    url: "https://daily-cloudcode-pa.googleapis.com",
  },
  {
    label: "Workspace / PayGo",
    url: "https://cloudcode-pa.googleapis.com",
  },
] as const;

const QUICK_MODELS = [
  "gemini-4-hydrogen",
  "gemini-4-argon",
  "gemini-4-neon",
  "gemini-4-carbon",
  "gemini-3.8-flash",
] as const;

function isSupportedGeminiModel(model: string): boolean {
  return /^gemini-(?:3|4)[-.].+/.test(model);
}

interface AppState {
  connected: boolean;
  hydratedFromInitialResult: boolean;
  snapshotEpoch: number;
  snapshot: SettingsSnapshot | null;
  savedPort: number;
  portChangedSinceSave: boolean;
  clearConfirmOpen: boolean;
  activeCalls: number;
  refreshing: boolean;
  saving: boolean;
  clearing: boolean;
  testing: boolean;
}

const state: AppState = {
  connected: false,
  hydratedFromInitialResult: false,
  snapshotEpoch: 0,
  snapshot: null,
  savedPort: DEFAULT_CONFIG.listen_port,
  portChangedSinceSave: false,
  clearConfirmOpen: false,
  activeCalls: 0,
  refreshing: false,
  saving: false,
  clearing: false,
  testing: false,
};

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

function parseRedactedConfig(raw: unknown): RedactedConfig | null {
  if (!isRecord(raw)) return null;
  const schemaVersion = asNumber(raw.schema_version);
  const listenHost = asString(raw.listen_host);
  const listenPort = asNumber(raw.listen_port);
  const baseUrl = asString(raw.gemini_base_url);
  const defaultModel = asString(raw.gemini_default_model);
  const timeoutSecs = asNumber(raw.gemini_timeout_secs);
  const cloudcodeUpstreamUrl =
    asString(raw.cloudcode_upstream_url) ?? DEFAULT_CONFIG.cloudcode_upstream_url;
  const apiKeyConfigured = asBoolean(raw.api_key_configured);
  const apiKeyHint = asNullableString(raw.api_key_hint);

  if (
    schemaVersion === null ||
    listenHost === null ||
    listenPort === null ||
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
    listen_host: listenHost,
    listen_port: listenPort,
    gemini_base_url: baseUrl,
    gemini_default_model: defaultModel,
    gemini_timeout_secs: timeoutSecs,
    cloudcode_upstream_url: cloudcodeUpstreamUrl,
    api_key_configured: apiKeyConfigured,
    api_key_hint: apiKeyHint,
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

function parseProxyStats(raw: unknown): ProxyStatsPayload | null {
  if (!isRecord(raw)) return null;
  const requests = asNumber(raw.requests);
  const promptTokens = asNumber(raw.prompt_tokens);
  const cachedTokens = asNumber(raw.cached_tokens);
  const outputTokens = asNumber(raw.output_tokens);
  const reasoningTokens = asNumber(raw.reasoning_tokens);
  const cacheHitRatio = asNumber(raw.cache_hit_ratio);

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

  return {
    requests,
    prompt_tokens: promptTokens,
    cached_tokens: cachedTokens,
    output_tokens: outputTokens,
    reasoning_tokens: reasoningTokens,
    cache_hit_ratio: cacheHitRatio,
  };
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

  let stats: ProxyStatsPayload | null = null;
  if (raw.stats !== null && raw.stats !== undefined) {
    stats = parseProxyStats(raw.stats);
    if (stats === null) {
      return null;
    }
  }

  return {
    config,
    binary_version: binaryVersion,
    proxy,
    version_skew: versionSkew,
    stats,
  };
}

function parseTestConnectionPayload(raw: unknown): TestConnectionPayload | null {
  if (!isRecord(raw)) return null;
  const ok = asBoolean(raw.ok);
  const message = asString(raw.message);
  if (ok === null || message === null) return null;
  return { ok, message };
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

function extractToolText(result: ToolCallResultLike): string {
  if (!Array.isArray(result.content)) return "";
  return result.content
    .filter((block) => block.type === "text" && typeof block.text === "string")
    .map((block) => block.text ?? "")
    .join("\n")
    .trim();
}

function extractToolErrorText(result: ToolCallResultLike): string {
  if (isRecord(result.structuredContent)) {
    const structuredError = asString(result.structuredContent.error);
    if (structuredError) return structuredError;
  }
  const text = extractToolText(result);
  return text || "The MCP tool reported an error.";
}

function formatRatioPercent(ratio: number): string {
  const clamped = Math.max(0, Math.min(1, ratio));
  return `${(clamped * 100).toFixed(1)}%`;
}

function buildHybridLaunchSnippet(port: number): string {
  const safePort = Number.isInteger(port) && port >= 1 && port <= 65535 ? port : DEFAULT_CONFIG.listen_port;
  return [
    "# Keep all original Google AI Pro models (with thinking levels)",
    "# AND add [Proxy] Gemini 4 / 3 models side-by-side:",
    `agy-companion launch --port ${safePort}`,
    "",
    "# Or launch Antigravity.app directly with env vars:",
    `CLOUD_CODE_URL="http://127.0.0.1:${safePort}" \\`,
    `GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:${safePort}" \\`,
    'GEMINI_API_KEY="agy-companion-local" \\',
    "/Applications/Antigravity.app/Contents/MacOS/Antigravity",
  ].join("\n");
}

function buildCustomModelsSnippet(port: number, model: string): string {
  const safePort = Number.isInteger(port) && port >= 1 && port <= 65535 ? port : DEFAULT_CONFIG.listen_port;
  const safeModel = model.trim() || DEFAULT_CONFIG.gemini_default_model;
  return JSON.stringify(
    {
      modelProvider: "gemini",
      customModelsConfig: {
        customModels: [
          {
            modelName: `gemini-api://http://127.0.0.1:${safePort}/models/${safeModel}`,
            label: `Antigravity Companion (${safeModel})`,
          },
        ],
      },
    },
    null,
    2,
  );
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  opts?: {
    className?: string;
    text?: string;
    id?: string;
    attrs?: Record<string, string>;
  },
  ...children: Array<Node | string>
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (opts?.className) node.className = opts.className;
  if (opts?.id) node.id = opts.id;
  if (opts?.text !== undefined) node.textContent = opts.text;
  if (opts?.attrs) {
    for (const [key, value] of Object.entries(opts.attrs)) {
      node.setAttribute(key, value);
    }
  }
  for (const child of children) {
    if (typeof child === "string") {
      node.appendChild(document.createTextNode(child));
    } else {
      node.appendChild(child);
    }
  }
  return node;
}

const SVG_NS = "http://www.w3.org/2000/svg";

function svgEl(
  tag: string,
  attrs?: Record<string, string>,
  ...children: SVGElement[]
): SVGElement {
  const node = document.createElementNS(SVG_NS, tag);
  if (attrs) {
    for (const [key, value] of Object.entries(attrs)) {
      node.setAttribute(key, value);
    }
  }
  for (const child of children) {
    node.appendChild(child);
  }
  return node;
}

function createBrandIcon(): SVGElement {
  const bgGrad = svgEl(
    "linearGradient",
    {
      id: "agy-companion-logo-bg-grad",
      x1: "12",
      y1: "10",
      x2: "116",
      y2: "118",
      gradientUnits: "userSpaceOnUse",
    },
    svgEl("stop", { offset: "0%", "stop-color": "#1A73E8" }),
    svgEl("stop", { offset: "52%", "stop-color": "#4F46E5" }),
    svgEl("stop", { offset: "100%", "stop-color": "#7C3AED" }),
  );
  const sparkGrad = svgEl(
    "linearGradient",
    {
      id: "agy-companion-logo-spark-grad",
      x1: "64",
      y1: "34",
      x2: "64",
      y2: "94",
      gradientUnits: "userSpaceOnUse",
    },
    svgEl("stop", { offset: "0%", "stop-color": "#FFFFFF" }),
    svgEl("stop", { offset: "100%", "stop-color": "#DBEAFE" }),
  );
  const orbitGrad = svgEl(
    "linearGradient",
    {
      id: "agy-companion-logo-orbit-grad",
      x1: "25",
      y1: "82",
      x2: "103",
      y2: "46",
      gradientUnits: "userSpaceOnUse",
    },
    svgEl("stop", { offset: "0%", "stop-color": "#93C5FD", "stop-opacity": "0.55" }),
    svgEl("stop", { offset: "50%", "stop-color": "#E0F2FE", "stop-opacity": "0.95" }),
    svgEl("stop", { offset: "100%", "stop-color": "#67E8F9", "stop-opacity": "0.9" }),
  );

  return svgEl(
    "svg",
    {
      class: "app-logo",
      width: "22",
      height: "22",
      viewBox: "0 0 128 128",
      fill: "none",
      "aria-hidden": "true",
      focusable: "false",
    },
    svgEl("defs", undefined, bgGrad, sparkGrad, orbitGrad),
    svgEl("rect", {
      x: "10",
      y: "10",
      width: "108",
      height: "108",
      rx: "28",
      fill: "url(#agy-companion-logo-bg-grad)",
    }),
    svgEl("rect", {
      x: "10.75",
      y: "10.75",
      width: "106.5",
      height: "106.5",
      rx: "27.25",
      stroke: "#FFFFFF",
      "stroke-opacity": "0.22",
      "stroke-width": "1.5",
    }),
    svgEl("ellipse", {
      cx: "64",
      cy: "64",
      rx: "39",
      ry: "16",
      transform: "rotate(-28 64 64)",
      stroke: "url(#agy-companion-logo-orbit-grad)",
      "stroke-width": "3.5",
    }),
    svgEl("path", {
      d: "M64 34 C64 52 76 64 94 64 C76 64 64 76 64 94 C64 76 52 64 34 64 C52 64 64 52 64 34 Z",
      fill: "url(#agy-companion-logo-spark-grad)",
    }),
    svgEl("circle", {
      cx: "98.4",
      cy: "45.7",
      r: "8.5",
      fill: "#67E8F9",
      "fill-opacity": "0.28",
    }),
    svgEl("circle", { cx: "98.4", cy: "45.7", r: "5", fill: "#FFFFFF" }),
    svgEl("circle", {
      cx: "29.6",
      cy: "82.3",
      r: "7",
      fill: "#93C5FD",
      "fill-opacity": "0.26",
    }),
    svgEl("circle", { cx: "29.6", cy: "82.3", r: "4", fill: "#E0F2FE" }),
  );
}

// DOM references populated during mount
interface UiRefs {
  globalBanner: HTMLDivElement;
  globalBannerTitle: HTMLDivElement;
  globalBannerBody: HTMLDivElement;

  proxyBadge: HTMLSpanElement;
  endpointTag: HTMLSpanElement;
  refreshBtn: HTMLButtonElement;
  proxyVersionVal: HTMLSpanElement;
  binaryVersionVal: HTMLSpanElement;
  cacheHitVal: HTMLSpanElement;
  skewBanner: HTMLDivElement;
  skewBody: HTMLDivElement;

  form: HTMLFormElement;
  keyBadge: HTMLSpanElement;
  apiKeyInput: HTMLInputElement;
  clearKeyBtn: HTMLButtonElement;
  clearConfirmRow: HTMLDivElement;
  confirmClearBtn: HTMLButtonElement;
  cancelClearBtn: HTMLButtonElement;
  modelInput: HTMLInputElement;
  chipButtons: HTMLButtonElement[];
  baseUrlInput: HTMLInputElement;
  cloudcodeUpstreamInput: HTMLInputElement;
  cloudcodeChipButtons: HTMLButtonElement[];
  listenPortInput: HTMLInputElement;
  portRestartNote: HTMLParagraphElement;
  saveBtn: HTMLButtonElement;
  testBtn: HTMLButtonElement;
  formFeedback: HTMLDivElement;

  hybridSnippetCode: HTMLElement;
  hybridCopyBtn: HTMLButtonElement;
  hybridCopyStatus: HTMLSpanElement;

  snippetCode: HTMLElement;
  copyBtn: HTMLButtonElement;
  copyStatus: HTMLSpanElement;

  statsBadge: HTMLSpanElement;
  statsBody: HTMLDivElement;
}

function mountUi(root: HTMLElement): UiRefs {
  // Global banner (standalone notice / initial tool error)
  const globalBannerTitle = el("div", { className: "banner-title" });
  const globalBannerBody = el("div");
  const globalBanner = el(
    "div",
    { className: "banner banner-info hidden", attrs: { role: "status", "aria-live": "polite" } },
    globalBannerTitle,
    globalBannerBody,
  );

  // Section 1: Proxy Status & Version Skew Bar
  const proxyBadge = el("span", { className: "badge", text: "Connecting..." });
  const endpointTag = el("span", {
    className: "endpoint-tag",
    text: `127.0.0.1:${DEFAULT_CONFIG.listen_port}`,
  });
  const refreshBtn = el("button", {
    className: "btn btn-sm",
    text: "Refresh",
    attrs: { type: "button" },
  });

  const proxyVersionVal = el("span", { className: "meta-value", text: "—" });
  const binaryVersionVal = el("span", { className: "meta-value", text: "—" });
  const cacheHitVal = el("span", { className: "meta-value", text: "—" });

  const metaStrip = el(
    "div",
    { className: "meta-strip" },
    el("span", { className: "meta-item" }, "Proxy: ", proxyVersionVal),
    el("span", { className: "meta-item" }, "Binary: ", binaryVersionVal),
    el("span", { className: "meta-item" }, "Cache Hit: ", cacheHitVal),
  );

  const skewBody = el("div");
  const skewBanner = el(
    "div",
    { className: "banner banner-warning hidden", attrs: { role: "alert" } },
    el("div", { className: "banner-title", text: "Version skew detected" }),
    skewBody,
  );

  const statusCard = el(
    "section",
    { className: "card", attrs: { "aria-label": "Proxy status" } },
    el(
      "div",
      { className: "card-header" },
      el(
        "div",
        { className: "title-group" },
        el(
          "div",
          { className: "brand-lockup" },
          createBrandIcon(),
          el("h1", { className: "app-title", text: "Antigravity Companion" }),
        ),
        proxyBadge,
        endpointTag,
      ),
      refreshBtn,
    ),
    metaStrip,
    skewBanner,
  );

  // Section 2: Gemini Settings Form
  const keyBadge = el("span", { className: "badge", text: "Not Configured" });
  const apiKeyInput = el("input", {
    id: "api-key",
    className: "text-input mono",
    attrs: {
      type: "password",
      autocomplete: "off",
      spellcheck: "false",
      autocapitalize: "off",
      placeholder: "Not configured — enter Gemini API key",
    },
  });

  const clearKeyBtn = el("button", {
    className: "btn btn-sm btn-danger hidden",
    text: "Clear API Key",
    attrs: { type: "button" },
  });
  const confirmClearBtn = el("button", {
    className: "btn btn-sm btn-danger",
    text: "Confirm Clear",
    attrs: { type: "button" },
  });
  const cancelClearBtn = el("button", {
    className: "btn btn-sm",
    text: "Cancel",
    attrs: { type: "button" },
  });
  const clearConfirmRow = el(
    "div",
    { className: "inline-confirm hidden", attrs: { role: "group", "aria-label": "Clear API key confirmation" } },
    el("span", { text: "Remove saved API key from local config?" }),
    el("div", { className: "action-row" }, confirmClearBtn, cancelClearBtn),
  );

  const apiKeyField = el(
    "div",
    { className: "field" },
    el(
      "div",
      { className: "field-label-row" },
      el("label", { className: "field-label", text: "Gemini API Key", attrs: { for: "api-key" } }),
      el("div", { className: "action-row" }, keyBadge, clearKeyBtn),
    ),
    apiKeyInput,
    clearConfirmRow,
    el("p", {
      className: "field-help",
      text: "Write-only field. Leave blank to keep the currently saved key; saved locally with 0600 permissions.",
    }),
  );

  const modelInput = el("input", {
    id: "default-model",
    className: "text-input mono",
    attrs: {
      type: "text",
      autocomplete: "off",
      spellcheck: "false",
      placeholder: DEFAULT_CONFIG.gemini_default_model,
    },
  });
  modelInput.value = DEFAULT_CONFIG.gemini_default_model;

  const chipButtons: HTMLButtonElement[] = QUICK_MODELS.map((modelName) =>
    el("button", {
      className: "chip",
      text: modelName,
      attrs: { type: "button", "data-model": modelName },
    }),
  );

  const modelField = el(
    "div",
    { className: "field" },
    el("label", { className: "field-label", text: "Default Model", attrs: { for: "default-model" } }),
    modelInput,
    el("div", { className: "chip-row" }, ...chipButtons),
  );

  const listenPortInput = el("input", {
    id: "listen-port",
    className: "text-input mono",
    attrs: {
      type: "number",
      min: "1",
      max: "65535",
      step: "1",
      inputmode: "numeric",
    },
  });
  listenPortInput.value = String(DEFAULT_CONFIG.listen_port);

  const portRestartNote = el("p", {
    className: "field-note hidden",
    text: "Changing the listen port requires restarting the proxy sidecar to bind the new port.",
  });

  const portField = el(
    "div",
    { className: "field" },
    el("label", { className: "field-label", text: "Listen Port", attrs: { for: "listen-port" } }),
    listenPortInput,
    portRestartNote,
  );

  const baseUrlInput = el("input", {
    id: "base-url",
    className: "text-input mono",
    attrs: {
      type: "url",
      autocomplete: "off",
      spellcheck: "false",
      placeholder: DEFAULT_CONFIG.gemini_base_url,
    },
  });
  baseUrlInput.value = DEFAULT_CONFIG.gemini_base_url;

  const baseUrlField = el(
    "div",
    { className: "field" },
    el("label", { className: "field-label", text: "Upstream Base URL", attrs: { for: "base-url" } }),
    baseUrlInput,
    el("p", {
      className: "field-help",
      text: "Remote endpoints must use https:// (http:// is allowed only for 127.0.0.1 or localhost).",
    }),
  );

  const cloudcodeUpstreamInput = el("input", {
    id: "cloudcode-upstream-input",
    className: "input",
    attrs: {
      type: "url",
      value: DEFAULT_CONFIG.cloudcode_upstream_url,
      placeholder: DEFAULT_CONFIG.cloudcode_upstream_url,
      spellcheck: "false",
    },
  });
  const cloudcodeChipButtons = QUICK_CLOUDCODE_UPSTREAMS.map((preset) =>
    el("button", {
      className: "chip",
      text: preset.label,
      attrs: { type: "button", "data-cloudcode-url": preset.url },
    }),
  );
  const cloudcodeField = el(
    "div",
    { className: "field" },
    el("label", {
      className: "field-label",
      text: "CloudCode Upstream URL (Hybrid Mode)",
      attrs: { for: "cloudcode-upstream-input" },
    }),
    cloudcodeUpstreamInput,
    el("div", { className: "chip-row" }, ...cloudcodeChipButtons),
    el("p", {
      className: "field-help",
      text: "Passes through original Antigravity OAuth calls while merging [Proxy] models with thinking levels. Use daily-cloudcode-pa for Personal Google AI Pro.",
    }),
  );

  const saveBtn = el("button", {
    className: "btn btn-primary",
    text: "Save Settings",
    attrs: { type: "submit" },
  });
  const testBtn = el("button", {
    className: "btn",
    text: "Test Connection",
    attrs: { type: "button" },
  });
  const formFeedback = el("div", {
    className: "banner hidden",
    attrs: { role: "status", "aria-live": "polite" },
  });

  const form = el(
    "form",
    { className: "settings-form", attrs: { novalidate: "true" } },
    apiKeyField,
    el("div", { className: "field-grid field-grid-two" }, modelField, portField),
    baseUrlField,
    cloudcodeField,
    el("div", { className: "action-row" }, saveBtn, testBtn),
    formFeedback,
  );

  const settingsCard = el(
    "section",
    { className: "card", attrs: { "aria-label": "Gemini settings" } },
    el(
      "div",
      { className: "card-header" },
      el("h2", { className: "section-title", text: "Gemini Settings" }),
    ),
    form,
  );

  // Section 3: Copyable Antigravity App Hybrid Mode Launch Snippet
  const hybridSnippetCode = el("code", {
    id: "hybrid-snippet",
    text: buildHybridLaunchSnippet(DEFAULT_CONFIG.listen_port),
  });
  const hybridCopyBtn = el("button", {
    className: "btn btn-sm",
    text: "Copy Command",
    attrs: { type: "button" },
  });
  const hybridCopyStatus = el("span", {
    className: "copy-status",
    attrs: { role: "status", "aria-live": "polite" },
  });

  const hybridCard = el(
    "section",
    { className: "card", attrs: { "aria-label": "Antigravity App hybrid launch snippet" } },
    el(
      "div",
      { className: "card-header" },
      el(
        "div",
        {},
        el("h2", { className: "section-title", text: "Antigravity App (Hybrid Mode)" }),
        el("p", {
          className: "section-subtitle",
          text: "Keeps all original Google AI Pro models + thinking levels and adds [Proxy] models side-by-side",
        }),
      ),
      el("div", { className: "action-row" }, hybridCopyStatus, hybridCopyBtn),
    ),
    el("pre", { className: "code-block" }, hybridSnippetCode),
    el("p", {
      className: "footer-note",
      text: "Original Google AI Pro models stay untouched. Selecting a [Proxy] model routes through this local proxy with your Gemini API key, implicit caching, and thinking levels.",
    }),
  );

  // Section 4: Copyable customModelsConfig JSON Snippet (agy CLI)
  const snippetCode = el("code", {
    id: "config-snippet",
    text: buildCustomModelsSnippet(DEFAULT_CONFIG.listen_port, DEFAULT_CONFIG.gemini_default_model),
  });
  const copyBtn = el("button", {
    className: "btn btn-sm",
    text: "Copy JSON",
    attrs: { type: "button" },
  });
  const copyStatus = el("span", {
    className: "copy-status",
    attrs: { role: "status", "aria-live": "polite" },
  });

  const snippetCard = el(
    "section",
    { className: "card", attrs: { "aria-label": "Antigravity model config snippet" } },
    el(
      "div",
      { className: "card-header" },
      el(
        "div",
        {},
        el("h2", { className: "section-title", text: "customModelsConfig Snippet" }),
        el("p", {
          className: "section-subtitle",
          text: "Paste into ~/.gemini/antigravity-cli/settings.json",
        }),
      ),
      el("div", { className: "action-row" }, copyStatus, copyBtn),
    ),
    el("pre", { className: "code-block" }, snippetCode),
  );

  // Section 5: Usage & Cost Telemetry
  const statsBadge = el("span", { className: "badge", text: "Proxy offline" });
  const statsBody = el("div");
  const statsCard = el(
    "section",
    { className: "card", attrs: { "aria-label": "Usage and cache telemetry" } },
    el(
      "div",
      { className: "card-header" },
      el("h2", { className: "section-title", text: "Usage & Cache Telemetry" }),
      statsBadge,
    ),
    statsBody,
    el("p", {
      className: "footer-note",
      text: "Rolling counters reflect the current proxy session. Per-request metadata is appended locally to JSONL; full historical usage & cost reports (agy-companion usage) are planned for a future milestone.",
    }),
  );

  root.replaceChildren(globalBanner, statusCard, settingsCard, hybridCard, snippetCard, statsCard);

  return {
    globalBanner,
    globalBannerTitle,
    globalBannerBody,
    proxyBadge,
    endpointTag,
    refreshBtn,
    proxyVersionVal,
    binaryVersionVal,
    cacheHitVal,
    skewBanner,
    skewBody,
    form,
    keyBadge,
    apiKeyInput,
    clearKeyBtn,
    clearConfirmRow,
    confirmClearBtn,
    cancelClearBtn,
    modelInput,
    chipButtons,
    baseUrlInput,
    cloudcodeUpstreamInput,
    cloudcodeChipButtons,
    listenPortInput,
    portRestartNote,
    saveBtn,
    testBtn,
    formFeedback,
    hybridSnippetCode,
    hybridCopyBtn,
    hybridCopyStatus,
    snippetCode,
    copyBtn,
    copyStatus,
    statsBadge,
    statsBody,
  };
}

function setBannerTone(node: HTMLElement, tone: BannerTone): void {
  node.className = `banner banner-${tone}`;
}

function showGlobalBanner(refs: UiRefs, tone: BannerTone, title: string, body: string): void {
  setBannerTone(refs.globalBanner, tone);
  refs.globalBannerTitle.textContent = title;
  refs.globalBannerBody.textContent = body;
}

function hideGlobalBanner(refs: UiRefs): void {
  refs.globalBanner.className = "banner hidden";
}

function showFormFeedback(refs: UiRefs, tone: BannerTone, message: string): void {
  setBannerTone(refs.formFeedback, tone);
  refs.formFeedback.textContent = message;
}

function clearFormFeedback(refs: UiRefs): void {
  refs.formFeedback.className = "banner hidden";
  refs.formFeedback.textContent = "";
}

function updateSnippet(refs: UiRefs): void {
  const parsedPort = Number.parseInt(refs.listenPortInput.value.trim(), 10);
  const port =
    Number.isInteger(parsedPort) && parsedPort >= 1 && parsedPort <= 65535
      ? parsedPort
      : state.savedPort;
  const model = refs.modelInput.value.trim() || DEFAULT_CONFIG.gemini_default_model;
  refs.hybridSnippetCode.textContent = buildHybridLaunchSnippet(port);
  refs.snippetCode.textContent = buildCustomModelsSnippet(port, model);

  const editedPortDiffers =
    Number.isInteger(parsedPort) && parsedPort >= 1 && parsedPort <= 65535 && parsedPort !== state.savedPort;
  refs.portRestartNote.classList.toggle("hidden", !(editedPortDiffers || state.portChangedSinceSave));
}

function updateBusyControls(refs: UiRefs): void {
  const anyWrite = state.saving || state.clearing;
  const canCall = state.connected && state.activeCalls < 4;

  refs.refreshBtn.disabled = !canCall || state.refreshing || anyWrite;
  refs.refreshBtn.textContent = state.refreshing ? "Refreshing..." : "Refresh";

  refs.saveBtn.disabled = !canCall || anyWrite;
  refs.saveBtn.textContent = state.saving ? "Saving..." : "Save Settings";

  refs.testBtn.disabled = !canCall || state.testing || anyWrite;
  refs.testBtn.textContent = state.testing ? "Testing..." : "Test Connection";

  refs.clearKeyBtn.disabled = !canCall || anyWrite;
  refs.confirmClearBtn.disabled = !canCall || anyWrite;
  refs.cancelClearBtn.disabled = anyWrite;
}

function renderStats(refs: UiRefs, snapshot: SettingsSnapshot | null): void {
  const host = snapshot?.config.listen_host ?? DEFAULT_CONFIG.listen_host;
  const port = snapshot?.config.listen_port ?? DEFAULT_CONFIG.listen_port;
  if (!snapshot || !snapshot.proxy.reachable || snapshot.stats === null) {
    refs.statsBadge.className = "badge";
    refs.statsBadge.textContent = "Proxy offline";

    const detail = snapshot?.proxy.error ? ` (${snapshot.proxy.error})` : "";
    const emptyPanel = el("div", {
      className: "empty-state",
      text: `Proxy is currently unreachable at ${host}:${port}${detail}. Start agy-companion serve or enable the proxy sidecar to see live session token and implicit-cache metrics.`,
    });
    refs.statsBody.replaceChildren(emptyPanel);
    return;
  }

  const stats = snapshot.stats;
  refs.statsBadge.className = "badge badge-success";
  refs.statsBadge.textContent = "Live session";

  const pct = Math.max(0, Math.min(100, stats.cache_hit_ratio * 100));
  const progressFill = el("div", { className: "progress-fill" });
  progressFill.style.width = `${pct.toFixed(1)}%`;

  const progressTrack = el(
    "div",
    {
      className: "progress-track",
      attrs: {
        role: "progressbar",
        "aria-label": "Implicit cache hit ratio",
        "aria-valuemin": "0",
        "aria-valuemax": "100",
        "aria-valuenow": pct.toFixed(1),
      },
    },
    progressFill,
  );

  const metric = (label: string, value: string, extra?: Node): HTMLDivElement => {
    const card = el(
      "div",
      { className: "stat-card" },
      el("dt", { className: "stat-label", text: label }),
      el("dd", { className: "stat-value", text: value }),
    );
    if (extra) card.appendChild(extra);
    return card;
  };

  const grid = el(
    "dl",
    { className: "stats-grid" },
    metric("Requests", stats.requests.toLocaleString()),
    metric("Prompt Tokens", stats.prompt_tokens.toLocaleString()),
    metric("Cached Tokens", stats.cached_tokens.toLocaleString()),
    metric("Output Tokens", stats.output_tokens.toLocaleString()),
    metric("Reasoning Tokens", stats.reasoning_tokens.toLocaleString()),
    metric("Cache Hit Ratio", formatRatioPercent(stats.cache_hit_ratio), progressTrack),
  );

  refs.statsBody.replaceChildren(grid);
}

function renderSnapshot(refs: UiRefs, snapshot: SettingsSnapshot, syncFormInputs: boolean): void {
  state.snapshot = snapshot;
  state.savedPort = snapshot.config.listen_port;

  // Section 1: Proxy Status & Version Skew Bar
  refs.endpointTag.textContent = `${snapshot.config.listen_host}:${snapshot.config.listen_port}`;
  refs.binaryVersionVal.textContent = `v${snapshot.binary_version}`;

  if (snapshot.proxy.reachable) {
    refs.proxyBadge.className = "badge badge-success";
    refs.proxyBadge.textContent = "Reachable";
    refs.proxyVersionVal.textContent = snapshot.proxy.version ? `v${snapshot.proxy.version}` : "unknown";
    refs.cacheHitVal.textContent = snapshot.stats ? formatRatioPercent(snapshot.stats.cache_hit_ratio) : "—";
  } else {
    refs.proxyBadge.className = "badge badge-danger";
    refs.proxyBadge.textContent = "Unreachable";
    refs.proxyVersionVal.textContent = "—";
    refs.cacheHitVal.textContent = "—";
  }

  if (snapshot.version_skew) {
    const runningVer = snapshot.proxy.version ?? "unknown";
    refs.skewBody.textContent =
      `Running proxy is v${runningVer}, while the installed companion binary is v${snapshot.binary_version}. Restart the Antigravity Companion proxy sidecar so the running proxy matches the installed binary.`;
    refs.skewBanner.classList.remove("hidden");
  } else {
    refs.skewBanner.classList.add("hidden");
  }

  // Section 2: API Key badge & write-only placeholder (never populate apiKeyInput.value)
  if (snapshot.config.api_key_configured) {
    refs.keyBadge.className = "badge badge-success";
    refs.keyBadge.textContent = snapshot.config.api_key_hint
      ? `Configured · ${snapshot.config.api_key_hint}`
      : "Configured";
    refs.apiKeyInput.placeholder = snapshot.config.api_key_hint
      ? `Enter new key to replace (${snapshot.config.api_key_hint})`
      : "Enter new key to replace saved key";
    refs.clearKeyBtn.classList.remove("hidden");
  } else {
    refs.keyBadge.className = "badge";
    refs.keyBadge.textContent = "Not Configured";
    refs.apiKeyInput.placeholder = "Not configured — enter Gemini API key";
    refs.clearKeyBtn.classList.add("hidden");
    state.clearConfirmOpen = false;
    refs.clearConfirmRow.classList.add("hidden");
  }

  if (syncFormInputs) {
    refs.modelInput.value = snapshot.config.gemini_default_model;
    refs.baseUrlInput.value = snapshot.config.gemini_base_url;
    refs.cloudcodeUpstreamInput.value = snapshot.config.cloudcode_upstream_url;
    refs.listenPortInput.value = String(snapshot.config.listen_port);
  }

  updateSnippet(refs);
  renderStats(refs, snapshot);
}

function applyHostContext(ctx: Partial<McpUiHostContext> | undefined): void {
  if (!ctx) return;
  if (ctx.theme) {
    applyDocumentTheme(ctx.theme);
  }
  if (ctx.styles?.variables) {
    applyHostStyleVariables(ctx.styles.variables);
  }
}

async function copySnippetWithFallback(
  codeEl: HTMLElement,
  statusEl: HTMLSpanElement,
  snippet: string,
): Promise<void> {
  let copied = false;

  // Tier 1: Clipboard API
  if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
    try {
      await navigator.clipboard.writeText(snippet);
      copied = true;
    } catch {
      copied = false;
    }
  }

  // Tier 2: Ephemeral off-screen textarea + execCommand
  if (!copied) {
    const textarea = document.createElement("textarea");
    textarea.value = snippet;
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

  if (copied) {
    statusEl.textContent = "Copied!";
    window.setTimeout(() => {
      if (statusEl.textContent === "Copied!") {
        statusEl.textContent = "";
      }
    }, 2500);
    return;
  }

  // Tier 3: Select snippet range & show inline shortcut hint
  const selection = window.getSelection();
  if (selection) {
    const range = document.createRange();
    range.selectNodeContents(codeEl);
    selection.removeAllRanges();
    selection.addRange(range);
  }
  statusEl.textContent = "Select & press Cmd/Ctrl+C to copy";
}

function formatCaughtError(err: unknown): string {
  if (err instanceof Error && err.message.trim().length > 0) {
    return err.message;
  }
  return "Request failed. Check the companion MCP server connection.";
}

async function main(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;

  const refs = mountUi(root);
  renderStats(refs, null);
  updateBusyControls(refs);

  const app = new App({ name: "Antigravity Companion", version: "0.1.0" }, {});

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
    if (!state.connected || state.refreshing || state.saving || state.clearing) return;
    const epoch = state.snapshotEpoch;
    state.refreshing = true;
    updateBusyControls(refs);
    try {
      const outcome = await callTool("get_settings", {}, parseSettingsSnapshot);
      if (state.snapshotEpoch !== epoch || state.saving || state.clearing) {
        return;
      }
      if (!outcome.ok) {
        showGlobalBanner(refs, "danger", "Failed to load settings", outcome.error);
        return;
      }
      hideGlobalBanner(refs);
      if (outcome.data.proxy.reachable) {
        state.portChangedSinceSave = false;
      }
      renderSnapshot(refs, outcome.data, syncFormInputs);
    } finally {
      state.refreshing = false;
      updateBusyControls(refs);
    }
  }

  // Register ALL handlers BEFORE app.connect()
  app.ontoolinput = () => {
    if (!state.hydratedFromInitialResult) {
      refs.proxyBadge.className = "badge";
      refs.proxyBadge.textContent = "Loading...";
    }
  };

  app.ontoolresult = (result) => {
    const typed = result as ToolCallResultLike;
    if (typed.isError) {
      showGlobalBanner(refs, "danger", "Settings load error", extractToolErrorText(typed));
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
      hideGlobalBanner(refs);
      renderSnapshot(refs, parsed, true);
    }
  };

  app.onhostcontextchanged = (ctx) => {
    applyHostContext(ctx);
  };

  // Wire UI interactions
  refs.refreshBtn.addEventListener("click", () => {
    void refreshSettings(false);
  });

  refs.modelInput.addEventListener("input", () => {
    updateSnippet(refs);
  });

  refs.listenPortInput.addEventListener("input", () => {
    updateSnippet(refs);
  });

  for (const chip of refs.chipButtons) {
    chip.addEventListener("click", () => {
      const chosen = chip.getAttribute("data-model");
      if (chosen) {
        refs.modelInput.value = chosen;
        updateSnippet(refs);
      }
    });
  }

  for (const chip of refs.cloudcodeChipButtons) {
    chip.addEventListener("click", () => {
      const chosen = chip.getAttribute("data-cloudcode-url");
      if (chosen) {
        refs.cloudcodeUpstreamInput.value = chosen;
      }
    });
  }

  refs.clearKeyBtn.addEventListener("click", () => {
    state.clearConfirmOpen = !state.clearConfirmOpen;
    refs.clearConfirmRow.classList.toggle("hidden", !state.clearConfirmOpen);
  });

  refs.cancelClearBtn.addEventListener("click", () => {
    state.clearConfirmOpen = false;
    refs.clearConfirmRow.classList.add("hidden");
  });

  refs.confirmClearBtn.addEventListener("click", async () => {
    if (!state.connected || state.clearing || state.saving) return;
    state.snapshotEpoch += 1;
    state.clearing = true;
    clearFormFeedback(refs);
    updateBusyControls(refs);
    try {
      const outcome = await callTool(
        "save_settings",
        { clear_api_key: true },
        parseSettingsSnapshot,
      );
      if (!outcome.ok) {
        showFormFeedback(refs, "danger", outcome.error);
        return;
      }
      refs.apiKeyInput.value = "";
      state.clearConfirmOpen = false;
      refs.clearConfirmRow.classList.add("hidden");
      renderSnapshot(refs, outcome.data, false);
      showFormFeedback(refs, "success", "Saved API key cleared (config written at 0600).");
    } finally {
      state.clearing = false;
      updateBusyControls(refs);
    }
  });

  refs.form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!state.connected || state.saving || state.clearing) return;

    const rawPort = refs.listenPortInput.value.trim();
    const port = Number.parseInt(rawPort, 10);
    if (!/^\d+$/.test(rawPort) || !Number.isInteger(port) || port < 1 || port > 65535) {
      showFormFeedback(refs, "danger", "Listen port must be an integer between 1 and 65535.");
      return;
    }

    const defaultModel = refs.modelInput.value.trim();
    if (!defaultModel) {
      showFormFeedback(refs, "danger", "Default model cannot be empty.");
      return;
    }
    if (!isSupportedGeminiModel(defaultModel)) {
      showFormFeedback(
        refs,
        "danger",
        "Only Gemini 3 and Gemini 4 models are supported (e.g. gemini-4-hydrogen, gemini-4-argon, gemini-3.8-flash).",
      );
      return;
    }

    const baseUrl = refs.baseUrlInput.value.trim();
    if (!baseUrl) {
      showFormFeedback(refs, "danger", "Upstream base URL cannot be empty.");
      return;
    }

    const cloudcodeUpstreamUrl = refs.cloudcodeUpstreamInput.value.trim();
    if (!cloudcodeUpstreamUrl) {
      showFormFeedback(refs, "danger", "CloudCode upstream URL cannot be empty.");
      return;
    }

    const args: SaveSettingsArgs = {
      default_model: defaultModel,
      base_url: baseUrl,
      cloudcode_upstream_url: cloudcodeUpstreamUrl,
      listen_port: port,
    };

    if (refs.apiKeyInput.value.length > 0) {
      args.api_key = refs.apiKeyInput.value;
    }

    const previousPort = state.savedPort;
    state.snapshotEpoch += 1;
    state.saving = true;
    clearFormFeedback(refs);
    updateBusyControls(refs);
    try {
      const outcome = await callTool(
        "save_settings",
        args as Record<string, unknown>,
        parseSettingsSnapshot,
      );
      if (!outcome.ok) {
        showFormFeedback(refs, "danger", outcome.error);
        return;
      }

      // Immediately clear write-only API key input on success
      refs.apiKeyInput.value = "";
      state.clearConfirmOpen = false;
      refs.clearConfirmRow.classList.add("hidden");
      state.portChangedSinceSave = outcome.data.config.listen_port !== previousPort;

      renderSnapshot(refs, outcome.data, true);
      const portMsg = state.portChangedSinceSave
        ? " Restart the proxy sidecar to listen on the new port."
        : "";
      showFormFeedback(refs, "success", `Settings saved (0600).${portMsg}`);
    } finally {
      state.saving = false;
      updateBusyControls(refs);
    }
  });

  refs.testBtn.addEventListener("click", async () => {
    if (!state.connected || state.testing || state.saving || state.clearing) return;
    state.testing = true;
    clearFormFeedback(refs);
    updateBusyControls(refs);
    try {
      const outcome = await callTool("test_connection", {}, parseTestConnectionPayload);
      if (!outcome.ok) {
        showFormFeedback(refs, "danger", outcome.error);
        return;
      }
      const unsavedKeyTip =
        refs.apiKeyInput.value.length > 0
          ? " Tip: save settings first to test the newly entered key."
          : "";
      if (outcome.data.ok) {
        showFormFeedback(refs, "success", `Connection OK: ${outcome.data.message}${unsavedKeyTip}`);
      } else {
        showFormFeedback(refs, "danger", `Connection failed: ${outcome.data.message}${unsavedKeyTip}`);
      }
    } finally {
      state.testing = false;
      updateBusyControls(refs);
    }
  });

  refs.hybridCopyBtn.addEventListener("click", () => {
    void copySnippetWithFallback(
      refs.hybridSnippetCode,
      refs.hybridCopyStatus,
      refs.hybridSnippetCode.textContent ?? "",
    );
  });

  refs.copyBtn.addEventListener("click", () => {
    void copySnippetWithFallback(refs.snippetCode, refs.copyStatus, refs.snippetCode.textContent ?? "");
  });

  // Connect to host with graceful standalone fallback
  try {
    await app.connect();
    state.connected = true;
    applyHostContext(app.getHostContext());
    updateBusyControls(refs);

    window.setTimeout(() => {
      if (!state.hydratedFromInitialResult && !state.refreshing) {
        void refreshSettings(true);
      }
    }, 120);
  } catch {
    state.connected = false;
    refs.proxyBadge.className = "badge";
    refs.proxyBadge.textContent = "Standalone";
    showGlobalBanner(
      refs,
      "info",
      "Standalone preview mode",
      "Open this panel through Antigravity (open_settings) to read and persist local companion settings.",
    );
    updateBusyControls(refs);
  }
}

void main();
