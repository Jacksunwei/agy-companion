import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  callSidecarApi,
  detectSidecarContext,
  inferThemeFromCssVars,
  isPluginHostProtocol,
  parseCssColorLuminance,
  sanitizeConversationId,
} from "../src/transport.ts";

describe("sidecar transport helpers", () => {
  it("sanitizes valid conversation IDs and rejects invalid ones", () => {
    assert.equal(
      sanitizeConversationId(" a25ce165-c93a-42be-a648-ac6144c7acba "),
      "a25ce165-c93a-42be-a648-ac6144c7acba",
    );
    assert.equal(sanitizeConversationId("short"), null);
    assert.equal(sanitizeConversationId("../etc/passwd"), null);
    assert.equal(sanitizeConversationId("conv?x=1"), null);
    assert.equal(sanitizeConversationId(""), null);
    assert.equal(sanitizeConversationId(null), null);
  });

  it("detects sidecar mode strictly from token or conversationId query params", () => {
    assert.equal(detectSidecarContext(""), null);
    assert.equal(detectSidecarContext("?theme=dark"), null);

    assert.deepEqual(
      detectSidecarContext("?token=tok-123&conversationId=a25ce165-c93a-42be-a648-ac6144c7acba"),
      {
        token: "tok-123",
        conversationId: "a25ce165-c93a-42be-a648-ac6144c7acba",
      },
    );

    assert.deepEqual(detectSidecarContext("?token=tok-only&conversationId=bad/id"), {
      token: "tok-only",
      conversationId: null,
    });

    // Antigravity Electron hosts Sidecar Web UIs on privileged `plugin://` origins.
    assert.deepEqual(
      detectSidecarContext(
        "?token=tok-plugin&conversationId=a25ce165-c93a-42be-a648-ac6144c7acba",
        "plugin:",
      ),
      {
        token: "tok-plugin",
        conversationId: "a25ce165-c93a-42be-a648-ac6144c7acba",
      },
    );
    assert.deepEqual(detectSidecarContext("", "plugin:"), {
      token: "",
      conversationId: null,
    });
    assert.deepEqual(detectSidecarContext("", "http:"), {
      token: "",
      conversationId: null,
    });
    assert.equal(detectSidecarContext("?token=tok-file", "file:"), null);
    assert.equal(detectSidecarContext("", "about:"), null);

    assert.equal(isPluginHostProtocol("plugin:"), true);
    assert.equal(isPluginHostProtocol("PLUGIN:"), true);
    assert.equal(isPluginHostProtocol("http:"), false);
    assert.equal(isPluginHostProtocol("https:"), false);
    assert.equal(isPluginHostProtocol("file:"), false);
    assert.equal(isPluginHostProtocol(""), false);
  });

  it("infers dark and light theme from host CSS background variables", () => {
    assert.ok((parseCssColorLuminance("#1e1e1e") ?? 1) < 0.2);
    assert.ok((parseCssColorLuminance("#ffffff") ?? 0) > 0.9);
    assert.equal(inferThemeFromCssVars({ "--background": "#121316" }), "dark");
    assert.equal(inferThemeFromCssVars({ "--background": "rgb(250, 250, 250)" }), "light");
    assert.equal(inferThemeFromCssVars({}), null);
  });

  it("allows all Codex OAuth sidecar endpoints, forwards conversation_id on snapshot mutations, and rejects unknown actions", async () => {
    const requests: Array<{ url: string; headers: Record<string, string>; body: Record<string, unknown> }> = [];
    const origFetch = globalThis.fetch;
    globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
      requests.push({
        url,
        headers: (init?.headers ?? {}) as Record<string, string>,
        body,
      });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;

    try {
      const ctx = { token: "tok-codex", conversationId: "conv-codex-123456" };
      const pass = (raw: unknown) => raw;

      const rejected = await callSidecarApi(ctx, "not_allowed_tool", {}, pass);
      assert.equal(rejected.ok, false);
      assert.equal(requests.length, 0);

      for (const tool of [
        "start_codex_login",
        "cancel_codex_login",
        "complete_codex_login",
        "disconnect_codex",
        "test_connection",
      ] as const) {
        const res = await callSidecarApi(
          ctx,
          tool,
          tool === "complete_codex_login" ? { callback_input: "code#state" } : {},
          pass,
        );
        assert.equal(res.ok, true, `${tool} should be allowed`);
      }

      assert.deepEqual(
        requests.map((r) => r.url),
        [
          "/_api/start_codex_login",
          "/_api/cancel_codex_login",
          "/_api/complete_codex_login",
          "/_api/disconnect_codex",
          "/_api/test_connection",
        ],
      );
      assert.equal(requests[0].headers["X-Sidecar-Token"], "tok-codex");
      assert.equal("conversation_id" in requests[0].body, false);
      assert.equal(requests[1].body.conversation_id, "conv-codex-123456");
      assert.equal(requests[2].body.conversation_id, "conv-codex-123456");
      assert.equal(requests[2].body.callback_input, "code#state");
      assert.equal(requests[3].body.conversation_id, "conv-codex-123456");
      assert.equal("conversation_id" in requests[4].body, false);
    } finally {
      globalThis.fetch = origFetch;
    }
  });
});
