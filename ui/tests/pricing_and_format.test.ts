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
  it("resolves all 5 curated models with prefix/case normalization and 1M context window", () => {
    const expectedIds = [
      "gemini-4-carbon",
      "gemini-4-neon",
      "gemini-4-hydrogen",
      "gemini-4-argon",
      "gemini-3.8-flash",
    ];
    assert.deepEqual(Object.keys(MODEL_LIST_PRICES), expectedIds);

    for (const id of expectedIds) {
      const rate = getModelListPrice(id);
      assert.ok(rate !== null);
      assert.equal(rate.contextWindow, 1_000_000);
      assert.equal(getModelContextWindow(id), 1_000_000);
    }

    assert.deepEqual(
      getModelListPrice("  MODELS/Gemini-4-Carbon  "),
      MODEL_LIST_PRICES["gemini-4-carbon"],
    );
    assert.equal(getModelListPrice("gemini-3.8-pro"), null);
    assert.equal(getModelListPrice("antigravity-builtin"), null);
    assert.equal(getModelListPrice(""), null);
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
      estimateTokensCost("gemini-3.8-pro", {
        inputTokens: 1000,
        cachedTokens: 0,
        outputTokens: 100,
      }),
      null,
    );
  });

  it("computes sessionModelKey and estimateSessionSummary on mixed priced + unpriced rows", () => {
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
    assert.equal(builtinCost, null);
    assert.ok(Math.abs(summary.totalUsd - carbonCost.totalUsd) < 1e-12);

    const empty = estimateSessionSummary([]);
    assert.equal(empty.hasPricedModels, false);
    assert.equal(empty.totalUsd, 0);

    const unpricedOnly = estimateSessionSummary([active.stats.by_model[1]]);
    assert.equal(unpricedOnly.hasPricedModels, false);
    assert.equal(unpricedOnly.totalUsd, 0);
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
    assert.equal(builtin, null);

    const geminiProv = costs.byProviderKey.get("gemini");
    assert.ok(typeof geminiProv === "number");
    assert.ok(Math.abs(geminiProv - (carbon.totalUsd + neon.totalUsd)) < 1e-12);
    assert.equal(costs.byProviderKey.get("antigravity"), null);

    assert.equal(costs.bucketCosts.length, 7);
    assert.equal(costs.bucketCosts[0]?.totalUsd, 0);
    assert.equal(costs.bucketCosts[3]?.cacheSavingsUsd, 0);
    assert.ok((costs.bucketCosts[3]?.totalUsd ?? 0) > 0);
    assert.ok((costs.bucketCosts[6]?.totalUsd ?? 0) > (costs.bucketCosts[3]?.totalUsd ?? 0));

    // All-unpriced summary yields null bucketCosts and null provider totals
    const unpricedSummary: UsageSummary = {
      ...usage,
      by_model: [usage.by_model[1]],
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
    assert.equal(formatModelLabel("antigravity-builtin", []), "Antigravity Built-in");
    assert.equal(formatModelLabel("  custom-model-v1  ", []), "custom-model-v1");
    assert.equal(formatModelLabel("   ", []), "Unknown model");

    assert.equal(formatProviderBadge("antigravity", null), "Antigravity");
    assert.equal(formatProviderBadge("gemini", "builtin"), "Antigravity");
    assert.equal(formatProviderBadge("", null), "Antigravity");
    assert.equal(formatProviderBadge("gemini", "proxy"), "Gemini");
    assert.equal(formatProviderBadge("openai", "proxy"), "Openai");

    assert.equal(isBuiltinProvider("antigravity", "proxy"), true);
    assert.equal(isBuiltinProvider(" Antigravity ", null), true);
    assert.equal(isBuiltinProvider("gemini", "BUILTIN"), true);
    assert.equal(isBuiltinProvider("gemini", "proxy"), false);
    assert.equal(isBuiltinProvider("", null), false);

    assert.equal(formatProviderFullName("antigravity"), "Antigravity");
    assert.equal(formatProviderFullName(""), "Antigravity");
    assert.equal(formatProviderFullName("gemini"), "Google AI Studio");
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
