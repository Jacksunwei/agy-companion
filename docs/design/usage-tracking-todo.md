# Usage & Cost Tracking Shim (TODO)

> **Status:** Deferred post-Milestone 1 (day-1 seam included in M1).
> **Reference point:** `npx ccusage` (local JSONL log aggregation), tailored to `agy-companion`'s multi-provider proxy and optional Antigravity local-data inspection.

## Day-1 Seam (Implemented in Milestone 1)

Every request handled by `companion-proxy` emits a non-blocking `UsageEvent` into a `UsageSink` trait defined in `crates/companion-core`:

```rust
pub struct UsageEvent {
    pub timestamp_utc: String,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub latency_ms: u64,
    pub source: String, // e.g., "proxy"
}
```

- Default M1 sink: append-only JSONL under `~/.local/share/agy-companion/usage/YYYY-MM.jsonl` (mode `0600`).
- Zero prompt or response body text is logged — token counts and metadata only.

## Deferred Scope (Design Later)

1. `agy-companion usage` CLI subcommand (daily/monthly/model/provider rollups, cache savings breakdown).
2. Versioned pricing table (input, output, cached-input discount, thinking tokens) with offline defaults and optional refresh.
3. Optional read-only adapter to inspect Antigravity's locally stored session/usage data (if documented and stable) alongside proxy JSONL records.
4. Usage & Cost tab inside the `agy-companion` MCP App (`global` full-page view).
