/**
 * ClientRollup (gap 15) — the AGGREGATE "by client" deep-dive for the flows screen. Answers the
 * spec-15 operator question "WHO is generating the cost, errors, latency — or abuse?" by rolling
 * cost / errors / latency up BY non-secret client (`client_label` + `client_source`, gap 04), over
 * the SAME filtered flow set the table shows.
 *
 * Pure render over `clientRollup(rows)` (the DOM-free model) — every figure carries its `data-quality`
 * provenance and the don't-lie-with-zeros rules live in the model:
 *  - SOURCE STRENGTH: a key-hash / configured-id client is a STRONG `measured` identity; a `user_agent`
 *    client is a WEAK `derived` fallback, rendered VISIBLY weaker (dimmed/italic + a `ua` badge) and
 *    LABELLED — never presented as a confirmed identity.
 *  - DON'T-LIE-WITH-ZEROS: an UNATTRIBUTED flow is never a fabricated client — it bumps the explicit
 *    "unattributed" count instead. A window with NO attributed flow is an EXPLICIT `unavailable` `—`
 *    state (NOT hidden, NOT `0`). A per-client cost/latency with no priced/timed flow reads `—`.
 *  - NEVER a raw secret: only the already-hashed `client_label` is shown (gap 04 guarantees only the
 *    hash prefix exists). This is the INTENDED diagnostic purpose — showing the key-hash to the
 *    auth-gated operator is NOT a credential leak.
 *
 * A client row CROSS-LINKS into the per-client filter (`flowFilterStore.setClient`) so "click here →
 * see those flows" scopes the table (+ this roll-up) to that one client. Collapsed by default (a dense
 * secondary surface under the table — mirrors CacheEconomics).
 *
 * SOURCE: the same observed flow population the FlowTable shows (`rows` from `useFlowRows(filters)`),
 * so filtering re-scopes the roll-up too.
 */
import { useMemo, useState } from 'react';
import type { FlowSummary } from '../../api/types';
import { clientRollup, fmtLatency, type ClientRollupRow } from './clientAttribution';
import { flowFilterStore } from '../../store/flowFilterStore';
import { fmtCost } from './format';
import { cn } from '../../lib/cn';

