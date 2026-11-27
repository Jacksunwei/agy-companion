import { formatCaughtError } from "./types.ts";

export type SidecarContext = {
  token: string;
  conversationId: string | null;
};

export type AllowedSidecarTool =
  | "get_settings"
  | "get_usage"
  | "save_settings"
  | "test_connection"
  | "set_launcher_integration"
  | "start_codex_login"
  | "cancel_codex_login"
  | "complete_codex_login"
  | "disconnect_codex";

const ALLOWED_SIDECAR_TOOLS: ReadonlySet<string> = new Set<AllowedSidecarTool>([
  "get_settings",
  "get_usage",
  "save_settings",
  "test_connection",
  "set_launcher_integration",
  "start_codex_login",
  "cancel_codex_login",
  "complete_codex_login",
  "disconnect_codex",
]);

const CONVERSATION_SCOPED_TOOLS: ReadonlySet<string> = new Set<AllowedSidecarTool>([
  "get_settings",
  "save_settings",
  "set_launcher_integration",
  "cancel_codex_login",
  "complete_codex_login",
  "disconnect_codex",
]);

const CONVERSATION_ID_RE = /^[0-9A-Za-z_-]{6,64}$/;

export function sanitizeConversationId(raw: string | null | undefined): string | null {
  if (!raw) return null;
  const trimmed = raw.trim();
  if (!CONVERSATION_ID_RE.test(trimmed)) return null;
  return trimmed;
}

export function detectSidecarContext(
  search: string,
  protocol?: string,
): SidecarContext | null {
  if (protocol && protocol !== "http:" && protocol !== "https:") {
    return null;
  }
  const params = new URLSearchParams(search || "");
  if (!protocol && !params.has("token") && !params.has("conversationId")) {
    return null;
  }
  return {
    token: (params.get("token") ?? "").trim(),
    conversationId: sanitizeConversationId(params.get("conversationId")),
  };
}

export function parseCssColorLuminance(raw: string): number | null {
  const s = raw.trim().toLowerCase();
  let r: number;
  let g: number;
  let b: number;

  const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/.exec(s);
  if (hex) {
    const digits = hex[1];
    if (digits.length === 3) {
      r = parseInt(digits[0] + digits[0], 16);
      g = parseInt(digits[1] + digits[1], 16);
      b = parseInt(digits[2] + digits[2], 16);
    } else {
      r = parseInt(digits.slice(0, 2), 16);
      g = parseInt(digits.slice(2, 4), 16);
      b = parseInt(digits.slice(4, 6), 16);
    }
  } else {
    const rgb = /^rgba?\(\s*(\d+(?:\.\d+)?)\s*[,\s]\s*(\d+(?:\.\d+)?)\s*[,\s]\s*(\d+(?:\.\d+)?)/.exec(
      s,
    );
    if (!rgb) return null;
    r = Number(rgb[1]);
    g = Number(rgb[2]);
    b = Number(rgb[3]);
  }

  if (!Number.isFinite(r) || !Number.isFinite(g) || !Number.isFinite(b)) {
    return null;
  }
  return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
}

export function inferThemeFromCssVars(
  vars: Record<string, string>,
): "dark" | "light" | null {
  const bg =
    vars["--background"] ??
    vars["--color-background-primary"] ??
    vars["--vscode-sideBar-background"] ??
    vars["--vscode-editor-background"];
  if (!bg) return null;
  const lum = parseCssColorLuminance(bg);
  if (lum === null) return null;
  return lum < 0.5 ? "dark" : "light";
}

export async function callSidecarApi<T>(
  ctx: SidecarContext,
  toolName: string,
  args: Record<string, unknown>,
  parser: (raw: unknown) => T | null,
): Promise<{ ok: true; data: T } | { ok: false; error: string }> {
  if (!ALLOWED_SIDECAR_TOOLS.has(toolName)) {
    return { ok: false, error: `Unsupported sidecar action: ${toolName}` };
  }
  const bodyObj: Record<string, unknown> = { ...args };
  if (ctx.conversationId && CONVERSATION_SCOPED_TOOLS.has(toolName)) {
    bodyObj.conversation_id = ctx.conversationId;
  }

  const headers: Record<string, string> = {
    "Content-Type": "application/json",
  };
  if (ctx.token) {
    headers["X-Sidecar-Token"] = ctx.token;
  }

  try {
    const resp = await fetch(`/_api/${toolName}`, {
      method: "POST",
      credentials: "same-origin",
      headers,
      body: JSON.stringify(bodyObj),
    });

    let rawJson: unknown = null;
    try {
      rawJson = await resp.json();
    } catch {
      rawJson = null;
    }

    if (!resp.ok) {
      const msg =
        rawJson &&
        typeof rawJson === "object" &&
        "error" in rawJson &&
        typeof (rawJson as { error?: unknown }).error === "string"
          ? (rawJson as { error: string }).error
          : `Request failed (HTTP ${resp.status})`;
      return { ok: false, error: msg };
    }

    if (
      rawJson &&
      typeof rawJson === "object" &&
      "ok" in rawJson &&
      (rawJson as { ok?: unknown }).ok === false &&
      "error" in rawJson &&
      typeof (rawJson as { error?: unknown }).error === "string"
    ) {
      return { ok: false, error: (rawJson as { error: string }).error };
    }

    const parsed = parser(rawJson);
    if (parsed === null) {
      return { ok: false, error: `Unexpected response from ${toolName}.` };
    }
    return { ok: true, data: parsed };
  } catch (err) {
    return { ok: false, error: formatCaughtError(err) };
  }
}
