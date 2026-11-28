import {
  CURATED_CODEX_MODEL_CATALOG,
  CURATED_MODEL_CATALOG,
  type ModelPreset,
  type ModelSessionStats,
  type UsageBucket,
  type UsageSummary,
} from "./types.ts";

export interface ModelListPrice {
  inputPerM: number;
  cachedPerM: number;
  outputPerM: number;
  contextWindow: number;
}

export interface TokenUsageInput {
  inputTokens: number;
  cachedTokens: number;
  outputTokens: number;
  reasoningTokens?: number;
}

export interface CostBreakdown {
  totalUsd: number;
  cachedReadUsd: number;
  newInputUsd: number;
  outputUsd: number;
  cacheSavingsUsd: number;
}

export interface SessionCostSummary {
  hasPricedModels: boolean;
  totalUsd: number;
  cachedReadUsd: number;
  newInputUsd: number;
  outputUsd: number;
  cacheSavingsUsd: number;
  byModelKey: Map<string, CostBreakdown | null>;
}

export interface UsageCostSummary {
  hasPricedModels: boolean;
  totalUsd: number;
  cachedReadUsd: number;
  newInputUsd: number;
  outputUsd: number;
  cacheSavingsUsd: number;
  byModelKey: Map<string, CostBreakdown | null>;
  byProviderKey: Map<string, number | null>;
  bucketCosts: Array<CostBreakdown | null>;
}

/**
 * Official list prices (USD per 1M tokens) for Gemini Developer API, Vertex Gemini,
 * Vertex Claude, and Vertex OpenAI models. All UI figures are prefixed with
 * approximately and labeled "est. list price".
 */
export const MODEL_LIST_PRICES: Readonly<Record<string, ModelListPrice>> = {
  // Gemini Developer API & Vertex Gemini (1M context window)
  "gemini-4-carbon": {
    inputPerM: 4.0,
    cachedPerM: 0.2,
    outputPerM: 20.0,
    contextWindow: 1_000_000,
  },
  "gemini-4-neon": {
    inputPerM: 3.0,
    cachedPerM: 0.15,
    outputPerM: 15.0,
    contextWindow: 1_000_000,
  },
  "gemini-4-hydrogen": {
    inputPerM: 1.5,
    cachedPerM: 0.075,
    outputPerM: 7.5,
    contextWindow: 1_000_000,
  },
  "gemini-4-argon": {
    inputPerM: 4.0,
    cachedPerM: 0.2,
    outputPerM: 20.0,
    contextWindow: 1_000_000,
  },
  "gemini-3.8-pro": {
    inputPerM: 2.0,
    cachedPerM: 0.2,
    outputPerM: 12.0,
    contextWindow: 1_000_000,
  },
  "gemini-3.8-flash": {
    inputPerM: 1.5,
    cachedPerM: 0.15,
    outputPerM: 7.5,
    contextWindow: 1_000_000,
  },
  "gemini-3.1-pro": {
    inputPerM: 2.0,
    cachedPerM: 0.2,
    outputPerM: 12.0,
    contextWindow: 1_000_000,
  },
  "gemini-3.1-flash": {
    inputPerM: 0.5,
    cachedPerM: 0.05,
    outputPerM: 3.0,
    contextWindow: 1_000_000,
  },
  "gemini-2.5-pro": {
    inputPerM: 1.25,
    cachedPerM: 0.125,
    outputPerM: 10.0,
    contextWindow: 1_000_000,
  },
  "gemini-2.5-flash": {
    inputPerM: 0.3,
    cachedPerM: 0.03,
    outputPerM: 2.5,
    contextWindow: 1_000_000,
  },

  // Vertex Claude (200K context window)
  "claude-opus-4-6": {
    inputPerM: 5.0,
    cachedPerM: 0.5,
    outputPerM: 25.0,
    contextWindow: 200_000,
  },
  "claude-opus-4-5": {
    inputPerM: 5.0,
    cachedPerM: 0.5,
    outputPerM: 25.0,
    contextWindow: 200_000,
  },
  "claude-opus-4": {
    inputPerM: 15.0,
    cachedPerM: 1.5,
    outputPerM: 75.0,
    contextWindow: 200_000,
  },
  "claude-sonnet-4-6": {
    inputPerM: 3.0,
    cachedPerM: 0.3,
    outputPerM: 15.0,
    contextWindow: 200_000,
  },
  "claude-sonnet-4-5": {
    inputPerM: 3.0,
    cachedPerM: 0.3,
    outputPerM: 15.0,
    contextWindow: 200_000,
  },
  "claude-sonnet-4": {
    inputPerM: 3.0,
    cachedPerM: 0.3,
    outputPerM: 15.0,
    contextWindow: 200_000,
  },
  "claude-haiku-4-5": {
    inputPerM: 1.0,
    cachedPerM: 0.1,
    outputPerM: 5.0,
    contextWindow: 200_000,
  },
  "claude-3-5-haiku": {
    inputPerM: 0.8,
    cachedPerM: 0.08,
    outputPerM: 4.0,
    contextWindow: 200_000,
  },

  // Vertex OpenAI
  "gpt-5.4": {
    inputPerM: 2.5,
    cachedPerM: 0.25,
    outputPerM: 15.0,
    contextWindow: 400_000,
  },
  "gpt-5.3-codex": {
    inputPerM: 1.75,
    cachedPerM: 0.175,
    outputPerM: 14.0,
    contextWindow: 400_000,
  },
  "gpt-5": {
    inputPerM: 1.25,
    cachedPerM: 0.125,
    outputPerM: 10.0,
    contextWindow: 400_000,
  },
  "gpt-4.1": {
    inputPerM: 2.0,
    cachedPerM: 0.5,
    outputPerM: 8.0,
    contextWindow: 1_000_000,
  },
  "gpt-4o": {
    inputPerM: 2.5,
    cachedPerM: 1.25,
    outputPerM: 10.0,
    contextWindow: 128_000,
  },
  o3: {
    inputPerM: 2.0,
    cachedPerM: 0.5,
    outputPerM: 8.0,
    contextWindow: 200_000,
  },
  "o4-mini": {
    inputPerM: 1.1,
    cachedPerM: 0.275,
    outputPerM: 4.4,
    contextWindow: 200_000,
  },
};

