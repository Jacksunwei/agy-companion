import {
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
 * Official Gemini Developer API Standard Paid Tier list prices (USD per 1M tokens).
 * Source: Gemini API pricing guide (verified Nov 2026). All UI figures are prefixed
 * with approximately and labeled "est. list price".
 */
export const MODEL_LIST_PRICES: Readonly<Record<string, ModelListPrice>> = {
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
  "gemini-3.8-flash": {
    inputPerM: 1.5,
    cachedPerM: 0.15,
    outputPerM: 7.5,
    contextWindow: 1_000_000,
  },
};

function normalizeModelId(modelId: string): string {
  return modelId.trim().replace(/^models\//i, "").toLowerCase();
}

export function getModelListPrice(modelId: string): ModelListPrice | null {
  const norm = normalizeModelId(modelId);
  return MODEL_LIST_PRICES[norm] ?? null;
}

export function getModelContextWindow(modelId: string): number {
  return getModelListPrice(modelId)?.contextWindow ?? 1_000_000;
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

  if (norm === "antigravity-builtin") return "Antigravity Built-in";
  return modelId.trim() || "Unknown model";
}

export function formatProviderBadge(provider: string, source: string | null): string {
  const p = provider.trim().toLowerCase();
  const s = (source ?? "").trim().toLowerCase();
  if (p === "antigravity" || s === "builtin") return "Antigravity";
  if (p === "gemini") return "Gemini";
  if (!p) return "Antigravity";
  return p.charAt(0).toUpperCase() + p.slice(1);
}

export function isBuiltinProvider(provider: string, source: string | null): boolean {
  const p = provider.trim().toLowerCase();
  const s = (source ?? "").trim().toLowerCase();
  return p === "antigravity" || s === "builtin";
}

export function formatProviderFullName(provider: string): string {
  const p = provider.trim().toLowerCase();
  if (p === "antigravity") return "Antigravity";
  if (p === "gemini") return "Google AI Studio";
  if (!p) return "Antigravity";
  return p.charAt(0).toUpperCase() + p.slice(1);
}
