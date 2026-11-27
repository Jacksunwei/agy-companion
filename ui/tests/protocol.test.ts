import assert from "node:assert/strict";
import fs from "node:fs";
import { describe, it } from "node:test";
import {
  DEFAULT_CODEX_CONFIG,
  DEFAULT_CODEX_MODEL_PRESETS,
  DEFAULT_ENABLED_CODEX_MODELS,
  DEFAULT_ENABLED_MODELS,
  DEFAULT_MODEL_PRESETS,
  PLACEHOLDER_API_KEY,
  extractStructuredPayload,
  extractToolErrorText,
  extractToolText,
  formatCaughtError,
  normalizeHostPlatform,
  parseSettingsSnapshot,
  parseStartCodexLoginPayload,
  parseTestConnectionPayload,
  parseUsageSummary,
  validateApiKey,
} from "../src/types.ts";

function loadFixture(name: string): unknown {
  const url = new URL(`./fixtures/${name}`, import.meta.url);
  return JSON.parse(fs.readFileSync(url, "utf8")) as unknown;
}

function cloneJson<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

describe("golden wire fixtures (Rust <-> TS parity)", () => {
  it("parses settings_snapshot_active.json with RSS, account_plan, mixed calls, and strips Rust-only cloudcode", () => {
    const raw = loadFixture("settings_snapshot_active.json");
    const parsed = parseSettingsSnapshot(raw);
    assert.ok(parsed !== null);

    assert.equal(parsed.binary_version, "0.1.0");
    assert.equal(parsed.proxy.reachable, true);
    assert.equal(parsed.proxy.status, "ok");
    assert.equal(parsed.proxy.version, "0.1.0");
    assert.equal(parsed.proxy.protocol_version, 1);
    assert.equal(parsed.proxy.error, null);
    assert.equal(parsed.version_skew, false);

    assert.equal(parsed.proxy_rss_bytes, 25_165_824);
    assert.equal(parsed.stats?.proxy_rss_bytes, 25_165_824);
    assert.deepEqual(parsed.account_plan, {
      mode: "auto",
      resolved: "organization",
    });
    assert.equal(parsed.platform, "macos");
    assert.equal(parsed.ls_telemetry_active, false);

    assert.equal(parsed.config.schema_version, 1);
    assert.deepEqual(parsed.config.proxy, { host: "127.0.0.1", port: 24141 });
    assert.equal(parsed.config.launcher_auto_start, true);
    assert.equal(parsed.config.providers.gemini.api_key_configured, true);
    assert.equal(parsed.config.providers.gemini.api_key_hint, "AIza...1234");
    assert.deepEqual(parsed.config.providers.gemini.enabled_models, [
      "gemini-4-carbon",
      "gemini-4-neon",
      "gemini-4-hydrogen",
    ]);
    assert.deepEqual(parsed.config.providers.codex, DEFAULT_CODEX_CONFIG);

    assert.equal(parsed.launcher_integration?.state, "active");
    assert.equal(parsed.launcher_integration?.enabled, true);
    assert.equal(parsed.launcher_integration?.hybrid_active, true);
    assert.equal(parsed.launcher_integration?.shell, "zsh");

    assert.equal(parsed.supported_models.length, 5);
    assert.deepEqual(parsed.supported_models, DEFAULT_MODEL_PRESETS);
    assert.equal(parsed.supported_codex_models.length, 4);
    assert.deepEqual(parsed.supported_codex_models, DEFAULT_CODEX_MODEL_PRESETS);
    assert.equal(parsed.codex_login_pending, false);

    assert.ok(parsed.stats !== null);
    assert.equal(parsed.stats.usage.requests, 2);
    assert.equal(parsed.stats.usage.cache_hit_ratio, 0.625);
    assert.equal(parsed.stats.recent_calls.length, 2);
    assert.equal(parsed.stats.recent_calls[0].provider, "antigravity");
    assert.equal(parsed.stats.recent_calls[0].source, "builtin");
    assert.equal(parsed.stats.recent_calls[0].cold_start, true);
    assert.equal(parsed.stats.recent_calls[1].provider, "gemini");
    assert.equal(parsed.stats.recent_calls[1].source, "proxy");
    assert.equal(parsed.stats.by_model.length, 2);

    // Rust-only wire fields are ignored cleanly by the TS surface
    assert.equal("cloudcode" in parsed, false);
    assert.equal("cloudcode" in parsed.config, false);
    assert.equal("cloudcode" in parsed.stats, false);
  });

  it("parses settings_snapshot_day0_offline.json", () => {
    const raw = loadFixture("settings_snapshot_day0_offline.json");
    const parsed = parseSettingsSnapshot(raw);
    assert.ok(parsed !== null);

    assert.equal(parsed.config.providers.gemini.api_key_configured, false);
    assert.equal(parsed.config.providers.gemini.api_key_hint, null);
    assert.deepEqual(parsed.config.providers.codex, DEFAULT_CODEX_CONFIG);
    assert.equal(parsed.config.launcher_auto_start, null);
    assert.equal(parsed.proxy.reachable, false);
    assert.equal(parsed.proxy.status, null);
    assert.equal(parsed.proxy.version, null);
    assert.equal(parsed.proxy.protocol_version, null);
    assert.equal(parsed.proxy.error, "connection refused");
    assert.equal(parsed.stats, null);
    assert.equal(parsed.proxy_rss_bytes, null);
    assert.equal(parsed.launcher_integration?.state, "manual_required");
    assert.equal(parsed.launcher_integration?.enabled, false);
    assert.equal(parsed.launcher_integration?.hybrid_active, false);
    assert.ok(parsed.launcher_integration?.manual_snippet?.includes("env.sh"));
    assert.equal(parsed.platform, "macos");
    assert.equal(parsed.ls_telemetry_active, false);
    assert.deepEqual(parsed.account_plan, { mode: "auto", resolved: null });
  });

  it("parses usage_summary_7d.json with 7 buckets and model/provider breakdowns", () => {
    const raw = loadFixture("usage_summary_7d.json");
    const parsed = parseUsageSummary(raw);
    assert.ok(parsed !== null);

    assert.equal(parsed.range, "7d");
    assert.equal(parsed.since_utc, "2026-05-09T00:00:00Z");
    assert.equal(parsed.totals.label, "");
    assert.equal(parsed.totals.requests, 3);
    assert.equal(parsed.totals.input_tokens, 3500);
    assert.equal(parsed.totals.cached_tokens, 2000);
    assert.equal(parsed.totals.output_tokens, 850);
    assert.equal(parsed.totals.reasoning_tokens, 170);

    assert.equal(parsed.buckets.length, 7);
    assert.equal(parsed.by_model.length, 3);
    assert.equal(parsed.by_model[0].source, "proxy");
    assert.equal(parsed.by_model[0].model_count, null);
    assert.equal(parsed.by_model[1].provider, "antigravity");
    assert.equal(parsed.by_model[1].source, "builtin");

    assert.equal(parsed.by_provider.length, 2);
    assert.equal(parsed.by_provider[0].key, "gemini");
    assert.equal(parsed.by_provider[0].source, null);
    assert.equal(parsed.by_provider[0].model_count, 2);
    assert.equal(parsed.by_provider[1].key, "antigravity");
    assert.equal(parsed.by_provider[1].source, null);
    assert.equal(parsed.by_provider[1].model_count, 1);
  });

  it("parses test_connection_ok.json and start_codex_login.json", () => {
    const raw = loadFixture("test_connection_ok.json");
    const parsed = parseTestConnectionPayload(raw);
    assert.deepEqual(parsed, {
      ok: true,
      message:
        "Connected to Gemini upstream (https://generativelanguage.googleapis.com) successfully.",
    });

    const startRaw = loadFixture("start_codex_login.json");
    const startParsed = parseStartCodexLoginPayload(startRaw);
    assert.deepEqual(startParsed, {
      authorize_url:
        "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann",
      callback_listening: true,
      manual_fallback: false,
    });
    assert.equal(parseStartCodexLoginPayload({ authorize_url: "", callback_listening: true, manual_fallback: false }), null);
    assert.equal(parseStartCodexLoginPayload({ authorize_url: "https://auth.openai.com", callback_listening: "yes", manual_fallback: false }), null);
    assert.equal(parseStartCodexLoginPayload(null), null);
  });
});

