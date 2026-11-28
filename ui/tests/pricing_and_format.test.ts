import assert from "node:assert/strict";
import fs from "node:fs";
import { describe, it } from "node:test";
import {
  formatAccountPlanSummary,
  formatCompactTokens,
  formatLatencyMs,
  formatRamMb,
  formatResolvedPlanLabel,
  formatShortDateUtc,
  formatTimeHHMM,
  formatUsd,
  formatUsdCompact,
  quitReopenCapitalized,
  quitReopenSentence,
} from "../src/dom.ts";
import {
  MODEL_LIST_PRICES,
  estimateSessionSummary,
  estimateTokensCost,
  estimateUsageSummaryCosts,
  formatModelLabel,
  formatProviderBadge,
  formatProviderFullName,
  getModelContextWindow,
  getModelListPrice,
  isBuiltinProvider,
  sessionModelKey,
} from "../src/pricing.ts";
import {
  DEFAULT_MODEL_PRESETS,
  parseSettingsSnapshot,
  parseUsageSummary,
  type UsageSummary,
} from "../src/types.ts";

function loadFixture(name: string): unknown {
  const url = new URL(`./fixtures/${name}`, import.meta.url);
  return JSON.parse(fs.readFileSync(url, "utf8")) as unknown;
}

describe("pricing.ts list prices, token cost math & rollups", () => {
  it("resolves Gemini, Vertex Claude, and Vertex OpenAI models with suffix stripping, family fallback, and accurate context windows", () => {
    const geminiIds = [
      "gemini-4-carbon",
      "gemini-4-neon",
      "gemini-4-hydrogen",
      "gemini-4-argon",
      "gemini-3.8-pro",
      "gemini-3.8-flash",
      "gemini-3.1-pro",
      "gemini-3.1-flash",
      "gemini-2.5-pro",
      "gemini-2.5-flash",
    ];
    for (const id of geminiIds) {
      const rate = getModelListPrice(id);
      assert.ok(rate !== null, `expected list price for ${id}`);
      assert.equal(rate.contextWindow, 1_000_000);
      assert.equal(getModelContextWindow(id), 1_000_000);
    }

    const claudeIds = [
      "claude-opus-4-6",
      "claude-opus-4-5",
      "claude-opus-4",
      "claude-sonnet-4-6",
      "claude-sonnet-4-5",
      "claude-sonnet-4",
      "claude-haiku-4-5",
      "claude-3-5-haiku",
    ];
    for (const id of claudeIds) {
      const rate = getModelListPrice(id);
      assert.ok(rate !== null, `expected list price for ${id}`);
      assert.equal(rate.contextWindow, 200_000);
      assert.equal(getModelContextWindow(id), 200_000);
    }
    assert.equal(MODEL_LIST_PRICES["claude-opus-4-6"].inputPerM, 5.0);
    assert.equal(MODEL_LIST_PRICES["claude-opus-4-6"].cachedPerM, 0.5);
    assert.equal(MODEL_LIST_PRICES["claude-opus-4-6"].outputPerM, 25.0);
    assert.equal(MODEL_LIST_PRICES["claude-opus-4"].inputPerM, 15.0);
    assert.equal(MODEL_LIST_PRICES["claude-sonnet-4-6"].inputPerM, 3.0);
    assert.equal(MODEL_LIST_PRICES["claude-haiku-4-5"].inputPerM, 1.0);

    const openaiExpectedCtx: ReadonlyArray<[string, number]> = [
      ["gpt-5.4", 400_000],
      ["gpt-5.3-codex", 400_000],
      ["gpt-5", 400_000],
      ["gpt-4.1", 1_000_000],
      ["gpt-4o", 128_000],
      ["o3", 200_000],
      ["o4-mini", 200_000],
    ];
    for (const [id, ctx] of openaiExpectedCtx) {
      const rate = getModelListPrice(id);
      assert.ok(rate !== null, `expected list price for ${id}`);
      assert.equal(rate.contextWindow, ctx);
      assert.equal(getModelContextWindow(id), ctx);
    }

    // Prefix, case, effort-suffix (-low/-medium/-high/-max), date-stamp & preview normalization
    assert.deepEqual(
      getModelListPrice("  MODELS/Gemini-4-Carbon  "),
      MODEL_LIST_PRICES["gemini-4-carbon"],
    );
    assert.deepEqual(
      getModelListPrice("companion/gemini-3.8-pro-high"),
      MODEL_LIST_PRICES["gemini-3.8-pro"],
    );
    assert.deepEqual(
      getModelListPrice("gemini-3.1-pro-preview"),
      MODEL_LIST_PRICES["gemini-3.1-pro"],
    );
    assert.deepEqual(
      getModelListPrice("claude-sonnet-4-6-medium"),
      MODEL_LIST_PRICES["claude-sonnet-4-6"],
    );
    assert.deepEqual(
      getModelListPrice("claude-sonnet-4-5@20250929"),
      MODEL_LIST_PRICES["claude-sonnet-4-5"],
    );
    assert.deepEqual(
      getModelListPrice("claude-opus-4-6-max"),
      MODEL_LIST_PRICES["claude-opus-4-6"],
    );
    assert.deepEqual(
      getModelListPrice("gpt-5.3-codex-high"),
      MODEL_LIST_PRICES["gpt-5.3-codex"],
    );
    assert.deepEqual(
      getModelListPrice("o4-mini-low"),
      MODEL_LIST_PRICES["o4-mini"],
    );

    // Claude family fallback for future/versioned IDs (always 200K, never 1M)
    assert.deepEqual(
      getModelListPrice("claude-opus-5-5-high"),
      MODEL_LIST_PRICES["claude-opus-4-6"],
    );
    assert.equal(getModelContextWindow("claude-opus-5-5-high"), 200_000);
    assert.deepEqual(
      getModelListPrice("claude-sonnet-5-0"),
      MODEL_LIST_PRICES["claude-sonnet-4-6"],
    );
    assert.deepEqual(
      getModelListPrice("claude-haiku-5-0-low"),
      MODEL_LIST_PRICES["claude-haiku-4-5"],
    );
    assert.equal(getModelListPrice("claude-future-unknown"), null);
    assert.equal(getModelContextWindow("claude-future-unknown"), 200_000);

    // Non-Claude context window fallbacks & unpriced models
    assert.equal(getModelContextWindow("gemini-5-future-pro"), 1_000_000);
    assert.equal(getModelContextWindow("gpt-5.9-preview"), 400_000);
    assert.equal(getModelContextWindow("gpt-4.1-mini"), 1_000_000);
    assert.equal(getModelContextWindow("o3-pro"), 200_000);
    assert.equal(getModelContextWindow("o4-mini-high"), 200_000);
    assert.equal(getModelContextWindow("gpt-4o-mini"), 128_000);
    assert.equal(getModelContextWindow("gpt-6.2-sol"), 1_050_000);
    assert.equal(getModelContextWindow("gpt-6.2-astra"), 1_050_000);
    assert.equal(getModelListPrice("antigravity-builtin"), null);
    assert.equal(getModelListPrice("gpt-6.2-astra"), null);
    assert.equal(getModelListPrice("gpt-6.2-sol"), null);
    assert.equal(getModelListPrice("gemini-unpriced-preview"), null);
    assert.equal(getModelListPrice(""), null);
    assert.equal(getModelContextWindow("gpt-6.2-astra"), 1_050_000);
    assert.equal(getModelContextWindow("gpt-6.2-sol"), 1_050_000);
    assert.equal(getModelContextWindow("gpt-6.1-terra"), 1_050_000);
    assert.equal(getModelContextWindow("gpt-6-luna"), 1_050_000);
    assert.equal(getModelContextWindow("unknown-model"), 1_000_000);
  });

  it("computes exact estimateTokensCost including reasoning tokens and clamps invalid inputs", () => {
    const cost = estimateTokensCost("gemini-4-carbon", {
      inputTokens: 1_000_000,
      cachedTokens: 600_000,
      outputTokens: 100_000,
      reasoningTokens: 50_000,
    });
    assert.ok(cost !== null);
    assert.ok(Math.abs(cost.cachedReadUsd - 0.12) < 1e-12);
    assert.ok(Math.abs(cost.newInputUsd - 1.6) < 1e-12);
    assert.ok(Math.abs(cost.outputUsd - 3.0) < 1e-12);
    assert.ok(Math.abs(cost.totalUsd - 4.72) < 1e-12);
    assert.ok(Math.abs(cost.cacheSavingsUsd - 2.28) < 1e-12);

    // Vertex Claude cost math (Opus 4.6: $5 / $0.50 / $25 per 1M)
    const claudeCost = estimateTokensCost("claude-opus-4-6-high", {
      inputTokens: 200_000,
      cachedTokens: 100_000,
      outputTokens: 10_000,
      reasoningTokens: 10_000,
    });
    assert.ok(claudeCost !== null);
    assert.ok(Math.abs(claudeCost.cachedReadUsd - 0.05) < 1e-12);
    assert.ok(Math.abs(claudeCost.newInputUsd - 0.5) < 1e-12);
    assert.ok(Math.abs(claudeCost.outputUsd - 0.5) < 1e-12);
    assert.ok(Math.abs(claudeCost.totalUsd - 1.05) < 1e-12);

    // cachedTokens > inputTokens clamps newInput to 0 (no negative cost)
    const clampedCache = estimateTokensCost("gemini-4-carbon", {
      inputTokens: 500_000,
      cachedTokens: 900_000,
      outputTokens: 0,
    });
    assert.ok(clampedCache !== null);
    assert.equal(clampedCache.newInputUsd, 0);
    assert.ok(Math.abs(clampedCache.cachedReadUsd - 0.1) < 1e-12);

    // Negative tokens clamp to 0
    const negative = estimateTokensCost("gemini-4-hydrogen", {
      inputTokens: -100,
      cachedTokens: -50,
      outputTokens: -25,
      reasoningTokens: -10,
    });
    assert.deepEqual(negative, {
      totalUsd: 0,
      cachedReadUsd: 0,
      newInputUsd: 0,
      outputUsd: 0,
      cacheSavingsUsd: 0,
    });

    assert.equal(
      estimateTokensCost("antigravity-builtin", {
        inputTokens: 1000,
        cachedTokens: 0,
        outputTokens: 100,
      }),
      null,
    );
  });

  it("computes sessionModelKey and estimateSessionSummary on priced, mixed, and unpriced rows", () => {
    assert.equal(
      sessionModelKey({ provider: "gemini", source: "proxy", model: "gemini-4-carbon" }),
      "gemini:proxy:gemini-4-carbon",
    );

    const active = parseSettingsSnapshot(loadFixture("settings_snapshot_active.json"));
    assert.ok(active?.stats);

    const summary = estimateSessionSummary(active.stats.by_model);
    assert.equal(summary.hasPricedModels, true);
    const carbonCost = summary.byModelKey.get("gemini:proxy:gemini-4-carbon");
    const builtinCost = summary.byModelKey.get("antigravity:builtin:gemini-3.8-pro");
    assert.ok(carbonCost !== null && carbonCost !== undefined);
    assert.ok(builtinCost !== null && builtinCost !== undefined);
    assert.ok(Math.abs(summary.totalUsd - (carbonCost.totalUsd + builtinCost.totalUsd)) < 1e-12);

    const empty = estimateSessionSummary([]);
    assert.equal(empty.hasPricedModels, false);
    assert.equal(empty.totalUsd, 0);

    const unpricedRow = {
      ...active.stats.by_model[1],
      model: "antigravity-builtin",
    };
    const unpricedOnly = estimateSessionSummary([unpricedRow]);
    assert.equal(unpricedOnly.hasPricedModels, false);
    assert.equal(unpricedOnly.totalUsd, 0);

    const mixed = estimateSessionSummary([active.stats.by_model[0], unpricedRow]);
    assert.equal(mixed.hasPricedModels, true);
    assert.equal(mixed.byModelKey.get("antigravity:builtin:antigravity-builtin"), null);
    assert.ok(Math.abs(mixed.totalUsd - carbonCost.totalUsd) < 1e-12);
  });

  it("computes estimateUsageSummaryCosts on usage_summary_7d.json, all-unpriced, and mixed same-provider", () => {
    const usage = parseUsageSummary(loadFixture("usage_summary_7d.json"));
    assert.ok(usage !== null);

    const costs = estimateUsageSummaryCosts(usage);
    assert.equal(costs.hasPricedModels, true);

    const carbon = costs.byModelKey.get("gemini:proxy:gemini-4-carbon");
    const neon = costs.byModelKey.get("gemini:proxy:gemini-4-neon");
    const builtin = costs.byModelKey.get("antigravity:builtin:gemini-3.8-pro");
    assert.ok(carbon !== null && carbon !== undefined);
    assert.ok(neon !== null && neon !== undefined);
    assert.ok(builtin !== null && builtin !== undefined);

    const geminiProv = costs.byProviderKey.get("gemini");
    assert.ok(typeof geminiProv === "number");
    assert.ok(Math.abs(geminiProv - (carbon.totalUsd + neon.totalUsd)) < 1e-12);
    const agyProv = costs.byProviderKey.get("antigravity");
    assert.ok(typeof agyProv === "number");
    assert.ok(Math.abs(agyProv - builtin.totalUsd) < 1e-12);

    assert.equal(costs.bucketCosts.length, 7);
    assert.equal(costs.bucketCosts[0]?.totalUsd, 0);
    assert.equal(costs.bucketCosts[3]?.cacheSavingsUsd, 0);
    assert.ok((costs.bucketCosts[3]?.totalUsd ?? 0) > 0);
    assert.ok((costs.bucketCosts[6]?.totalUsd ?? 0) > (costs.bucketCosts[3]?.totalUsd ?? 0));

    // All-unpriced summary yields null bucketCosts and null provider totals
    const unpricedSummary: UsageSummary = {
      ...usage,
      by_model: [{ ...usage.by_model[1], key: "antigravity-builtin" }],
      by_provider: [usage.by_provider[1]],
    };
    const unpricedCosts = estimateUsageSummaryCosts(unpricedSummary);
    assert.equal(unpricedCosts.hasPricedModels, false);
    assert.equal(unpricedCosts.byProviderKey.get("antigravity"), null);
    assert.ok(unpricedCosts.bucketCosts.every((b) => b === null));

    // Mixed priced + unpriced under the SAME provider does not clobber numeric total
    const mixedSameProvider: UsageSummary = {
      ...usage,
      by_model: [
        usage.by_model[0], // gemini / gemini-4-carbon (priced)
        {
          ...usage.by_model[1],
          key: "gemini-unpriced-preview",
          provider: "gemini",
          source: "proxy",
        },
      ],
    };
    const mixedCosts = estimateUsageSummaryCosts(mixedSameProvider);
    assert.equal(mixedCosts.hasPricedModels, true);
    assert.ok(Math.abs((mixedCosts.byProviderKey.get("gemini") ?? 0) - carbon.totalUsd) < 1e-12);
  });

  it("formats model labels and provider names/badges accurately", () => {
    const customPresets = [
      { id: "gemini-4-carbon", display_name: "Carbon Override", short_label: "Carbon" },
    ];
    assert.equal(formatModelLabel("models/GEMINI-4-CARBON", customPresets), "Carbon Override");
    assert.equal(formatModelLabel("gemini-4-neon", []), "Gemini 4 Neon");
    assert.equal(formatModelLabel("gemini-4-hydrogen", DEFAULT_MODEL_PRESETS), "Gemini 4 Hydrogen");
    assert.equal(formatModelLabel("gpt-6.2-astra", []), "GPT-6.2 Astra");
    assert.equal(formatModelLabel("gpt-6.2-sol", []), "GPT-6.2 Sol");
    assert.equal(formatModelLabel("gpt-6.1-terra", []), "GPT-6.1 Terra");
    assert.equal(formatModelLabel("gpt-6-luna", []), "GPT-6 Luna");
    assert.equal(formatModelLabel("antigravity-builtin", []), "Antigravity Built-in");
    assert.equal(formatModelLabel("  custom-model-v1  ", []), "custom-model-v1");
    assert.equal(formatModelLabel("   ", []), "Unknown model");

    assert.equal(formatProviderBadge("vertex_claude", "builtin"), "Claude");
    assert.equal(formatProviderBadge("vertex_claude", null), "Claude");
    assert.equal(formatProviderBadge("vertex_openai", "builtin"), "OpenAI");
    assert.equal(formatProviderBadge("vertex_openai", null), "OpenAI");
    assert.equal(formatProviderBadge("gemini", "direct"), "Direct");
    assert.equal(formatProviderBadge("antigravity", null), "Antigravity");
    assert.equal(formatProviderBadge("gemini", "builtin"), "Antigravity");
    assert.equal(formatProviderBadge("", null), "Antigravity");
    assert.equal(formatProviderBadge("gemini", "proxy"), "Gemini");
    assert.equal(formatProviderBadge("openai-codex", "proxy"), "Codex");
    assert.equal(formatProviderBadge("codex", "proxy"), "Codex");
    assert.equal(formatProviderBadge("openai", "proxy"), "Openai");

    assert.equal(isBuiltinProvider("vertex_claude", "builtin"), true);
    assert.equal(isBuiltinProvider("vertex_claude", null), true);
    assert.equal(isBuiltinProvider("vertex_openai", "builtin"), true);
    assert.equal(isBuiltinProvider("vertex_openai", null), true);
    assert.equal(isBuiltinProvider("antigravity", "proxy"), true);
    assert.equal(isBuiltinProvider(" Antigravity ", null), true);
    assert.equal(isBuiltinProvider("gemini", "BUILTIN"), true);
    assert.equal(isBuiltinProvider("gemini", "proxy"), false);
    assert.equal(isBuiltinProvider("openai-codex", "proxy"), false);
    assert.equal(isBuiltinProvider("gemini", "direct"), false);
    assert.equal(isBuiltinProvider("codex", "proxy"), false);
    assert.equal(isBuiltinProvider("", null), false);

    assert.equal(formatProviderFullName("vertex_claude"), "Vertex Claude");
    assert.equal(formatProviderFullName("vertex_openai"), "Vertex OpenAI");
    assert.equal(formatProviderFullName("gemini", "direct"), "Google AI Studio (Direct)");
    assert.equal(formatProviderFullName("gemini", "proxy"), "Google AI Studio");
    assert.equal(formatProviderFullName("antigravity"), "Antigravity");
    assert.equal(formatProviderFullName(""), "Antigravity");
    assert.equal(formatProviderFullName("gemini"), "Google AI Studio");
    assert.equal(formatProviderFullName("openai-codex"), "OpenAI Codex");
    assert.equal(formatProviderFullName("codex"), "OpenAI Codex");
    assert.equal(formatProviderFullName("anthropic"), "Anthropic");
  });
});