export function ClientRollup({ rows }: { rows: FlowSummary[] }) {
  const [open, setOpen] = useState(false);
  const model = useMemo(() => clientRollup(rows), [rows]);
  const setClient = flowFilterStore.getState().setClient;

  return (
    <section
      className="shrink-0 border-t border-line bg-panel"
      data-testid="client-rollup-panel"
      data-available={model.available ? 'true' : 'false'}
      aria-label="aggregate cost / errors / latency by client"
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] uppercase tracking-[0.14em] text-text-muted transition-colors hover:text-text"
        data-testid="client-rollup-toggle"
      >
        <span className={cn('inline-block transition-transform', open ? 'rotate-90' : '')} aria-hidden>
          ▸
        </span>
        <span>by client</span>
        <span className="ml-auto font-mono tabular-nums text-text-muted" data-testid="client-rollup-summary">
          {!model.available
            ? 'no attributed clients'
            : `${model.rows.length} client${model.rows.length === 1 ? '' : 's'}` +
              (model.unattributedFlows > 0 ? ` · ${model.unattributedFlows} unattributed` : '')}
        </span>
      </button>
      {open && (
        <div className="max-h-44 overflow-auto px-3 pb-2">
          {!model.available ? (
            // ZERO attributed flows ⇒ EXPLICIT unavailable `—` (NOT a blank, NOT a fabricated client).
            // The companion line states the honest reason: nothing carried an attribution.
            <div
              className="py-2 text-center text-xs italic text-text-muted"
              data-testid="client-rollup-unavailable"
              data-quality="unavailable"
            >
              No attributed clients in this window — client unavailable ({'—'}), not 0.
              {model.unattributedFlows > 0 && ` (${model.unattributedFlows}/${model.totalFlows} flows unattributed)`}
            </div>
          ) : (
            <table className="w-full text-xs" data-testid="client-rollup-table">
              <thead>
                <tr className="text-[10px] uppercase tracking-[0.12em] text-text-muted">
                  <th className="py-1 text-left font-normal">client</th>
                  <th className="py-1 text-right font-normal">flows</th>
                  <th className="py-1 text-right font-normal">err</th>
                  <th className="py-1 text-right font-normal">cost</th>
                  <th className="py-1 text-right font-normal">avg latency</th>
                </tr>
              </thead>
              <tbody>
                {model.rows.map((row) => (
                  <ClientRow key={row.key} row={row} onPick={() => setClient(row.key)} />
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}
    </section>
  );
}

/** Source-tag display for a roll-up row: badge text + title, keyed off the BOUNDED source. A `null`
 *  source — an accepted label with NO recorded provenance — is EXPLICITLY source-unavailable (a `?`
 *  badge, neutral-dim), NEVER masqueraded as a strong configured-id with a `ua` badge (review MEDIUM).
 *  Mirrors the model's strength classification (`row.attributionQuality` is already `unavailable` here). */
function rowSourceTag(source: ClientRollupRow['source'], label: string): { badge: string; title: string } {
  switch (source) {
    case 'key_hash':
      return { badge: 'key', title: `strong key-hash identity (a key-<hex> digest — not a raw key). Click to filter to ${label}.` };
    case 'configured_header':
      return { badge: 'id', title: `strong configured caller-id identity. Click to filter to ${label}.` };
    case 'user_agent':
      return { badge: 'ua', title: `WEAK user-agent fallback — not a confirmed identity. Click to filter to ${label}.` };
    default:
      // A labelled row with NO source provenance — source-UNAVAILABLE (NOT a strong identity, NOT a UA).
      return { badge: '?', title: `client source unavailable (a label with no recorded provenance). Click to filter to ${label}.` };
  }
}

/** One client's roll-up row — its (strength-tagged) identity + flows / err / cost / latency. Clicking it
 *  cross-links into the per-client filter. */
function ClientRow({ row, onPick }: { row: ClientRollupRow; onPick: () => void }) {
  const errOver = row.errorRatePct > 5;
  const tag = rowSourceTag(row.source, row.label);
  return (
    <tr className="border-t border-line/40" data-testid="client-rollup-row" data-client={row.key} data-strength={row.strength}>
      <td className="py-1 pr-2">
        <button
          type="button"
          onClick={onPick}
          className="flex min-w-0 items-center gap-1 text-left hover:text-accent"
          data-testid="client-rollup-pick"
          title={tag.title}
        >
          <span className={cn('truncate font-mono', row.weak ? 'italic text-text-muted' : 'text-text')}>{row.label}</span>
          {/* The source tag — the WEAK UA fallback is visibly distinct (amber) from a strong identity
              (neutral); a source-UNAVAILABLE row (null source) is neutral-dim with a `?` badge, NEVER
              amber/`ua`. `data-quality` is the model's classification (measured/derived/unavailable). */}
          <span
            className={cn(
              'shrink-0 rounded-sm px-1 text-[9px] uppercase tracking-wide',
              row.weak ? 'bg-status-cooling/15 text-status-cooling' : 'bg-line/40 text-text-muted',
            )}
            data-testid="client-rollup-source"
            data-quality={row.attributionQuality}
            data-source={row.source ?? undefined}
          >
            {tag.badge}
          </span>
        </button>
      </td>
      <td className="py-1 text-right tabular-nums text-text-muted" data-testid="client-rollup-flows">
        {row.total}
      </td>
      <td className="py-1 text-right tabular-nums" data-testid="client-rollup-err" data-quality="derived">
        <span className={errOver ? 'text-status-down' : 'text-text-muted'}>{row.errorRateText}</span>
      </td>
      <td className="py-1 text-right tabular-nums" data-testid="client-rollup-cost" data-quality={row.costQuality}>
        {/* Don't-lie-with-zeros: an unpriced client reads `—`, never a fabricated `$0.00`. */}
        <span className={row.costQuality === 'unavailable' ? 'text-text-muted' : 'text-meta'}>
          {row.cost === null ? '—' : fmtCost(row.cost)}
        </span>
      </td>
      <td className="py-1 text-right tabular-nums" data-testid="client-rollup-latency" data-quality={row.latencyQuality}>
        <span className={row.latencyQuality === 'unavailable' ? 'text-text-muted' : 'text-text'}>
          {fmtLatency(row.avgLatencyMs)}
        </span>
      </td>
    </tr>
  );
}