describe("parseSettingsSnapshot defensive invariants", () => {
  const activeBase = loadFixture("settings_snapshot_active.json") as Record<string, unknown>;

  it("rejects non-object inputs and missing required top-level fields", () => {
    assert.equal(parseSettingsSnapshot(null), null);
    assert.equal(parseSettingsSnapshot(undefined), null);
    assert.equal(parseSettingsSnapshot([]), null);
    assert.equal(parseSettingsSnapshot("string"), null);

    const noConfig = cloneJson(activeBase);
    delete noConfig.config;
    assert.equal(parseSettingsSnapshot(noConfig), null);

    const noProxy = cloneJson(activeBase);
    delete noProxy.proxy;
    assert.equal(parseSettingsSnapshot(noProxy), null);

    const badSkew = cloneJson(activeBase);
    badSkew.version_skew = "false";
    assert.equal(parseSettingsSnapshot(badSkew), null);
  });

  it("enforces strict nullable fields (rejects undefined or non-finite numbers) and malformed stats", () => {
    const missingHint = cloneJson(activeBase) as {
      config: { providers: { gemini: Record<string, unknown> } };
    };
    delete missingHint.config.providers.gemini.api_key_hint;
    assert.equal(parseSettingsSnapshot(missingHint), null);

    const missingProxyStatus = cloneJson(activeBase) as { proxy: Record<string, unknown> };
    delete missingProxyStatus.proxy.status;
    assert.equal(parseSettingsSnapshot(missingProxyStatus), null);

    const nonFinitePort = cloneJson(activeBase) as { config: { proxy: Record<string, unknown> } };
    nonFinitePort.config.proxy.port = Number.NaN;
    assert.equal(parseSettingsSnapshot(nonFinitePort), null);

    const badStats = cloneJson(activeBase);
    badStats.stats = { usage: { requests: 1 } };
    assert.equal(parseSettingsSnapshot(badStats), null);

    const omittedStats = cloneJson(activeBase);
    delete omittedStats.stats;
    const parsedOmitted = parseSettingsSnapshot(omittedStats);
    assert.ok(parsedOmitted !== null);
    assert.equal(parsedOmitted.stats, null);
  });

  it("handles launcher_integration leniently (null, missing, or unknown state)", () => {
    for (const candidate of [null, undefined, { state: "bogus", enabled: true, hybrid_active: true }]) {
      const sample = cloneJson(activeBase);
      sample.launcher_integration = candidate;
      const parsed = parseSettingsSnapshot(sample);
      assert.ok(parsed !== null);
      assert.equal(parsed.launcher_integration, null);
    }
  });

  it("resolves proxy_rss_bytes from top-level first, then falls back to stats.proxy_rss_bytes", () => {
    const topWins = cloneJson(activeBase) as Record<string, unknown> & {
      stats: Record<string, unknown>;
    };
    topWins.proxy_rss_bytes = 33_554_432;
    topWins.stats.proxy_rss_bytes = 12_582_912;
    assert.equal(parseSettingsSnapshot(topWins)?.proxy_rss_bytes, 33_554_432);

    const fallbackFromNull = cloneJson(activeBase) as Record<string, unknown> & {
      stats: Record<string, unknown>;
    };
    fallbackFromNull.proxy_rss_bytes = null;
    fallbackFromNull.stats.proxy_rss_bytes = 18_874_368;
    assert.equal(parseSettingsSnapshot(fallbackFromNull)?.proxy_rss_bytes, 18_874_368);

    const fallbackFromOmitted = cloneJson(activeBase) as Record<string, unknown> & {
      stats: Record<string, unknown>;
    };
    delete fallbackFromOmitted.proxy_rss_bytes;
    fallbackFromOmitted.stats.proxy_rss_bytes = 18_874_368;
    assert.equal(parseSettingsSnapshot(fallbackFromOmitted)?.proxy_rss_bytes, 18_874_368);

    const bothNull = cloneJson(activeBase) as Record<string, unknown> & {
      stats: Record<string, unknown>;
    };
    bothNull.proxy_rss_bytes = null;
    bothNull.stats.proxy_rss_bytes = null;
    assert.equal(parseSettingsSnapshot(bothNull)?.proxy_rss_bytes, null);
  });

  it("preserves explicit empty enabled_models [] pause and falls back when omitted", () => {
    const paused = cloneJson(activeBase) as {
      config: { providers: { gemini: Record<string, unknown> } };
    };
    paused.config.providers.gemini.enabled_models = [];
    assert.deepEqual(
      parseSettingsSnapshot(paused)?.config.providers.gemini.enabled_models,
      [],
    );

    const dirty = cloneJson(activeBase) as {
      config: { providers: { gemini: Record<string, unknown> } };
    };
    dirty.config.providers.gemini.enabled_models = ["  gemini-4-carbon ", "", 42, "   "];
    assert.deepEqual(
      parseSettingsSnapshot(dirty)?.config.providers.gemini.enabled_models,
      ["gemini-4-carbon"],
    );

    const omitted = cloneJson(activeBase) as {
      config: { providers: { gemini: Record<string, unknown> } };
    };
    delete omitted.config.providers.gemini.enabled_models;
    assert.deepEqual(
      parseSettingsSnapshot(omitted)?.config.providers.gemini.enabled_models,
      DEFAULT_ENABLED_MODELS,
    );
  });

  it("parses authenticated, omitted, paused, and rejects malformed providers.codex", () => {
    const authed = cloneJson(activeBase) as {
      config: { providers: { codex?: Record<string, unknown> } };
    };
    authed.config.providers.codex = {
      base_url: "https://chatgpt.com/backend-api/codex",
      default_model: "gpt-6.2-astra",
      enabled_models: [],
      timeout_secs: 600,
      authenticated: true,
      account_hint: "j***@example.com",
      plan_type: "pro",
      expires_at_ms: 1_950_000_000_000,
    };
    const parsedAuthed = parseSettingsSnapshot(authed);
    assert.deepEqual(parsedAuthed?.config.providers.codex, {
      base_url: "https://chatgpt.com/backend-api/codex",
      default_model: "gpt-6.2-astra",
      enabled_models: [],
      timeout_secs: 600,
      authenticated: true,
      account_hint: "j***@example.com",
      plan_type: "pro",
      expires_at_ms: 1_950_000_000_000,
    });

    // Omitted providers.codex defaults cleanly for backwards compatibility (e.g. legacy mock snapshots)
    const omittedCodex = cloneJson(activeBase) as {
      config: { providers: { codex?: Record<string, unknown> } };
    };
    delete omittedCodex.config.providers.codex;
    const parsedOmitted = parseSettingsSnapshot(omittedCodex);
    assert.deepEqual(parsedOmitted?.config.providers.codex, {
      ...DEFAULT_CODEX_CONFIG,
      enabled_models: [...DEFAULT_ENABLED_CODEX_MODELS],
    });

    // Present-but-malformed providers.codex fails closed (returns null)
    const malformedCases: Array<(c: Record<string, unknown>) => void> = [
      (c) => {
        c.codex = "not-an-object";
      },
      (c) => {
        c.codex = { ...(c.codex as Record<string, unknown>), authenticated: "true" };
      },
      (c) => {
        const copy = { ...(c.codex as Record<string, unknown>) };
        delete copy.account_hint;
        c.codex = copy;
      },
      (c) => {
        c.codex = { ...(c.codex as Record<string, unknown>), expires_at_ms: Number.NaN };
      },
      (c) => {
        c.codex = { ...(c.codex as Record<string, unknown>), enabled_models: "gpt-6.2-sol" };
      },
    ];
    for (const mutate of malformedCases) {
      const sample = cloneJson(activeBase) as {
        config: { providers: Record<string, unknown> };
      };
      mutate(sample.config.providers);
      assert.equal(parseSettingsSnapshot(sample), null);
    }
  });

  it("falls back to DEFAULT_MODEL_PRESETS when supported_models is empty or invalid", () => {
    for (const candidate of [[], null, [{ id: "", display_name: "X", short_label: "X" }]]) {
      const sample = cloneJson(activeBase);
      sample.supported_models = candidate;
      assert.deepEqual(parseSettingsSnapshot(sample)?.supported_models, DEFAULT_MODEL_PRESETS);
    }

    const partial = cloneJson(activeBase);
    partial.supported_models = [
      { id: "custom-1", display_name: "Custom 1", short_label: "C1" },
      { id: "", display_name: "Broken", short_label: "B" },
    ];
    assert.deepEqual(parseSettingsSnapshot(partial)?.supported_models, [
      { id: "custom-1", display_name: "Custom 1", short_label: "C1" },
    ]);

    // supported_codex_models fallback and strict boolean codex_login_pending validation
    const codexOmitted = cloneJson(activeBase);
    delete codexOmitted.supported_codex_models;
    delete codexOmitted.codex_login_pending;
    const parsedCodexOmitted = parseSettingsSnapshot(codexOmitted);
    assert.deepEqual(parsedCodexOmitted?.supported_codex_models, DEFAULT_CODEX_MODEL_PRESETS);
    assert.equal(parsedCodexOmitted?.codex_login_pending, false);

    const badPending = cloneJson(activeBase);
    badPending.codex_login_pending = "true";
    assert.equal(parseSettingsSnapshot(badPending), null);
  });

  it("parses all account_plan modes and defaults unknown values safely", () => {
    for (const mode of ["auto", "personal", "organization", "custom"] as const) {
      const sample = cloneJson(activeBase);
      sample.account_plan = { mode, resolved: "personal" };
      assert.deepEqual(parseSettingsSnapshot(sample)?.account_plan, {
        mode,
        resolved: "personal",
      });
    }

    const unknown = cloneJson(activeBase);
    unknown.account_plan = { mode: "enterprise", resolved: "other" };
    assert.deepEqual(parseSettingsSnapshot(unknown)?.account_plan, {
      mode: "auto",
      resolved: null,
    });

    const missing = cloneJson(activeBase);
    delete missing.account_plan;
    assert.deepEqual(parseSettingsSnapshot(missing)?.account_plan, {
      mode: "auto",
      resolved: null,
    });
  });

  it("skips malformed recent_calls and by_model rows and defaults optional fields", () => {
    const sample = cloneJson(activeBase) as {
      stats: { recent_calls: unknown[]; by_model: unknown[] };
    };
    sample.stats.recent_calls = [
      { broken: true },
      {
        timestamp_ms: 1747300100000,
        provider: "gemini",
        source: "proxy",
        model: "gemini-4-neon",
        input_tokens: 100,
        cached_tokens: 50,
        output_tokens: 20,
      },
    ];
    sample.stats.by_model = [
      null,
      {
        provider: "gemini",
        source: "proxy",
        model: "gemini-4-neon",
        requests: 1,
        input_tokens: 100,
        cached_tokens: 50,
        output_tokens: 20,
      },
    ];

    const parsed = parseSettingsSnapshot(sample);
    assert.ok(parsed?.stats);
    assert.equal(parsed.stats.recent_calls.length, 1);
    assert.equal(parsed.stats.recent_calls[0].reasoning_tokens, 0);
    assert.equal(parsed.stats.recent_calls[0].latency_ms, 0);
    assert.equal(parsed.stats.recent_calls[0].cold_start, false);
    assert.equal(parsed.stats.recent_calls[0].cache_break, false);
    assert.equal(parsed.stats.by_model.length, 1);
    assert.equal(parsed.stats.by_model[0].reasoning_tokens, 0);
    assert.equal(parsed.stats.by_model[0].total_latency_ms, 0);
  });

  it("parses ls_telemetry_active true, false, omitted, and non-boolean safely", () => {
    const activeLs = cloneJson(activeBase);
    activeLs.ls_telemetry_active = true;
    assert.equal(parseSettingsSnapshot(activeLs)?.ls_telemetry_active, true);

    const inactiveLs = cloneJson(activeBase);
    inactiveLs.ls_telemetry_active = false;
    assert.equal(parseSettingsSnapshot(inactiveLs)?.ls_telemetry_active, false);

    const omittedLs = cloneJson(activeBase);
    delete omittedLs.ls_telemetry_active;
    assert.equal(parseSettingsSnapshot(omittedLs)?.ls_telemetry_active, false);

    const nonBoolLs = cloneJson(activeBase);
    nonBoolLs.ls_telemetry_active = "true";
    assert.equal(parseSettingsSnapshot(nonBoolLs)?.ls_telemetry_active, false);
  });
});

