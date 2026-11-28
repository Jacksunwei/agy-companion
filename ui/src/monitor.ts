import {
  createIcon,
  el,
  formatCompactTokens,
  formatTimeHHMM,
  formatUsd,
  quitReopenCapitalized,
  type TooltipController,
} from "./dom.ts";
import {
  estimateSessionSummary,
  estimateTokensCost,
  formatModelLabel,
  formatProviderBadge,
  getModelContextWindow,
  isBuiltinProvider,
  sessionModelKey,
} from "./pricing.ts";
import {
  DEFAULT_CODEX_MODEL_PRESETS,
  DEFAULT_MODEL_PRESETS,
  type AppState,
} from "./types.ts";

function setSegmentWidth(node: HTMLElement, value: number, total: number): void {
  const pct = total > 0 && Number.isFinite(value) && value > 0
    ? Math.max(0, Math.min(100, (value / total) * 100))
    : 0;
  node.style.width = `${pct.toFixed(1)}%`;
}

export function computeMonitorSignature(state: AppState): string {
  const snap = state.snapshot;
  const stats = snap?.stats ?? null;
  return JSON.stringify([
    snap?.platform ?? "macos",
    snap?.ls_telemetry_active === true,
    snap?.launcher_integration?.hybrid_active === true,
    snap?.supported_models ?? null,
    snap?.supported_codex_models ?? null,
    stats
      ? {
          usage: stats.usage,
          max_context_tokens: stats.max_context_tokens,
          recent_calls: stats.recent_calls,
          by_model: stats.by_model,
        }
      : null,
  ]);
}

function formatFullWindow(raw: number): string {
  const n = Number.isFinite(raw) && raw > 0 ? Math.round(raw) : 0;
  if (n >= 1_000_000) {
    return `${(n / 1_000_000).toFixed(2).replace(/\.?0+$/, "")}M`;
  }
  return formatCompactTokens(n);
}