describe("dom.ts pure formatters", () => {
  it("formats compact token counts across boundaries", () => {
    assert.equal(formatCompactTokens(0), "0");
    assert.equal(formatCompactTokens(-5), "0");
    assert.equal(formatCompactTokens(Number.NaN), "0");
    assert.equal(formatCompactTokens(Number.POSITIVE_INFINITY), "0");
    assert.equal(formatCompactTokens(42), "42");
    assert.equal(formatCompactTokens(999), "999");
    assert.equal(formatCompactTokens(1_000), "1K");
    assert.equal(formatCompactTokens(1_500), "1.5K");
    assert.equal(formatCompactTokens(10_000), "10K");
    assert.equal(formatCompactTokens(148_200), "148K");
    assert.equal(formatCompactTokens(1_000_000), "1M");
    assert.equal(formatCompactTokens(2_250_000), "2.3M");
    assert.equal(formatCompactTokens(1_000_000_000), "1B");
    assert.equal(formatCompactTokens(1_250_000_000), "1.3B");
  });

  it("formats RAM MB with 1 MB floor and latency ms/s", () => {
    assert.equal(formatRamMb(0), "1 MB RAM");
    assert.equal(formatRamMb(100), "1 MB RAM");
    assert.equal(formatRamMb(512_000), "1 MB RAM");
    assert.equal(formatRamMb(18_874_368), "18 MB RAM");
    assert.equal(formatRamMb(25_165_824), "24 MB RAM");

    assert.equal(formatLatencyMs(-1), "0ms");
    assert.equal(formatLatencyMs(Number.NaN), "0ms");
    assert.equal(formatLatencyMs(0), "0ms");
    assert.equal(formatLatencyMs(420), "420ms");
    assert.equal(formatLatencyMs(999), "999ms");
    assert.equal(formatLatencyMs(1000), "1.0s");
    assert.equal(formatLatencyMs(1550), "1.6s");
  });

  it("formats USD and compact USD", () => {
    assert.equal(formatUsd(0), "\u2248 $0.00");
    assert.equal(formatUsd(-1), "\u2248 $0.00");
    assert.equal(formatUsd(Number.NaN), "\u2248 $0.00");
    assert.equal(formatUsd(0.004), "\u2248 <$0.01");
    assert.equal(formatUsd(0.01), "\u2248 $0.01");
    assert.equal(formatUsd(38.4), "\u2248 $38.40");

    assert.equal(formatUsdCompact(0), "\u2248 $0");
    assert.equal(formatUsdCompact(Number.NaN), "\u2248 $0");
    assert.equal(formatUsdCompact(0.003), "\u2248 <$0.01");
    assert.equal(formatUsdCompact(4.25), "\u2248 $4.25");
    assert.equal(formatUsdCompact(10), "\u2248 $10");
    assert.equal(formatUsdCompact(29.6), "\u2248 $30");
  });

  it("formats UTC short dates and timezone-safe local HH:MM timestamps", () => {
    assert.equal(formatShortDateUtc("2026-11-19"), "Nov 19");
    assert.equal(formatShortDateUtc("2026-01-05T12:34:56Z"), "Jan 5");
    assert.equal(formatShortDateUtc("2026-13-01"), "2026-13-01");
    assert.equal(formatShortDateUtc("14:00"), "14:00");

    assert.equal(formatTimeHHMM(0), "");
    assert.equal(formatTimeHHMM(-1), "");
    assert.equal(formatTimeHHMM(Number.NaN), "");

    const ts = 1747300060000;
    const d = new Date(ts);
    const expected = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    assert.equal(formatTimeHHMM(ts), expected);
    assert.match(formatTimeHHMM(ts), /^\d{2}:\d{2}$/);
  });

  it("formats platform reopen copy and account plan summaries", () => {
    assert.equal(quitReopenSentence("macos"), "quit (\u2318Q) and reopen Antigravity");
    assert.equal(quitReopenSentence("linux"), "quit and reopen Antigravity");
    assert.ok(quitReopenSentence("windows").includes('"Antigravity (Companion)" Start Menu shortcut'));

    assert.equal(quitReopenCapitalized("macos"), "Quit (\u2318Q) and reopen Antigravity");
    assert.equal(quitReopenCapitalized("linux"), "Quit and reopen Antigravity");
    assert.ok(
      quitReopenCapitalized("windows").startsWith("Close Antigravity and launch it from"),
    );

    assert.equal(
      formatAccountPlanSummary({ mode: "auto", resolved: "organization" }),
      "Auto (Organization)",
    );
    assert.equal(
      formatAccountPlanSummary({ mode: "auto", resolved: "personal" }),
      "Auto (Personal)",
    );
    assert.equal(formatAccountPlanSummary({ mode: "auto", resolved: null }), "Auto");
    assert.equal(formatAccountPlanSummary({ mode: "personal", resolved: null }), "Personal");
    assert.equal(formatAccountPlanSummary({ mode: "organization", resolved: null }), "Organization");
    assert.equal(formatAccountPlanSummary({ mode: "custom", resolved: null }), "Custom");

    assert.equal(formatResolvedPlanLabel("organization"), "Organization");
    assert.equal(formatResolvedPlanLabel("personal"), "Personal");
    assert.equal(formatResolvedPlanLabel(null), null);
  });
});