describe("parseUsageSummary & parseTestConnectionPayload edge cases", () => {
  const usageBase = loadFixture("usage_summary_7d.json") as Record<string, unknown>;

  it("accepts 1d/7d/30d, rejects invalid ranges or missing totals, and skips malformed rows", () => {
    for (const range of ["1d", "7d", "30d"] as const) {
      const sample = cloneJson(usageBase);
      sample.range = range;
      assert.equal(parseUsageSummary(sample)?.range, range);
    }

    const badRange = cloneJson(usageBase);
    badRange.range = "90d";
    assert.equal(parseUsageSummary(badRange), null);

    const noSince = cloneJson(usageBase);
    delete noSince.since_utc;
    assert.equal(parseUsageSummary(noSince), null);

    const badTotals = cloneJson(usageBase);
    badTotals.totals = { label: "" };
    assert.equal(parseUsageSummary(badTotals), null);

    const partialRows = cloneJson(usageBase);
    partialRows.buckets = [
      { label: "bad" },
      { label: "2026-05-15", requests: 1, input_tokens: 10, cached_tokens: 0, output_tokens: 5 },
    ];
    partialRows.by_model = [
      "junk",
      {
        key: "gemini-4-carbon",
        provider: "gemini",
        requests: 1,
        input_tokens: 10,
        cached_tokens: 0,
        output_tokens: 5,
      },
    ];
    const parsed = parseUsageSummary(partialRows);
    assert.ok(parsed !== null);
    assert.equal(parsed.buckets.length, 1);
    assert.equal(parsed.buckets[0].reasoning_tokens, 0);
    assert.equal(parsed.by_model.length, 1);
    assert.equal(parsed.by_model[0].avg_latency_ms, 0);
    assert.equal(parsed.by_model[0].source, null);
    assert.equal(parsed.by_model[0].model_count, null);
  });

  it("handles parseTestConnectionPayload failure payloads and malformed objects", () => {
    assert.deepEqual(parseTestConnectionPayload({ ok: false, message: "401 Unauthorized" }), {
      ok: false,
      message: "401 Unauthorized",
    });
    assert.equal(parseTestConnectionPayload({ ok: true }), null);
    assert.equal(parseTestConnectionPayload({ ok: "true", message: "ok" }), null);
    assert.equal(parseTestConnectionPayload(null), null);
  });
});