export const CODEX_CONTEXT_WINDOWS: Readonly<Record<string, number>> = {
  "gpt-6.2-astra": 272_000,
  "gpt-6.2-sol": 500_000,
  "gpt-6.1-terra": 500_000,
  "gpt-6-luna": 272_000,
};

const STATIC_PLACEHOLDER_MAP: Readonly<
  Record<number, { id: string; displayName: string }>
> = {
  // Gemini 4 Hydrogen
  432: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  433: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  434: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  525: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  526: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  527: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  528: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  529: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  530: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  531: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  669: { id: "gemini-4-hydrogen", displayName: "Gemini 4 Hydrogen" },
  // Gemini 4 Neon
  465: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  466: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  467: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  468: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  532: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  533: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  534: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  535: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  536: { id: "gemini-4-neon", displayName: "Gemini 4 Neon" },
  // Gemini 4 Carbon
  427: { id: "gemini-4-carbon", displayName: "Gemini 4 Carbon" },
  437: { id: "gemini-4-carbon", displayName: "Gemini 4 Carbon" },
  438: { id: "gemini-4-carbon", displayName: "Gemini 4 Carbon" },
  // Gemini 4 Argon
  417: { id: "gemini-4-argon", displayName: "Gemini 4 Argon" },
  418: { id: "gemini-4-argon", displayName: "Gemini 4 Argon" },
  419: { id: "gemini-4-argon", displayName: "Gemini 4 Argon" },
  420: { id: "gemini-4-argon", displayName: "Gemini 4 Argon" },
  421: { id: "gemini-4-argon", displayName: "Gemini 4 Argon" },
  670: { id: "gemini-4-argon", displayName: "Gemini 4 Argon" },
  // Gemini 3.8 Flash
  318: { id: "gemini-3.8-flash", displayName: "Gemini 3.8 Flash" },
  319: { id: "gemini-3.8-flash", displayName: "Gemini 3.8 Flash" },
  320: { id: "gemini-3.8-flash", displayName: "Gemini 3.8 Flash" },
  322: { id: "gemini-3.8-flash", displayName: "Gemini 3.8 Flash" },
  // Claude Opus 5.5 / Sonnet 5.5
  400: { id: "claude-opus-5-5", displayName: "Claude Opus 5.5" },
  401: { id: "claude-opus-5-5", displayName: "Claude Opus 5.5" },
  402: { id: "claude-opus-5-5", displayName: "Claude Opus 5.5" },
  403: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
  404: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
  405: { id: "claude-sonnet-5-5", displayName: "Claude Sonnet 5.5" },
  // Gemini 2.5 Flash & Orchestrators
  50: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
  198: { id: "gemini-2.5-flash", displayName: "Gemini 2.5 Flash" },
  537: { id: "gemini-pro-orchestrator-neon", displayName: "Orchestrator (Neon)" },
  667: { id: "gemini-pro-orchestrator", displayName: "Orchestrator (Carbon)" },
  668: { id: "gemini-pro-orchestrator", displayName: "Orchestrator (Carbon)" },
  681: { id: "gemini-pro-orchestrator-argon", displayName: "Orchestrator (Argon)" },
};

