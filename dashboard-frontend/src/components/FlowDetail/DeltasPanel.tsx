/**
 * Streamed-deltas sub-panel (D10): renders the live `segment_append` stream for the flow's
 * response. Output text is BRIGHT, reasoning is DIM, and tool-call segments render as
 * EXPANDABLE cards (collapsed to the tool name, expand to show the call payload). Consecutive
 * same-kind segments are coalesced into one block so the rendered stream reads like the model's
 * actual output rather than one node per token. This INCLUDES tool segments: a real tool call's
 * arguments stream as MANY adjacent `tool` fragments, so adjacent tool segments accumulate into
 * ONE card rather than one card per fragment (finding 2).
 */
import { useState } from 'react';
import type { DebugSegment } from '../../api/types';
import { cn } from '../../lib/cn';

/** A coalesced run of adjacent same-kind segments (output / reasoning / one tool call). */
interface Block {
  kind: DebugSegment['kind'];
  text: string;
  /** First timestamp in the run (for ordering / keys). */
  ts: number;
}

/**
 * Coalesces adjacent same-kind segments into one block. Output/reasoning runs read as one
 * passage; an adjacent run of `tool` segments (the streamed argument fragments of a single tool
 * call) accumulates into ONE card so a fragmented tool call is not split across many cards
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

export function DeltasPanel({ segments }: { segments: DebugSegment[] }) {
  const blocks = coalesce(segments);

  if (blocks.length === 0) {
    return (
      <div className="px-3 py-4 text-xs italic text-text-muted" data-testid="deltas-empty">
        No streamed deltas yet.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-1.5 px-3 py-2" data-testid="deltas-panel">
      {blocks.map((b, i) =>
        b.kind === 'tool' ? (
          <ToolCard key={`${b.ts}-${i}`} text={b.text} />
        ) : (
          <pre
            key={`${b.ts}-${i}`}
            data-segment-kind={b.kind}
            className={cn(
              'whitespace-pre-wrap break-words font-mono text-xs leading-relaxed',
              b.kind === 'output' ? 'text-text' : 'italic text-text-muted',
            )}
          >
            {b.text}
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

/** Best-effort one-line label for a tool segment: a JSON `name`, else the first line. */
function toolLabel(text: string): string {
  try {
    const obj = JSON.parse(text) as Record<string, unknown>;
    const name = obj.name ?? obj.tool ?? obj.function;
    if (typeof name === 'string') return name;
  } catch {
    // not JSON — fall through to first-line
  }
  const firstLine = text.split('\n', 1)[0] ?? '';
  return firstLine.length > 60 ? `${firstLine.slice(0, 57)}…` : firstLine;
}
