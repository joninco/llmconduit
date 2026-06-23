/**
 * AttemptTrace (gap 11) — the per-flow FAILOVER / attempt stepper for the FlowDetail inspector
 * header. Renders the gap-03 `attempts[]` as an ordered chain of dispatch nodes:
 *
 *   A failed · 503 · 0.8s   →   B served · 1.2s
 *
 * answering "which provider failed, why, how long did we wait, and what served?" at a glance. The
 * SERVED node is visually distinct (a healthy-mint accent + a `served` tag); a FAILED node carries
 * its bounded taxonomic `error_class` + `failover_reason`. Each node is expandable to its full
 * per-attempt detail (provider, model, status/error_class, duration, first upstream byte (wire
 * TTFB), failover_reason).
 *
 * Design language: matches the inspector's other header surfaces (the gap-10 `LatencyBreakdown`,
 * the gap-09 `ContextGauge`) — `tabular-nums`, the shared status tokens, an instrument feel, a
 * `data-quality` provenance attribute on every figure. Reuses the Night Watch status traffic-lights
 * (healthy mint = served, down red = failed) per `DESIGN_NOTES.md`.
 *
 * Data quality (consumes `attemptTrace`, never fabricates):
 *  - a per-attempt time whose endpoint is unmeasured renders `—` (UNAVAILABLE), NEVER `0` — a real
 *    `0ms` (measured) is DISTINCT and reads `0ms` (spec 11 acceptance).
 *  - the whole trace (provider/model/status/error_class/failover_reason) is `measured` (from
 *    `attempts[]`); a clock-disordered attempt is FLAGGED (`skew`), never rendered negative.
 *  - a SINGLE-attempt flow renders a single node + no "failover" header (no fake chain); ≥ 2 → the
 *    chain. An empty/absent trace renders nothing (the caller gates it off entirely).
 */
import { useState } from 'react';
import type {
  AttemptErrorClass,
  AttemptFailoverReason,
} from '../../api/types';
import type { AttemptNode, AttemptTrace as AttemptTraceModel, ByteFigure } from './attemptTrace';
import { fmtElapsed } from '../FlowTable/format';
import { cn } from '../../lib/cn';

/** Human-readable label for a bounded taxonomic `error_class` (spec 03 enum, snake_case wire). */
const ERROR_CLASS_LABEL: Record<AttemptErrorClass, string> = {
  connect: 'connect',
  http_status: 'http status',
  timeout: 'timeout',
  stream: 'stream',
  terminal: 'terminal',
  other: 'other',
};

/** Human-readable label for a bounded taxonomic `failover_reason` (spec 03 enum). */
const FAILOVER_REASON_LABEL: Record<AttemptFailoverReason, string> = {
  provider_failed: 'provider failed → failover',
  terminal_no_failover: 'terminal — no failover',
};

/** A short status pill: served = healthy mint, failed = down red (Night Watch traffic-lights). */
function StatusPill({ node }: { node: AttemptNode }) {
  const served = node.isServed;
  return (
    <span
      className={cn(
        'rounded-sm px-1 py-0.5 text-[9px] font-medium uppercase tracking-wide',
        served ? 'bg-status-healthy/15 text-status-healthy' : 'bg-status-down/15 text-status-down',
      )}
      data-testid={`attempt-status-${node.index}`}
      data-status={node.status}
    >
      {node.status}
    </span>
  );
}

/** Format a per-attempt time figure: `—` when unavailable (never `0`), else the measured ms. */
function fmtByte(fig: ByteFigure): string {
  return fig.quality === 'unavailable' ? '—' : fmtElapsed(fig.valueMs);
}