const EXTRA_MODEL_DISPLAY_NAMES: Readonly<Record<string, string>> = {
  "claude-opus-5-5": "Claude Opus 5.5",
  "claude-sonnet-5-5": "Claude Sonnet 5.5",
  "gemini-2.5-flash": "Gemini 2.5 Flash",
  "gemini-pro-orchestrator": "Orchestrator (Carbon)",
  "gemini-pro-orchestrator-neon": "Orchestrator (Neon)",
  "gemini-pro-orchestrator-argon": "Orchestrator (Argon)",
};

function lookupStaticPlaceholder(norm: string): { id: string; displayName: string } | null {
  const m = /^(?:model_placeholder_m)?(\d+)$/i.exec(norm.trim());
  if (!m) return null;
  const num = Number.parseInt(m[1], 10);
  return STATIC_PLACEHOLDER_MAP[num] ?? null;
}

function normalizeModelId(modelId: string): string {
  return modelId
    .trim()
    .replace(/^gemini-api:\/\//i, "")
    .replace(/^companion\//i, "")
    .replace(/^models\//i, "")
    .toLowerCase();
}

function canonicalizeForPricing(norm: string): string {
  let s = norm.replace(/@\d{8}$/, "");
  s = s.replace(/-(?:low|medium|high|max|xhigh)$/, "");
  s = s.replace(/-tiered$/, "");
  s = s.replace(/-(?:\d{8}|\d{4}-\d{2}-\d{2})$/, "");
  s = s.replace(/-(?:preview|latest|thinking)$/, "");
  return s;
}

export function getModelListPrice(modelId: string): ModelListPrice | null {
  const rawNorm = normalizeModelId(modelId);
  if (!rawNorm) return null;
  const norm = lookupStaticPlaceholder(rawNorm)?.id ?? rawNorm;

  const exact = MODEL_LIST_PRICES[norm];
  if (exact) return exact;

  const canonical = canonicalizeForPricing(norm);
  const canonicalMatch = MODEL_LIST_PRICES[canonical];
  if (canonicalMatch) return canonicalMatch;

  // Claude family fallback for future/versioned Claude IDs (e.g. claude-opus-5-5-high)
  if (canonical.startsWith("claude-")) {
    if (canonical.includes("opus")) return MODEL_LIST_PRICES["claude-opus-4-6"];
    if (canonical.includes("sonnet")) return MODEL_LIST_PRICES["claude-sonnet-4-6"];
    if (canonical.includes("haiku")) return MODEL_LIST_PRICES["claude-haiku-4-5"];
  }

  return null;
}

export function getModelContextWindow(modelId: string): number {
  const priced = getModelListPrice(modelId);
  if (priced) return priced.contextWindow;

  const rawNorm = normalizeModelId(modelId);
  const norm = lookupStaticPlaceholder(rawNorm)?.id ?? rawNorm;
  const codexExact = CODEX_CONTEXT_WINDOWS[norm];
  if (codexExact !== undefined) return codexExact;

  const canonical = canonicalizeForPricing(norm);
  const codexCanonical = CODEX_CONTEXT_WINDOWS[canonical];
  if (codexCanonical !== undefined) return codexCanonical;

  if (canonical.startsWith("claude-") || canonical === "claude") {
    return 200_000;
  }
  if (canonical.startsWith("gemini-")) {
    return 1_000_000;
  }
  if (canonical.startsWith("gpt-5")) {
    return 400_000;
  }
  if (canonical.startsWith("gpt-4.1")) {
    return 1_000_000;
  }
  if (
    canonical === "o3" ||
    canonical.startsWith("o3-") ||
    canonical === "o4-mini" ||
    canonical.startsWith("o4-")
  ) {
    return 200_000;
  }
  if (canonical.startsWith("gpt-4o") || canonical.startsWith("gpt-4")) {
    return 128_000;
  }
  return 1_000_000;
}

export function estimateTokensCost(
  modelId: string,
  usage: TokenUsageInput,
): CostBreakdown | null {
  const rate = getModelListPrice(modelId);
  if (!rate) return null;

  const input = Math.max(0, usage.inputTokens);
  const cached = Math.min(input, Math.max(0, usage.cachedTokens));
  const newInput = Math.max(0, input - cached);
  const output = Math.max(0, usage.outputTokens) + Math.max(0, usage.reasoningTokens ?? 0);

  const cachedReadUsd = (cached / 1_000_000) * rate.cachedPerM;
  const newInputUsd = (newInput / 1_000_000) * rate.inputPerM;
  const outputUsd = (output / 1_000_000) * rate.outputPerM;
  const totalUsd = cachedReadUsd + newInputUsd + outputUsd;
  const cacheSavingsUsd = (cached / 1_000_000) * Math.max(0, rate.inputPerM - rate.cachedPerM);

  return {
    totalUsd,
    cachedReadUsd,
    newInputUsd,
    outputUsd,
    cacheSavingsUsd,
  };
}

export function sessionModelKey(row: Pick<ModelSessionStats, "provider" | "source" | "model">): string {
  return `${row.provider}:${row.source}:${row.model}`;
}

export function estimateSessionSummary(
  byModel: ReadonlyArray<ModelSessionStats>,
): SessionCostSummary {
  let hasPricedModels = false;
  let totalUsd = 0;
  let cachedReadUsd = 0;
  let newInputUsd = 0;
  let outputUsd = 0;
  let cacheSavingsUsd = 0;
  const byModelKey = new Map<string, CostBreakdown | null>();

  for (const row of byModel) {
    const cost = estimateTokensCost(row.model, {
      inputTokens: row.input_tokens,
      cachedTokens: row.cached_tokens,
      outputTokens: row.output_tokens,
      reasoningTokens: row.reasoning_tokens,
    });
    byModelKey.set(sessionModelKey(row), cost);
    if (cost) {
      hasPricedModels = true;
      totalUsd += cost.totalUsd;
      cachedReadUsd += cost.cachedReadUsd;
      newInputUsd += cost.newInputUsd;
      outputUsd += cost.outputUsd;
      cacheSavingsUsd += cost.cacheSavingsUsd;
    }
  }

  return {
    hasPricedModels,
    totalUsd,
    cachedReadUsd,
    newInputUsd,
    outputUsd,
    cacheSavingsUsd,
    byModelKey,
  };
}

export function estimateUsageSummaryCosts(summary: UsageSummary): UsageCostSummary {
  let hasPricedModels = false;
  let totalUsd = 0;
  let cachedReadUsd = 0;
  let newInputUsd = 0;
  let outputUsd = 0;
  let cacheSavingsUsd = 0;

  let pricedCachedTokens = 0;
  let pricedNewInputTokens = 0;
  let pricedOutputTokens = 0;

  const byModelKey = new Map<string, CostBreakdown | null>();
  const byProviderKey = new Map<string, number | null>();

  for (const row of summary.by_model) {
    const rowId = `${row.provider}:${row.source ?? ""}:${row.key}`;
    const cost = estimateTokensCost(row.key, {
      inputTokens: row.input_tokens,
      cachedTokens: row.cached_tokens,
      outputTokens: row.output_tokens,
      reasoningTokens: row.reasoning_tokens,
    });
    byModelKey.set(rowId, cost);

    if (cost) {
      hasPricedModels = true;
      totalUsd += cost.totalUsd;
      cachedReadUsd += cost.cachedReadUsd;
      newInputUsd += cost.newInputUsd;
      outputUsd += cost.outputUsd;
      cacheSavingsUsd += cost.cacheSavingsUsd;

      const cached = Math.min(Math.max(0, row.input_tokens), Math.max(0, row.cached_tokens));
      const newIn = Math.max(0, row.input_tokens - cached);
      const out = Math.max(0, row.output_tokens) + Math.max(0, row.reasoning_tokens);
      pricedCachedTokens += cached;
      pricedNewInputTokens += newIn;
      pricedOutputTokens += out;

      const prevProv = byProviderKey.get(row.provider);
      byProviderKey.set(row.provider, (prevProv ?? 0) + cost.totalUsd);
    } else if (!byProviderKey.has(row.provider)) {
      byProviderKey.set(row.provider, null);
    }
  }

  // Derive weighted effective USD-per-token rates for stacked bucket bars
  const effCachedRate = pricedCachedTokens > 0 ? cachedReadUsd / pricedCachedTokens : 0;
  const effNewRate = pricedNewInputTokens > 0 ? newInputUsd / pricedNewInputTokens : 0;
  const effOutRate = pricedOutputTokens > 0 ? outputUsd / pricedOutputTokens : 0;

  const bucketCosts = summary.buckets.map((b: UsageBucket): CostBreakdown | null => {
    if (!hasPricedModels) return null;
    const input = Math.max(0, b.input_tokens);
    const cached = Math.min(input, Math.max(0, b.cached_tokens));
    const newIn = Math.max(0, input - cached);
    const out = Math.max(0, b.output_tokens) + Math.max(0, b.reasoning_tokens);

    const bCachedUsd = cached * effCachedRate;
    const bNewUsd = newIn * effNewRate;
    const bOutUsd = out * effOutRate;
    return {
      totalUsd: bCachedUsd + bNewUsd + bOutUsd,
      cachedReadUsd: bCachedUsd,
      newInputUsd: bNewUsd,
      outputUsd: bOutUsd,
      cacheSavingsUsd: 0,
    };
  });

  return {
    hasPricedModels,
    totalUsd,
    cachedReadUsd,
    newInputUsd,
    outputUsd,
    cacheSavingsUsd,
    byModelKey,
    byProviderKey,
    bucketCosts,
  };
}

function findKnownModelDisplayName(
  candidate: string,
  presets: ReadonlyArray<ModelPreset>,
): string | null {
  const preset = presets.find((p) => normalizeModelId(p.id) === candidate);
  if (preset) return preset.display_name;

  const curated = CURATED_MODEL_CATALOG.find((m) => m.id === candidate);
  if (curated) return curated.displayName;

  const curatedCodex = CURATED_CODEX_MODEL_CATALOG.find((m) => m.id === candidate);
  if (curatedCodex) return curatedCodex.displayName;

  if (candidate === "antigravity-builtin") return "Antigravity Built-in";
  return EXTRA_MODEL_DISPLAY_NAMES[candidate] ?? null;
}

export function formatModelLabel(
  modelId: string,
  presets: ReadonlyArray<ModelPreset>,
  displayName?: string | null,
): string {
  const norm = normalizeModelId(modelId);
  const exactKnown = findKnownModelDisplayName(norm, presets);
  if (exactKnown) return exactKnown;

  const canonical = canonicalizeForPricing(norm);
  if (canonical !== norm) {
    const canonicalKnown = findKnownModelDisplayName(canonical, presets);
    if (canonicalKnown) return canonicalKnown;
  }

  const staticEntry = lookupStaticPlaceholder(norm);
  if (staticEntry) {
    return findKnownModelDisplayName(staticEntry.id, presets) ?? staticEntry.displayName;
  }

  const trimmedDisplay = (displayName ?? "").trim();
  if (trimmedDisplay) return trimmedDisplay;

  return modelId.trim() || "Unknown model";
}

export function formatProviderBadge(provider: string, source: string | null): string {
  const p = provider.trim().toLowerCase();
  const s = (source ?? "").trim().toLowerCase();
  if (p === "vertex_claude") return "Anthropic";
  if (p === "vertex_openai") return "OpenAI";
  if (p === "gemini" && s === "direct") return "AI Studio (Direct)";
  if (p === "antigravity" || s === "builtin") return "Antigravity";
  if (p === "gemini") return "AI Studio";
  if (p === "openai-codex" || p === "codex") return "OpenAI Codex";
  if (!p) return "Antigravity";
  return p.charAt(0).toUpperCase() + p.slice(1);
}

export function isBuiltinProvider(provider: string, source: string | null): boolean {
  const p = provider.trim().toLowerCase();
  const s = (source ?? "").trim().toLowerCase();
  return (
    s === "builtin" ||
    p === "antigravity" ||
    p === "vertex_claude" ||
    p === "vertex_openai"
  );
}

export function formatProviderFullName(provider: string, source?: string | null): string {
  const p = provider.trim().toLowerCase();
  const s = (source ?? "").trim().toLowerCase();
  if (p === "vertex_claude") return "Vertex Claude";
  if (p === "vertex_openai") return "Vertex OpenAI";
  if (p === "gemini" && s === "direct") return "Google AI Studio (Direct)";
  if (p === "antigravity") return "Antigravity";
  if (p === "gemini") return "Google AI Studio";
  if (p === "openai-codex" || p === "codex") return "OpenAI Codex";
  if (!p) return "Antigravity";
  return p.charAt(0).toUpperCase() + p.slice(1);
}
