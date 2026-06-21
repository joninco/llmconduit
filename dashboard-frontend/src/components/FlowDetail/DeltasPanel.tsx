/**
 * Streamed-deltas sub-panel (D10): renders the live `segment_append` stream for the flow's
 * response. Output text is BRIGHT, reasoning is DIM, and tool-call segments render as
 * EXPANDABLE cards (collapsed to the tool name, expand to show the call payload). Consecutive
 * same-kind segments are coalesced into one block so the rendered stream reads like the model's
 * actual output rather than one node per token. This INCLUDES tool segments: a real tool call's
 * arguments stream as MANY adjacent `tool` fragments, so adjacent tool segments accumulate into
 * ONE card rather than one card per fragment (finding 2).
 *
 * But TWO DISTINCT tool calls must not merge into one card just because they are kind-adjacent
 * (finding 5). The backend (`src/monitor.rs::append_function_call_delta`) introduces each distinct
 * call with an explicit boundary line — `tool arguments <id>:` — and coalesces same-kind text
 * server-side, so a single `tool` segment can hold several calls concatenated. We therefore split
 * a coalesced tool block on that boundary marker into one card PER call, rather than on `kind`
 * adjacency alone. A tool block with no marker (e.g. a lone streamed call whose fragments carry no
 * header) stays a single card. The boundary rule itself lives in `lib/toolCalls` (`splitToolCallText`
 * / `TOOL_CALL_BOUNDARY`) so the theater's `riverModel` splits identically — ONE source of truth.
 */
import { useState } from 'react';
import type { DebugSegment } from '../../api/types';
import { cn } from '../../lib/cn';
import { TOOL_CALL_BOUNDARY, splitToolCallText } from '../../lib/toolCalls';

/** A coalesced run of adjacent same-kind segments (output / reasoning / one or more tool calls). */
interface Block {
  kind: DebugSegment['kind'];
  text: string;
  /** First timestamp in the run (for ordering / keys). */
  ts: number;
}

/** A rendered card: an output/reasoning passage, or a SINGLE tool call's payload. */
interface Card {
  kind: DebugSegment['kind'];
  text: string;
  ts: number;
}

/**
 * Coalesces adjacent same-kind segments into one block. Output/reasoning runs read as one
 * passage; an adjacent run of `tool` segments (the streamed argument fragments of a single tool
 * call) accumulates into ONE block so a fragmented tool call is not split across many cards
 * (finding 2). A different kind in between (or a non-adjacent tool run) starts a fresh block.
 */
function coalesce(segments: DebugSegment[]): Block[] {
  const blocks: Block[] = [];
  for (const seg of segments) {
    const last = blocks[blocks.length - 1];
    if (last && last.kind === seg.kind) {
      last.text += seg.text;
    } else {
      blocks.push({ kind: seg.kind, text: seg.text, ts: seg.timestamp_ms });
    }
  }
  return blocks;
}

/**
 * Splits a coalesced `tool` block into one card per DISTINCT call, cutting on the backend's
 * `tool arguments <id>:` boundary line (finding 5) via the shared `splitToolCallText` rule. Text
 * before the first boundary (a streamed call whose fragments carry no header — the finding-2 case)
 * is its own card, so a single markerless tool block stays ONE card. Non-tool blocks pass through
 * unchanged.
 */
function splitToolCalls(block: Block): Card[] {
  if (block.kind !== 'tool') return [{ kind: block.kind, text: block.text, ts: block.ts }];
  return splitToolCallText(block.text).map((text) => ({ kind: 'tool' as const, text, ts: block.ts }));
}

export function DeltasPanel({ segments }: { segments: DebugSegment[] }) {
  // Coalesce adjacent same-kind runs, then split each tool block into one card per distinct call
  // (finding 5) — output/reasoning blocks pass through as single cards.
  const cards = coalesce(segments).flatMap(splitToolCalls);

  if (cards.length === 0) {
    return (
      <div className="px-3 py-4 text-xs italic text-text-muted" data-testid="deltas-empty">
        No streamed deltas yet.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-1.5 px-3 py-2" data-testid="deltas-panel">
      {cards.map((c, i) =>
        c.kind === 'tool' ? (
          <ToolCard key={`${c.ts}-${i}`} text={c.text} />
        ) : (
          <pre
            key={`${c.ts}-${i}`}
            data-segment-kind={c.kind}
            className={cn(
              'whitespace-pre-wrap break-words font-mono text-xs leading-relaxed',
              c.kind === 'output' ? 'text-text' : 'italic text-text-muted',
            )}
          >
            {c.text}
          </pre>
        ),
      )}
    </div>
  );
}

/** An expandable tool-call card: collapsed shows the tool label; expanded shows the payload. */
function ToolCard({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const label = toolLabel(text);
  return (
    <div className="rounded-md border border-meta/40 bg-meta/10" data-testid="tool-card" data-open={open || undefined}>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-2 py-1 text-left text-xs text-meta"
      >
        <span aria-hidden className="tabular-nums">{open ? '▾' : '▸'}</span>
        <span className="font-medium">tool</span>
        <span className="truncate font-mono text-text-muted">{label}</span>
      </button>
      {open && (
        <pre
          className="whitespace-pre-wrap break-words border-t border-meta/30 px-2 py-1.5 font-mono text-xs leading-relaxed text-text"
          data-testid="tool-card-body"
        >
          {text}
        </pre>
      )}
    </div>
  );
}

/**
 * Best-effort one-line label for a tool card. The card may lead with the backend's
 * `tool arguments <id>:` header (finding 5); we strip it and resolve the label from the argument
 * JSON that follows (a `name`/`tool`/`function` field). Falls back to the header line (showing the
 * call id) or the first non-empty line when there is no parseable JSON.
 */
function toolLabel(text: string): string {
  const lines = text.split('\n');
  const headerIdx = lines.findIndex((l) => TOOL_CALL_BOUNDARY.test(l));
  const header = headerIdx >= 0 ? lines[headerIdx]! : null;
  // The argument body is everything after the header (or the whole text when there is no header).
  const body = (headerIdx >= 0 ? lines.slice(headerIdx + 1) : lines).join('\n').trim();
  try {
    const obj = JSON.parse(body) as Record<string, unknown>;
    const name = obj.name ?? obj.tool ?? obj.function;
    if (typeof name === 'string') return name;
  } catch {
    // not JSON — fall through to the header / first-line label
  }
  const fallback = header ?? body.split('\n', 1)[0] ?? '';
  return fallback.length > 60 ? `${fallback.slice(0, 57)}…` : fallback;
}