/** One node of the stepper: the headline summary + an expandable per-attempt detail block. */
function AttemptNodeCard({ node }: { node: AttemptNode }) {
  const [open, setOpen] = useState(false);
  const served = node.isServed;
  const provider = node.provider ?? '—';
  // The headline duration: `—` when unavailable (an endpoint was unmeasured), never a fabricated 0.
  const durationText = node.durationQuality === 'unavailable' ? '—' : fmtElapsed(node.durationMs);
  // The failure summary on the headline (error class) — only on a failed node.
  const errorText = node.errorClass ? ERROR_CLASS_LABEL[node.errorClass] : null;

  return (
    <div
      className={cn(
        'flex min-w-[7rem] flex-col gap-0.5 rounded-md border px-2 py-1 text-left transition-colors',
        served ? 'border-status-healthy/40 bg-status-healthy/5' : 'border-status-down/30 bg-status-down/5',
      )}
      data-testid={`attempt-node-${node.index}`}
      data-status={node.status}
      data-served={served ? 'true' : 'false'}
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5"
        aria-expanded={open}
        data-testid={`attempt-toggle-${node.index}`}
        title={`attempt ${node.step}: ${provider}${node.model ? ` · ${node.model}` : ''}`}
      >
        <span className="font-mono text-[10px] uppercase tracking-wide text-text-muted">{node.step}</span>
        <StatusPill node={node} />
      </button>
      {/* Headline: provider + (error class for a failure) + duration. */}
      <div className="flex items-baseline gap-1">
        <span className="truncate font-mono text-[11px] text-text" title={provider}>{provider}</span>
      </div>
      <div className="flex items-center gap-1 text-[10px]">
        {errorText && (
          <span className="text-status-down" data-testid={`attempt-error-${node.index}`} title={`error class: ${errorText}`}>
            {errorText}
          </span>
        )}
        {errorText && <span className="text-line">·</span>}
        {/* Duration: a measured `0ms` reads `0ms`; an unmeasured endpoint reads `—` (never `0`). */}
        <span
          className={cn('tabular-nums', node.durationQuality === 'unavailable' ? 'text-text-muted' : 'text-text')}
          data-testid={`attempt-duration-${node.index}`}
          data-quality={node.durationQuality}
          title={node.durationQuality === 'unavailable'
            ? 'attempt duration unavailable — an endpoint was unmeasured (— , not 0)'
            : 'attempt wall-clock: start → resolve (measured)'}
        >
          {durationText}
        </span>
        {node.disordered && (
          <span
            className="text-status-cooling"
            data-testid={`attempt-skew-${node.index}`}
            title="clock skew — attempt endpoints out of order, clamped to 0 (not negative)"
          >
            skew
          </span>
        )}
      </div>

      {/* Expandable per-attempt detail (spec 11): provider/model, status/error_class, duration,
          first upstream byte (wire TTFB), failover_reason. */}
      {open && (
        <dl
          className="mt-1 grid grid-cols-[auto_1fr] gap-x-2 gap-y-0.5 border-t border-line/60 pt-1 text-[10px]"
          data-testid={`attempt-detail-${node.index}`}
        >
          <dt className="text-text-muted">model</dt>
          <dd className="truncate font-mono text-text" title={node.model ?? undefined}>{node.model ?? '—'}</dd>

          <dt className="text-text-muted">first byte</dt>
          {/* Per-attempt wire TTFB: `—` when no header ever arrived (never a fabricated `0`). A
              first byte observed BEFORE the attempt start is clamped to 0 + FLAGGED `skew` (so a
              disordered `0` stays distinct from a real measured `0ms`). */}
          <dd
            className={cn('flex items-center gap-1 tabular-nums', node.firstByte.quality === 'unavailable' ? 'text-text-muted' : 'text-text')}
            data-testid={`attempt-firstbyte-${node.index}`}
            data-quality={node.firstByte.quality}
            title={node.firstByte.detail}
          >
            <span>{fmtByte(node.firstByte)}</span>
            {node.firstByte.disordered && (
              <span
                className="text-status-cooling"
                data-testid={`attempt-firstbyte-skew-${node.index}`}
                title="clock skew — first byte before the attempt start, clamped to 0 (not a real 0ms)"
              >
                skew
              </span>
            )}
          </dd>

          {/* failover_reason — only meaningful on a failed node. */}
          {node.failoverReason && (
            <>
              <dt className="text-text-muted">failover</dt>
              <dd className="text-text" data-testid={`attempt-failover-${node.index}`} title="failover reason (taxonomic)">
                {FAILOVER_REASON_LABEL[node.failoverReason]}
              </dd>
            </>
          )}
        </dl>
      )}
    </div>
  );
}

/** The connector arrow between two stepper nodes (a failed attempt → the next try). */
function StepArrow() {
  return (
    <span className="shrink-0 self-center px-0.5 text-sm text-text-muted" aria-hidden="true" data-testid="attempt-arrow">
      →
    </span>
  );
}

export function AttemptTrace({ model }: { model: AttemptTraceModel }) {
  // The caller gates rendering off when there is no trace, but stay defensive (render nothing).
  if (!model.hasTrace) return null;

  return (
    <div className="flex flex-col gap-1" data-testid="attempt-trace" data-failover={model.isFailover ? 'true' : 'false'}>
      {/* A one-line summary: a real failover names the failed→served handoff; a single attempt
          does NOT claim a failover (spec 11: no fake failover for a single-attempt flow). */}
      <div className="text-[10px] uppercase tracking-wide text-text-muted" data-testid="attempt-trace-summary">
        {model.isFailover ? (
          <span data-testid="attempt-failover-label">
            failover · {model.failedCount} failed
            {model.servedIndex !== null
              ? ` → ${model.nodes[model.servedIndex]!.step} served`
              : ' → none served'}
          </span>
        ) : (
          <span data-testid="attempt-single-label">single attempt — no failover</span>
        )}
      </div>

      {/* The stepper chain: node → node → node. Each KNOWN attempt is a node; a `→` separates them. */}
      <div className="flex flex-wrap items-stretch gap-1" data-testid="attempt-stepper" role="list" aria-label="failover attempt trace">
        {model.nodes.map((node, i) => (
          <div key={node.index} className="flex items-stretch" role="listitem">
            {i > 0 && <StepArrow />}
            <AttemptNodeCard node={node} />
          </div>
        ))}
      </div>
    </div>
  );
}
