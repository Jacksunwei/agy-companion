import {
  el,
  formatCompactTokens,
  formatLatencyMs,
  formatShortDateUtc,
  formatUsd,
  formatUsdCompact,
} from "./dom.ts";
import {
  estimateUsageSummaryCosts,
  formatModelLabel,
  formatProviderBadge,
  formatProviderFullName,
  isBuiltinProvider,
} from "./pricing.ts";
import {
  DEFAULT_CODEX_MODEL_PRESETS,
  DEFAULT_MODEL_PRESETS,
  type AppState,
  type UsageGroup,
  type UsageRange,
  type UsageUnit,
} from "./types.ts";

export interface UsageViewActions {
  onSelectRange: (range: UsageRange) => void;
  onSelectUnit: (unit: UsageUnit) => void;
  onSelectGroup: (group: UsageGroup) => void;
}

function setSegmentWidth(node: HTMLElement, value: number, total: number): void {
  const pct = total > 0 && Number.isFinite(value) && value > 0
    ? Math.max(0, Math.min(100, (value / total) * 100))
    : 0;
  node.style.width = `${pct.toFixed(1)}%`;
}

function setSegmentHeight(node: HTMLElement, value: number, total: number): void {
  const pct = total > 0 && Number.isFinite(value) && value > 0
    ? Math.max(0, Math.min(100, (value / total) * 100))
    : 0;
  node.style.height = `${pct.toFixed(1)}%`;
}

function formatBucketLabel(label: string, range: UsageRange): string {
  if (range === "1d") return label;
  return formatShortDateUtc(label);
}

function pickAxisLabels(
  labels: ReadonlyArray<string>,
  range: UsageRange,
): string[] {
  if (labels.length === 0) return [];
  if (labels.length <= 4) {
    return labels.map((l) => formatBucketLabel(l, range));
  }
  const indices = [
    0,
    Math.floor((labels.length - 1) / 3),
    Math.floor(((labels.length - 1) * 2) / 3),
    labels.length - 1,
  ];
  const seen = new Set<number>();
  const out: string[] = [];
  for (const idx of indices) {
    if (!seen.has(idx)) {
      seen.add(idx);
      out.push(formatBucketLabel(labels[idx], range));
    }
  }
  return out;
}

export function computeUsageSignature(state: AppState): string {
  return JSON.stringify([
    state.usageRange,
    state.usageUnit,
    state.usageGroup,
    state.usageLoading,
    state.usageError,
    state.usageSummary,
    state.snapshot?.supported_models ?? null,
    state.snapshot?.supported_codex_models ?? null,
  ]);
}

