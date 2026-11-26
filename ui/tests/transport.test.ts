import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  detectSidecarContext,
  inferThemeFromCssVars,
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
  });

  it("infers dark and light theme from host CSS background variables", () => {
    assert.ok((parseCssColorLuminance("#1e1e1e") ?? 1) < 0.2);
    assert.ok((parseCssColorLuminance("#ffffff") ?? 0) > 0.9);
    assert.equal(inferThemeFromCssVars({ "--background": "#121316" }), "dark");
    assert.equal(inferThemeFromCssVars({ "--background": "rgb(250, 250, 250)" }), "light");
    assert.equal(inferThemeFromCssVars({}), null);
  });
});