describe("tool envelope helpers, normalizeHostPlatform & validateApiKey", () => {
  it("extracts tool text, structured payloads, tool errors, and caught errors", () => {
    assert.equal(
      extractToolText({
        content: [
          { type: "text", text: "  first line" },
          { type: "image", text: "ignored" },
          { type: "text", text: "second line  " },
        ],
      }),
      "first line\nsecond line",
    );
    assert.equal(extractToolText({}), "");

    assert.deepEqual(
      extractStructuredPayload({
        structuredContent: { ok: true },
        content: [{ type: "text", text: '{"ok":false}' }],
      }),
      { ok: true },
    );
    assert.equal(extractStructuredPayload({ structuredContent: false }), false);
    assert.equal(extractStructuredPayload({ structuredContent: 0 }), 0);
    assert.deepEqual(
      extractStructuredPayload({
        content: [{ type: "text", text: '{"a":1}' }],
      }),
      { a: 1 },
    );
    assert.equal(
      extractStructuredPayload({
        content: [{ type: "text", text: "not-json" }],
      }),
      null,
    );
    assert.equal(extractStructuredPayload({}), null);

    assert.equal(
      extractToolErrorText({
        structuredContent: { error: "Structured failure" },
        content: [{ type: "text", text: "Text failure" }],
      }),
      "Structured failure",
    );
    assert.equal(
      extractToolErrorText({
        content: [{ type: "text", text: "Text failure" }],
      }),
      "Text failure",
    );
    assert.equal(extractToolErrorText({}), "The companion API reported an error.");

    assert.equal(formatCaughtError(new Error("Boom")), "Boom");
    assert.equal(
      formatCaughtError(new Error("   ")),
      "Request failed. Check the companion sidecar connection.",
    );
    assert.equal(
      formatCaughtError("plain string"),
      "Request failed. Check the companion sidecar connection.",
    );
  });

  it("normalizes explicit host platforms and falls back safely via userAgent", () => {
    assert.equal(normalizeHostPlatform("macos"), "macos");
    assert.equal(normalizeHostPlatform("linux"), "linux");
    assert.equal(normalizeHostPlatform("windows"), "windows");

    const origDesc = Object.getOwnPropertyDescriptor(globalThis, "navigator");
    try {
      Object.defineProperty(globalThis, "navigator", {
        value: { userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" },
        configurable: true,
        enumerable: true,
        writable: true,
      });
      assert.equal(normalizeHostPlatform("unknown"), "windows");

      Object.defineProperty(globalThis, "navigator", {
        value: { userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)" },
        configurable: true,
        enumerable: true,
        writable: true,
      });
      assert.equal(normalizeHostPlatform(undefined), "macos");

      Object.defineProperty(globalThis, "navigator", {
        value: { userAgent: "Node.js/24" },
        configurable: true,
        enumerable: true,
        writable: true,
      });
      assert.equal(normalizeHostPlatform(null), "linux");
    } finally {
      if (origDesc) {
        Object.defineProperty(globalThis, "navigator", origDesc);
      }
    }
  });

  it("validates API keys rejecting blanks, placeholder key, whitespace, non-ASCII, and control chars", () => {
    assert.equal(validateApiKey(""), "Paste a Google AI Studio API key first.");
    assert.equal(validateApiKey("   "), "Paste a Google AI Studio API key first.");

    for (const candidate of [
      PLACEHOLDER_API_KEY,
      `  ${PLACEHOLDER_API_KEY}  `,
      PLACEHOLDER_API_KEY.toUpperCase(),
    ]) {
      assert.equal(
        validateApiKey(candidate),
        "Placeholder key cannot be saved. Paste a real Gemini API key.",
      );
    }

    assert.equal(
      validateApiKey("test bad key"),
      "API key must contain printable ASCII characters with no spaces.",
    );
    assert.equal(
      validateApiKey("test-key-\u2603"),
      "API key must contain printable ASCII characters with no spaces.",
    );
    assert.equal(
      validateApiKey("test-key\r\nnext"),
      "API key must contain printable ASCII characters with no spaces.",
    );

    assert.equal(validateApiKey("test-valid-key-1234"), null);
    assert.equal(validateApiKey("  test-valid-key-1234  "), null);
  });
});
