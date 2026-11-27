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
  s = s.replace(/-(?:\d{8}|\d{4}-\d{2}-\d{2})$/, "");
  s = s.replace(/-(?:preview|latest|thinking)$/, "");
  return s;
}

export function getModelListPrice(modelId: string): ModelListPrice | null {
  const norm = normalizeModelId(modelId);
  if (!norm) return null;

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

  const norm = normalizeModelId(modelId);
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

export function formatModelLabel(
  modelId: string,
  presets: ReadonlyArray<ModelPreset>,
): string {
  const norm = normalizeModelId(modelId);
  const preset = presets.find((p) => normalizeModelId(p.id) === norm);
  if (preset) return preset.display_name;

  const curated = CURATED_MODEL_CATALOG.find((m) => m.id === norm);
  if (curated) return curated.displayName;

  const curatedCodex = CURATED_CODEX_MODEL_CATALOG.find((m) => m.id === norm);
  if (curatedCodex) return curatedCodex.displayName;

  if (norm === "antigravity-builtin") return "Antigravity Built-in";
  return modelId.trim() || "Unknown model";
}

export function formatProviderBadge(provider: string, source: string | null): string {
  const p = provider.trim().toLowerCase();
  const s = (source ?? "").trim().toLowerCase();
  if (p === "vertex_claude") return "Claude";
  if (p === "vertex_openai") return "OpenAI";
  if (p === "gemini" && s === "direct") return "Direct";
  if (p === "antigravity" || s === "builtin") return "Antigravity";
  if (p === "gemini") return "Gemini";
  if (p === "openai-codex" || p === "codex") return "Codex";
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