export function buildMonitorView(
  state: AppState,
  tooltip?: TooltipController,
): DocumentFragment {
  const frag = document.createDocumentFragment();
  const snap = state.snapshot;
  const platform = snap?.platform ?? "macos";
  const lsTelemetryActive = snap?.ls_telemetry_active === true;
  const hybridActive = snap?.launcher_integration?.hybrid_active === true;
  const telemetryConnected = lsTelemetryActive || hybridActive;
  const stats = snap?.stats ?? null;
  const usage = stats?.usage ?? null;
  const requests = usage?.requests ?? 0;

  if (!usage || requests <= 0) {
    const empty = el("section", "card empty-card");
    const title = telemetryConnected
      ? "Waiting for requests"
      : "Waiting for Antigravity to connect";
    const body = telemetryConnected
      ? "Send a message in Antigravity to see live context usage, cache hit health, and cost estimates \u2014 works for built-in models as well as your own API keys."
      : `${quitReopenCapitalized(platform)} so requests route through the local proxy. Live monitoring works for built-in Antigravity models even without an API key.`;

    empty.append(el("h2", "empty-title", title), el("div", "empty-sub", body));
    frag.append(empty);
    return frag;
  }

  const presets = [
    ...(snap && snap.supported_models.length > 0 ? snap.supported_models : DEFAULT_MODEL_PRESETS),
    ...(snap && snap.supported_codex_models.length > 0
      ? snap.supported_codex_models
      : DEFAULT_CODEX_MODEL_PRESETS),
  ];
  const recentCalls = stats?.recent_calls ?? [];
  const byModel = stats?.by_model ?? [];
  const latest = recentCalls.length > 0 ? recentCalls[recentCalls.length - 1] : null;
  const topModel = byModel.length > 0 ? byModel[0] : null;

  // 1. Live Session Card
  const card = el("section", "card");

  const activeProvider = latest?.provider ?? topModel?.provider ?? "antigravity";
  const activeSource = latest?.source ?? topModel?.source ?? "builtin";
  const activeModelId = latest?.model ?? topModel?.model ?? "antigravity-builtin";
  const activeModelDisplayName =
    byModel.find(
      (m) =>
        m.provider === activeProvider &&
        m.source === activeSource &&
        m.model === activeModelId,
    )?.model_display_name ?? null;
  const activeModelLabel = formatModelLabel(activeModelId, presets, activeModelDisplayName);
  const providerBadgeText = formatProviderBadge(activeProvider, activeSource);
  const badgeClass = isBuiltinProvider(activeProvider, activeSource)
    ? "tag"
    : "tag tag-brand";

  const mHdr = el("div", "mon-model-hdr");
  mHdr.append(
    el("span", badgeClass, providerBadgeText),
    el("h2", "mon-model-name", activeModelLabel),
  );
  const latestTime = latest ? formatTimeHHMM(latest.timestamp_ms) : "";
  if (latestTime) {
    mHdr.append(el("span", "mon-time mono num", latestTime));
  }
  card.append(mHdr);

  // Latest prompt context & cache breakdown bar
  const latestInput = latest ? Math.max(0, latest.input_tokens) : Math.max(0, usage.prompt_tokens);
  const latestCached = latest
    ? Math.min(latestInput, Math.max(0, latest.cached_tokens))
    : Math.min(latestInput, Math.max(0, usage.cached_tokens));
  const latestNew = Math.max(0, latestInput - latestCached);
  const latestOutput = latest
    ? Math.max(0, latest.output_tokens)
    : Math.max(0, usage.output_tokens);
  const liveMaxCtx =
    stats?.max_context_tokens != null &&
    Number.isFinite(stats.max_context_tokens) &&
    stats.max_context_tokens > 0
      ? stats.max_context_tokens
      : null;
  const modelMaxCtx = getModelContextWindow(activeModelId);
  const maxCtx = liveMaxCtx ?? modelMaxCtx;
  const ctxPct = maxCtx > 0 ? Math.min(100, Math.round((latestInput / maxCtx) * 100)) : 0;

  const ctxLbl = el("div", "tile-k", latest ? "Latest prompt" : "Session prompt");
  const heroRow = el("div", "hero-row");
  const heroLeft = el("div", "hero-left");
  heroLeft.append(
    el("span", "hero-val num", formatCompactTokens(latestInput)),
    el("span", "hero-sub num", `of ${formatCompactTokens(maxCtx)}`),
  );
  if (liveMaxCtx !== null && liveMaxCtx < modelMaxCtx) {
    const infoBtn = el("button", "ctx-info-btn");
    infoBtn.type = "button";
    infoBtn.setAttribute(
      "aria-label",
      "Why the compaction window is smaller than the full context window",
    );
    infoBtn.append(createIcon("info"));
    if (tooltip) {
      const tipContent = {
        title: "Compaction window",
        badge: `${formatCompactTokens(liveMaxCtx)} / ${formatFullWindow(modelMaxCtx)}`,
        desc: "Compacts early to reserve room for max output, turn overflow, and the summary prompt.",
      };
      infoBtn.addEventListener("mouseenter", () => tooltip.showInfo(infoBtn, tipContent));
      infoBtn.addEventListener("mouseleave", () => tooltip.scheduleHide());
      infoBtn.addEventListener("focus", () => tooltip.showInfo(infoBtn, tipContent));
      infoBtn.addEventListener("blur", () => tooltip.hideNow());
    }
    heroLeft.append(infoBtn);
  }
  heroRow.append(heroLeft, el("span", "hero-pct num", `${ctxPct}%`));
  card.append(ctxLbl, heroRow);

  const latestBarTotal = latestCached + latestNew + latestOutput;
  const barDenom = maxCtx > 0 ? Math.max(maxCtx, latestBarTotal) : latestBarTotal;
  const bar = el("div", "prop-bar");
  const sCache = el("div", "prop-seg seg-cache");
  const sNew = el("div", "prop-seg seg-new");
  const sOut = el("div", "prop-seg seg-out");
  setSegmentWidth(sCache, latestCached, barDenom);
  setSegmentWidth(sNew, latestNew, barDenom);
  setSegmentWidth(sOut, latestOutput, barDenom);
  bar.append(sCache, sNew, sOut);
  card.append(bar);

  const legend = el("div", "legend-row num");
  const addLeg = (cls: string, text: string) => {
    const item = el("span", "legend-item");
    item.append(el("span", `ldot ${cls}`), el("span", null, text));
    legend.append(item);
  };
  addLeg("seg-cache", `${formatCompactTokens(latestCached)} from cache`);
  addLeg("seg-new", `${formatCompactTokens(latestNew)} new`);
  addLeg("seg-out", `${formatCompactTokens(latestOutput)} output`);
  card.append(legend);

  // Call health dot strip (chronological oldest -> newest; cold_start vs cache_break)
  if (recentCalls.length > 0) {
    const stripRow = el("div", "call-strip-row");
    const callCountLabel =
      recentCalls.length === 1 ? "Last 1 call" : `Last ${recentCalls.length} calls`;
    stripRow.append(el("span", "num", callCountLabel));

    const dots = el("div", "call-dots");
    dots.setAttribute("aria-hidden", "true");
    let hitCount = 0;
    let breakCount = 0;
    let coldCount = 0;
    for (const call of recentCalls) {
      const isAmber = call.cold_start || call.cache_break;
      const dot = el("span", isAmber ? "cdot break" : "cdot");
      if (call.cache_break) {
        breakCount += 1;
        dot.title = "Cache break";
      } else if (call.cold_start) {
        coldCount += 1;
        dot.title = "Cold prompt";
      } else {
        hitCount += 1;
        dot.title = "Cache hit";
      }
      dots.append(dot);
    }

    const fmtCount = (n: number, singular: string, plural: string) =>
      `${n} ${n === 1 ? singular : plural}`;
    const dotSummary = el(
      "span",
      "visually-hidden",
      `${fmtCount(hitCount, "cache hit", "cache hits")}, ${fmtCount(breakCount, "cache break", "cache breaks")}, ${fmtCount(coldCount, "cold prompt", "cold prompts")}`,
    );

    const breakText =
      breakCount === 0
        ? "no breaks"
        : breakCount === 1
          ? "1 break"
          : `${breakCount} breaks`;
    stripRow.append(dots, dotSummary, el("span", "num", breakText));
    card.append(stripRow);
  }

  // 3 recessed borderless tiles (Session cache | Input | Output)
  const overallCachePct =
    usage.prompt_tokens > 0
      ? Math.min(100, Math.round((usage.cached_tokens / usage.prompt_tokens) * 100))
      : Math.min(100, Math.round(Math.max(0, usage.cache_hit_ratio) * 100));
  const latestCachePct =
    latestInput > 0 ? Math.min(100, Math.round((latestCached / latestInput) * 100)) : 0;

  const tiles = el("div", "tile-grid");
  const addTile = (k: string, v: string, s: string) => {
    const t = el("div", "tile");
    t.append(
      el("div", "tile-k", k),
      el("div", "tile-v num", v),
      el("div", "tile-s num", s),
    );
    tiles.append(t);
  };
  addTile("Session cache", `${overallCachePct}%`, `latest ${latestCachePct}%`);
  addTile(
    "Input",
    formatCompactTokens(usage.prompt_tokens),
    requests === 1 ? "1 call" : `${requests} calls`,
  );
  const thinkSub =
    usage.reasoning_tokens > 0
      ? `incl. ${formatCompactTokens(usage.reasoning_tokens)} think`
      : "0 think";
  addTile("Output", formatCompactTokens(usage.output_tokens), thinkSub);
  card.append(tiles);

  // Session list price summary bar
  const sessionCosts = estimateSessionSummary(byModel);
  const costFoot = el("div", "cost-foot");
  const costHdr = el("div", "cost-foot-hdr");
  costHdr.append(
    el("span", null, "This session \u00b7 est. list price"),
    el(
      "span",
      "cost-foot-val num",
      sessionCosts.hasPricedModels ? formatUsd(sessionCosts.totalUsd) : "\u2014",
    ),
  );
  const costBar = el("div", "prop-bar");
  const cCache = el("div", "prop-seg seg-cache");
  const cOut = el("div", "prop-seg seg-out");
  const cNew = el("div", "prop-seg seg-new");
  if (sessionCosts.hasPricedModels && sessionCosts.totalUsd > 0) {
    setSegmentWidth(cCache, sessionCosts.cachedReadUsd, sessionCosts.totalUsd);
    setSegmentWidth(cOut, sessionCosts.outputUsd, sessionCosts.totalUsd);
    setSegmentWidth(cNew, sessionCosts.newInputUsd, sessionCosts.totalUsd);
  } else {
    const totalNewIn = Math.max(0, usage.prompt_tokens - usage.cached_tokens);
    const totalTok = usage.cached_tokens + usage.output_tokens + totalNewIn;
    setSegmentWidth(cCache, usage.cached_tokens, totalTok);
    setSegmentWidth(cOut, usage.output_tokens, totalTok);
    setSegmentWidth(cNew, totalNewIn, totalTok);
  }
  costBar.append(cCache, cOut, cNew);
  costFoot.append(costHdr, costBar);
  card.append(costFoot);
  frag.append(card);

  // 2. Usage by model card (current session)
  if (byModel.length > 0) {
    const byModelCard = el("section", "card");
    const bmHdr = el("div", "sec-h");
    const hasAnySubagents = byModel.some((r) => (r.subagent?.requests ?? 0) > 0);
    if (hasAnySubagents) {
      const bmLegend = el("span", "sec-right bk-legend");
      const mainLeg = el("span", "bk-legend-item");
      const mainDot = el("span", "bk-legend-dot is-main");
      mainDot.setAttribute("aria-hidden", "true");
      mainLeg.append(mainDot, el("span", null, "Main"));

      const subLeg = el("span", "bk-legend-item");
      const subDot = el("span", "bk-legend-dot is-sub");
      subDot.setAttribute("aria-hidden", "true");
      subLeg.append(subDot, el("span", null, "Sub-agents"));

      bmLegend.append(mainLeg, subLeg);
      bmHdr.append(el("h2", "sec-t", "Usage by model"), bmLegend);
    } else {
      bmHdr.append(
        el("h2", "sec-t", "Usage by model"),
        el("span", "sec-right sec-count", "this session"),
      );
    }
    byModelCard.append(bmHdr);

    const totalSessionTokens = byModel.reduce(
      (acc, r) => acc + Math.max(0, r.input_tokens) + Math.max(0, r.output_tokens),
      0,
    );

    const fmtCalls = (n: number): string => (n === 1 ? "1 call" : `${n} calls`);

    const list = el("div", "breakdown-list");
    for (const row of byModel) {
      const key = sessionModelKey(row);
      const cost = sessionCosts.byModelKey.get(key) ?? null;
      const rowTokens = Math.max(0, row.input_tokens) + Math.max(0, row.output_tokens);

      let pct = 0;
      if (sessionCosts.hasPricedModels && sessionCosts.totalUsd > 0 && cost) {
        pct = Math.round((cost.totalUsd / sessionCosts.totalUsd) * 100);
      } else if (totalSessionTokens > 0) {
        pct = Math.round((rowTokens / totalSessionTokens) * 100);
      }
      const clampedPct = Math.max(0, Math.min(100, pct));

      const rightVal = cost
        ? `${formatUsd(cost.totalUsd)} \u00b7 ${clampedPct}%`
        : `${formatCompactTokens(rowTokens)} \u00b7 ${clampedPct}%`;

      const sub = row.subagent && row.subagent.requests > 0 ? row.subagent : null;
      const subRequests = sub ? Math.min(row.requests, Math.max(0, sub.requests)) : 0;
      const mainRequests = Math.max(0, row.requests - subRequests);

      const rowCachePct =
        row.input_tokens > 0
          ? Math.min(100, Math.round((row.cached_tokens / row.input_tokens) * 100))
          : 0;
      let callsTxt: string;
      if (!sub) {
        callsTxt = fmtCalls(row.requests);
      } else if (mainRequests === 0) {
        callsTxt =
          subRequests === 1 ? "1 sub-agent call" : `${subRequests} sub-agent calls`;
      } else {
        const subPart =
          subRequests === 1 ? "1 sub-agent" : `${subRequests} sub-agents`;
        callsTxt = `${row.requests} calls (${mainRequests} main \u00b7 ${subPart})`;
      }
      const metaText = `${callsTxt} \u00b7 ${formatCompactTokens(row.input_tokens)} in (${rowCachePct}% cache) \u00b7 ${formatCompactTokens(row.output_tokens)} out`;

      const item = el("div");
      const top = el("div", "bk-row-top");
      const left = el("span", "bk-name");
      const rowBadgeCls = isBuiltinProvider(row.provider, row.source)
        ? "tag"
        : "tag tag-brand";
      left.append(
        el("span", rowBadgeCls, formatProviderBadge(row.provider, row.source)),
        el("span", "bk-name-text", formatModelLabel(row.model, presets, row.model_display_name)),
      );
      top.append(left, el("span", "bk-val num", rightVal));

      const bbar = el("div", "bk-bar");
      if (!sub) {
        const fill = el("div", "bk-bar-fill");
        fill.style.width = `${clampedPct}%`;
        fill.title = cost
          ? `Main: ${formatUsd(cost.totalUsd)} (${fmtCalls(row.requests)})`
          : `Main: ${formatCompactTokens(rowTokens)} tokens (${fmtCalls(row.requests)})`;
        bbar.append(fill);
      } else {
        const subTokens = Math.min(
          rowTokens,
          Math.max(0, sub.input_tokens) + Math.max(0, sub.output_tokens),
        );
        const mainTokens = Math.max(0, rowTokens - subTokens);
        const subCost = cost
          ? estimateTokensCost(row.model, {
              inputTokens: sub.input_tokens,
              cachedTokens: sub.cached_tokens,
              outputTokens: sub.output_tokens,
              reasoningTokens: sub.reasoning_tokens,
            })
          : null;
        const subUsd = cost ? Math.min(cost.totalUsd, Math.max(0, subCost?.totalUsd ?? 0)) : 0;
        const mainUsd = cost ? Math.max(0, cost.totalUsd - subUsd) : 0;

        if (mainRequests === 0) {
          const subFill = el("div", "bk-bar-sub");
          subFill.style.width = `${clampedPct}%`;
          subFill.title = cost
            ? `Sub-agents: ${formatUsd(cost.totalUsd)} (${fmtCalls(subRequests)})`
            : `Sub-agents: ${formatCompactTokens(rowTokens)} tokens (${fmtCalls(subRequests)})`;
          bbar.append(subFill);
        } else {
          let mainPct = 0;
          let subPct = 0;
          if (cost && cost.totalUsd > 0) {
            mainPct = (mainUsd / cost.totalUsd) * clampedPct;
            subPct = (subUsd / cost.totalUsd) * clampedPct;
          } else if (rowTokens > 0) {
            mainPct = (mainTokens / rowTokens) * clampedPct;
            subPct = (subTokens / rowTokens) * clampedPct;
          } else if (row.requests > 0) {
            mainPct = (mainRequests / row.requests) * clampedPct;
            subPct = (subRequests / row.requests) * clampedPct;
          }

          if (mainPct > 0) {
            const mainFill = el("div", "bk-bar-fill");
            mainFill.style.width = `${mainPct.toFixed(1)}%`;
            mainFill.title = cost
              ? `Main: ${formatUsd(mainUsd)} (${fmtCalls(mainRequests)})`
              : `Main: ${formatCompactTokens(mainTokens)} tokens (${fmtCalls(mainRequests)})`;
            bbar.append(mainFill);
          }
          if (mainPct > 0 && subPct > 0) {
            bbar.append(el("div", "bk-bar-gap"));
          }
          if (subPct > 0) {
            const subFill = el("div", "bk-bar-sub");
            subFill.style.width = `${subPct.toFixed(1)}%`;
            subFill.title = cost
              ? `Sub-agents: ${formatUsd(subUsd)} (${fmtCalls(subRequests)})`
              : `Sub-agents: ${formatCompactTokens(subTokens)} tokens (${fmtCalls(subRequests)})`;
            bbar.append(subFill);
          }
        }
      }

      item.append(top, bbar, el("div", "bk-meta num", metaText));
      list.append(item);
    }

    byModelCard.append(list);
    frag.append(byModelCard);
  }

  return frag;
}