export function buildUsageView(
  state: AppState,
  actions: UsageViewActions,
): DocumentFragment {
  const frag = document.createDocumentFragment();

  // Filter row: Today | 7 days | 30 days  +  $ | Tokens
  const filters = el("div", "usage-filters");

  const rangeSeg = el("div", "pill-seg");
  rangeSeg.setAttribute("role", "group");
  rangeSeg.setAttribute("aria-label", "Usage time range");
  const ranges: ReadonlyArray<{ id: UsageRange; label: string }> = [
    { id: "1d", label: "Today" },
    { id: "7d", label: "7 days" },
    { id: "30d", label: "30 days" },
  ];
  for (const r of ranges) {
    const b = el("button", null, r.label);
    b.type = "button";
    b.setAttribute("data-usage-ctrl", `range:${r.id}`);
    b.setAttribute("aria-pressed", String(state.usageRange === r.id));
    b.addEventListener("click", () => actions.onSelectRange(r.id));
    rangeSeg.append(b);
  }

  const unitSeg = el("div", "pill-seg");
  unitSeg.setAttribute("role", "group");
  unitSeg.setAttribute("aria-label", "Display unit");
  const units: ReadonlyArray<{ id: UsageUnit; label: string }> = [
    { id: "usd", label: "$" },
    { id: "tok", label: "Tokens" },
  ];
  for (const u of units) {
    const b = el("button", null, u.label);
    b.type = "button";
    b.setAttribute("data-usage-ctrl", `unit:${u.id}`);
    b.setAttribute("aria-pressed", String(state.usageUnit === u.id));
    b.addEventListener("click", () => actions.onSelectUnit(u.id));
    unitSeg.append(b);
  }

  filters.append(rangeSeg, unitSeg);
  frag.append(filters);

  const summary = state.usageSummary;

  if (!summary && state.usageLoading) {
    const loadingCard = el("section", "card empty-card");
    loadingCard.append(
      el("h2", "empty-title", "Loading usage history\u2026"),
      el("div", "empty-sub", "Reading local usage metrics from SQLite."),
    );
    frag.append(loadingCard);
    return frag;
  }

  if (!summary && state.usageError) {
    const errCard = el("section", "card empty-card");
    errCard.append(
      el("h2", "empty-title", "Unable to load usage history"),
      el("div", "empty-sub", state.usageError),
    );
    frag.append(errCard);
    return frag;
  }

  if (!summary || summary.totals.requests <= 0) {
    const emptyCard = el("section", "card empty-card");
    const rangeDesc =
      state.usageRange === "1d"
        ? "today"
        : state.usageRange === "7d"
          ? "in the last 7 days"
          : "in the last 30 days";
    emptyCard.append(
      el("h2", "empty-title", "No usage recorded yet"),
      el(
        "div",
        "empty-sub",
        `No requests logged ${rangeDesc}. Requests routed through the local proxy \u2014 both built-in Antigravity models and your own API keys \u2014 appear here automatically.`,
      ),
    );
    frag.append(emptyCard);
    return frag;
  }

  const presets = [
    ...(state.snapshot && state.snapshot.supported_models.length > 0
      ? state.snapshot.supported_models
      : DEFAULT_MODEL_PRESETS),
    ...(state.snapshot && state.snapshot.supported_codex_models.length > 0
      ? state.snapshot.supported_codex_models
      : DEFAULT_CODEX_MODEL_PRESETS),
  ];
  const costs = estimateUsageSummaryCosts(summary);
  const totals = summary.totals;
  const totalInput = Math.max(0, totals.input_tokens);
  const totalCached = Math.min(totalInput, Math.max(0, totals.cached_tokens));
  const totalNewIn = Math.max(0, totalInput - totalCached);
  const totalOutput = Math.max(0, totals.output_tokens);
  const totalTokens = totalInput + totalOutput;
  const cachePct = totalInput > 0 ? Math.min(100, Math.round((totalCached / totalInput) * 100)) : 0;

  // 1. Summary + Stacked Bar Chart Card
  const sumCard = el("section", "card");
  sumCard.append(el("h2", "visually-hidden", "Usage summary"));

  const heroRow = el("div", "hero-row");
  const heroLeft = el("div");
  const heroValText =
    state.usageUnit === "usd"
      ? costs.hasPricedModels
        ? formatUsd(costs.totalUsd)
        : "\u2014"
      : formatCompactTokens(totalTokens);
  const heroSubText =
    state.usageUnit === "usd" ? "est. list price" : "total tokens";
  heroLeft.append(
    el("span", "hero-val num", heroValText),
    el("span", "hero-sub", heroSubText),
  );

  const sinceText =
    summary.range === "1d"
      ? "today"
      : `since ${formatShortDateUtc(summary.since_utc)}`;
  heroRow.append(heroLeft, el("span", "opt-sub", sinceText));
  sumCard.append(heroRow);

  const callsTxt = totals.requests === 1 ? "1 call" : `${totals.requests.toLocaleString("en-US")} calls`;
  const savingsSuffix =
    costs.hasPricedModels && costs.cacheSavingsUsd >= 0.01
      ? ` (${formatUsdCompact(costs.cacheSavingsUsd)} saved)`
      : "";
  const subLine = `${formatCompactTokens(totalInput)} tokens in \u00b7 ${formatCompactTokens(totalOutput)} out \u00b7 ${callsTxt} \u00b7 ${cachePct}% cached${savingsSuffix}`;
  sumCard.append(el("div", "bk-meta num", subLine));

  // Proportional bar
  const pBar = el("div", "prop-bar");
  const psCache = el("div", "prop-seg seg-cache");
  const psOut = el("div", "prop-seg seg-out");
  const psNew = el("div", "prop-seg seg-new");
  if (state.usageUnit === "usd" && costs.hasPricedModels && costs.totalUsd > 0) {
    setSegmentWidth(psCache, costs.cachedReadUsd, costs.totalUsd);
    setSegmentWidth(psOut, costs.outputUsd, costs.totalUsd);
    setSegmentWidth(psNew, costs.newInputUsd, costs.totalUsd);
  } else {
    const segTotal = totalCached + totalOutput + totalNewIn;
    setSegmentWidth(psCache, totalCached, segTotal);
    setSegmentWidth(psOut, totalOutput, segTotal);
    setSegmentWidth(psNew, totalNewIn, segTotal);
  }
  pBar.append(psCache, psOut, psNew);
  sumCard.append(pBar);

  const leg = el("div", "legend-row");
  for (const [cls, label] of [
    ["seg-cache", "read from cache"],
    ["seg-out", "output"],
    ["seg-new", "new input"],
  ] as const) {
    const item = el("span", "legend-item");
    item.append(el("span", `ldot ${cls}`), el("span", null, label));
    leg.append(item);
  }
  sumCard.append(leg);

  // Stacked Daily / Hourly Bar Chart
  if (summary.buckets.length > 0) {
    const useCostScale = state.usageUnit === "usd" && costs.hasPricedModels;
    const bucketMetrics = summary.buckets.map((b, idx) => {
      const bIn = Math.max(0, b.input_tokens);
      const bCached = Math.min(bIn, Math.max(0, b.cached_tokens));
      const bNew = Math.max(0, bIn - bCached);
      const bOut = Math.max(0, b.output_tokens);
      const bCost = costs.bucketCosts[idx];
      if (useCostScale && bCost) {
        return {
          bucket: b,
          total: bCost.totalUsd,
          segCache: bCost.cachedReadUsd,
          segOut: bCost.outputUsd,
          segNew: bCost.newInputUsd,
          displayVal: formatUsd(bCost.totalUsd),
        };
      }
      const bTok = bIn + bOut;
      return {
        bucket: b,
        total: bTok,
        segCache: bCached,
        segOut: bOut,
        segNew: bNew,
        displayVal: formatCompactTokens(bTok),
      };
    });

    let maxVal = 0;
    let peakIdx = -1;
    for (let i = 0; i < bucketMetrics.length; i += 1) {
      if (bucketMetrics[i].total > maxVal) {
        maxVal = bucketMetrics[i].total;
        peakIdx = i;
      }
    }

    const chHdr = el("div", "daily-chart-hdr");
    const chartLabel =
      summary.range === "1d"
        ? "Today"
        : summary.range === "7d"
          ? "Last 7 days"
          : "Last 30 days";
    const peakText =
      peakIdx >= 0 && maxVal > 0
        ? `Peak ${formatBucketLabel(bucketMetrics[peakIdx].bucket.label, summary.range)} \u00b7 ${bucketMetrics[peakIdx].displayVal}`
        : "";
    chHdr.append(el("span", null, chartLabel), el("span", "num", peakText));
    sumCard.append(chHdr);

    const barsWrap = el("div", "daily-bars");
    barsWrap.setAttribute("role", "img");
    barsWrap.setAttribute("aria-label", `${chartLabel} usage bar chart`);
    for (const m of bucketMetrics) {
      const col = el("div", "dbar-col");
      const stack = el("div", "dbar-stack");
      const heightPct = maxVal > 0 && m.total > 0 ? Math.max(3, (m.total / maxVal) * 100) : 0;
      stack.style.height = `${Math.min(100, heightPct).toFixed(1)}%`;
      if (m.total <= 0) {
        stack.classList.add("is-empty");
      }
      col.title = `${formatBucketLabel(m.bucket.label, summary.range)}: ${m.displayVal} (${m.bucket.requests} calls)`;

      const bCache = el("div", "seg-cache");
      const bOut = el("div", "seg-out");
      const bNew = el("div", "seg-new");
      const segTotal = m.segCache + m.segOut + m.segNew;
      setSegmentHeight(bCache, m.segCache, segTotal);
      setSegmentHeight(bOut, m.segOut, segTotal);
      setSegmentHeight(bNew, m.segNew, segTotal);
      stack.append(bCache, bOut, bNew);
      col.append(stack);
      barsWrap.append(col);
    }
    sumCard.append(barsWrap);

    const axisLabels = pickAxisLabels(
      summary.buckets.map((b) => b.label),
      summary.range,
    );
    if (axisLabels.length > 0) {
      const axis = el("div", "daily-axis num");
      for (const lbl of axisLabels) {
        axis.append(el("span", null, lbl));
      }
      sumCard.append(axis);
    }

    const dataTable = el("table", "visually-hidden");
    dataTable.append(el("caption", null, `${chartLabel} usage`));
    const thead = el("thead");
    const headRow = el("tr");
    for (const colTitle of ["Period", useCostScale ? "Estimated cost" : "Tokens", "Calls"]) {
      const th = el("th", null, colTitle);
      th.setAttribute("scope", "col");
      headRow.append(th);
    }
    thead.append(headRow);
    const tbody = el("tbody");
    for (const m of bucketMetrics) {
      const tr = el("tr");
      const labelCell = el("th", null, formatBucketLabel(m.bucket.label, summary.range));
      labelCell.setAttribute("scope", "row");
      const callLabel = m.bucket.requests === 1 ? "1 call" : `${m.bucket.requests} calls`;
      tr.append(labelCell, el("td", null, m.displayVal), el("td", null, callLabel));
      tbody.append(tr);
    }
    dataTable.append(thead, tbody);
    sumCard.append(dataTable);
  }

  frag.append(sumCard);

  // 2. Breakdown Card: Model | Provider
  const bkCard = el("section", "card");
  bkCard.append(el("h2", "visually-hidden", "Usage breakdown"));

  const bkSeg = el("div", "pill-seg");
  bkSeg.setAttribute("role", "group");
  bkSeg.setAttribute("aria-label", "Breakdown grouping");
  for (const g of [
    { id: "model" as const, label: "Model" },
    { id: "provider" as const, label: "Provider" },
  ]) {
    const b = el("button", null, g.label);
    b.type = "button";
    b.setAttribute("data-usage-ctrl", `group:${g.id}`);
    b.setAttribute("aria-pressed", String(state.usageGroup === g.id));
    b.addEventListener("click", () => actions.onSelectGroup(g.id));
    bkSeg.append(b);
  }
  bkCard.append(bkSeg);

  const bkList = el("div", "breakdown-list");
  if (state.usageGroup === "model") {
    const maxTok = Math.max(
      1,
      ...summary.by_model.map((r) => Math.max(0, r.input_tokens) + Math.max(0, r.output_tokens)),
    );
    let maxCost = 0;
    for (const r of summary.by_model) {
      const id = `${r.provider}:${r.source ?? ""}:${r.key}`;
      const c = costs.byModelKey.get(id);
      if (c && c.totalUsd > maxCost) maxCost = c.totalUsd;
    }

    for (const row of summary.by_model) {
      const id = `${row.provider}:${row.source ?? ""}:${row.key}`;
      const c = costs.byModelKey.get(id) ?? null;
      const rowTok = Math.max(0, row.input_tokens) + Math.max(0, row.output_tokens);
      const barPct =
        state.usageUnit === "usd" && maxCost > 0 && c
          ? Math.round((c.totalUsd / maxCost) * 100)
          : Math.round((rowTok / maxTok) * 100);
      const clampedPct = Math.max(0, Math.min(100, barPct));

      const rightVal =
        state.usageUnit === "usd"
          ? c
            ? formatUsd(c.totalUsd)
            : "\u2014"
          : formatCompactTokens(rowTok);

      const rCachePct =
        row.input_tokens > 0
          ? Math.min(100, Math.round((row.cached_tokens / row.input_tokens) * 100))
          : 0;
      const rCalls = row.requests === 1 ? "1 call" : `${row.requests} calls`;
      const meta = `${rCalls} \u00b7 ${rCachePct}% cached \u00b7 avg ${formatLatencyMs(row.avg_latency_ms)}`;

      const item = el("div");
      const top = el("div", "bk-row-top");
      const nameWrap = el("span", "bk-name");
      const badgeCls = isBuiltinProvider(row.provider, row.source)
        ? "tag"
        : "tag tag-brand";
      nameWrap.append(
        el("span", badgeCls, formatProviderBadge(row.provider, row.source)),
        el("span", "bk-name-text", formatModelLabel(row.key, presets)),
      );
      top.append(nameWrap, el("span", "bk-val num", rightVal));

      const bbar = el("div", "bk-bar");
      const fill = el("div", "bk-bar-fill");
      fill.style.width = `${clampedPct}%`;
      bbar.append(fill);

      item.append(top, bbar, el("div", "bk-meta num", meta));
      bkList.append(item);
    }
  } else {
    const maxTok = Math.max(
      1,
      ...summary.by_provider.map((r) => Math.max(0, r.input_tokens) + Math.max(0, r.output_tokens)),
    );
    let maxCost = 0;
    for (const r of summary.by_provider) {
      const pc = costs.byProviderKey.get(r.provider) ?? null;
      if (pc !== null && pc > maxCost) maxCost = pc;
    }

    for (const row of summary.by_provider) {
      const pc = costs.byProviderKey.get(row.provider) ?? null;
      const rowTok = Math.max(0, row.input_tokens) + Math.max(0, row.output_tokens);
      const barPct =
        state.usageUnit === "usd" && maxCost > 0 && pc !== null
          ? Math.round((pc / maxCost) * 100)
          : Math.round((rowTok / maxTok) * 100);
      const clampedPct = Math.max(0, Math.min(100, barPct));

      const rightVal =
        state.usageUnit === "usd"
          ? pc !== null
            ? formatUsd(pc)
            : "\u2014"
          : formatCompactTokens(rowTok);

      const rCachePct =
        row.input_tokens > 0
          ? Math.min(100, Math.round((row.cached_tokens / row.input_tokens) * 100))
          : 0;
      const rCalls = row.requests === 1 ? "1 call" : `${row.requests} calls`;
      const modelCount = row.model_count ?? 0;
      const modelsTxt = modelCount === 1 ? "1 model" : `${modelCount} models`;
      const meta = `${rCalls} \u00b7 ${modelsTxt} \u00b7 ${rCachePct}% cached \u00b7 avg ${formatLatencyMs(row.avg_latency_ms)}`;

      const item = el("div");
      const top = el("div", "bk-row-top");
      top.append(
        el("span", "bk-name", formatProviderFullName(row.key, row.source)),
        el("span", "bk-val num", rightVal),
      );

      const bbar = el("div", "bk-bar");
      const fill = el("div", "bk-bar-fill");
      fill.style.width = `${clampedPct}%`;
      bbar.append(fill);

      item.append(top, bbar, el("div", "bk-meta num", meta));
      bkList.append(item);
    }
  }

  bkCard.append(bkList);
  frag.append(bkCard);
  return frag;
}
