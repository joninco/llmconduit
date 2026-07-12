/**
 * JsonPane — a per-path COLLAPSIBLE + SEARCHABLE JSON viewer with highlight.js syntax coloring
 * and explicit, color-independent transformation annotations.
 *
 * The structural diff (./FlowDetail/diff) is keyed by JSON PATH, and `toJsonLines` serializes the
 * value into path-tagged lines. `ChangeMap`s then explain the actual operation at a path: what was
 * introduced, what value was rewritten, and what will be omitted from the next layer. On top of
 * that flat list, `jsonFold` pairs each container's open/close lines and computes the visible rows:
 * collapsed containers render as a single `{ … } N` summary, an active search filters to matching
 * lines + ancestors, and changes-only mode shows operation roots instead of walls of tinted JSON.
 *
 * Rendered with React (not the old imperative highlight build) so the fold chevrons + search state
 * are ordinary event handlers. The final rows are fixed-height virtualized: only the viewport plus
 * overscan mounts a `JsonRow`, so highlight.js runs only for visible lines. The DOM contract remains
 * `jsonpane-{code,scroll,empty}-<label>` and `.json-line[data-path]` (`[data-diff]` when changed).
 */
import { useCallback, useMemo, useRef, useState, type MutableRefObject } from 'react';
import { observeElementRect, useVirtualizer, type Rect, type Virtualizer } from '@tanstack/react-virtual';
import hljs from 'highlight.js/lib/core';
import json from 'highlight.js/lib/languages/json';
import type { ChangeDetail, ChangeMap, DiffKind, DiffMap } from '../FlowDetail/diff';
import { toJsonLines } from './jsonLines';
import { buildFoldModel, computeRows, isCloseLine, type FoldRow } from './jsonFold';

let registered = false;
function ensureJsonLanguage(): void {
  if (registered) return;
  hljs.registerLanguage('json', json);
  registered = true;
}

function highlightJson(text: string): string {
  ensureJsonLanguage();
  try {
    return hljs.highlight(text, { language: 'json', ignoreIllegals: true }).value || '​';
  } catch {
    // A line highlighted in isolation should never throw; fall back to a zero-width space.
    return text ? escapeHtml(text) : '​';
  }
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>]/g, (c) => (c === '&' ? '&amp;' : c === '<' ? '&lt;' : '&gt;'));
}

export type DiffSide = 'left' | 'right' | 'both';

export interface JsonStage {
  step: 'A' | 'B' | 'C';
  title: string;
  subtitle: string;
  nextLabel?: string;
}

type SignalTone = 'introduced' | 'rewritten' | 'omitted';

interface ChangeSignal {
  key: string;
  glyph: string;
  label: string;
  detail?: string;
  tone: SignalTone;
}

/** Compact counterpart value for an inline operation label; never stringify a giant prompt. */
function valuePreview(value: unknown): string {
  if (Array.isArray(value)) return `array · ${value.length.toLocaleString()} items`;
  if (typeof value === 'object' && value !== null) {
    return `object · ${Object.keys(value as Record<string, unknown>).length.toLocaleString()} fields`;
  }
  if (typeof value === 'string') {
    const clipped = value.length > 44 ? `${value.slice(0, 41)}…` : value;
    return JSON.stringify(clipped);
  }
  return JSON.stringify(value) ?? String(value);
}

function incomingSignal(change: ChangeDetail | undefined): ChangeSignal | null {
  if (!change || change.kind === 'removed') return null;
  if (change.kind === 'added') {
    return {
      key: 'incoming-added',
      glyph: '+',
      label: 'introduced here',
      tone: 'introduced',
    };
  }
  return {
    key: 'incoming-changed',
    glyph: '←',
    label: 'was',
    detail: valuePreview(change.before),
    tone: 'rewritten',
  };
}

function outgoingSignal(change: ChangeDetail | undefined, nextLabel?: string): ChangeSignal | null {
  if (!change || change.kind === 'added') return null;
  if (change.kind === 'removed') {
    const label = nextLabel === 'upstream'
      ? 'not sent upstream'
      : nextLabel === 'canonical'
        ? 'not in canonical'
        : `not sent to ${nextLabel ?? 'next layer'}`;
    return {
      key: 'outgoing-removed',
      glyph: '−',
      label,
      tone: 'omitted',
    };
  }
  return {
    key: 'outgoing-changed',
    glyph: '→',
    label: 'becomes',
    detail: valuePreview(change.after),
    tone: 'rewritten',
  };
}

/** Compatibility fallback for callers that provide only the structural paint map. */
function fallbackSignals(kind: DiffKind | undefined, side: DiffSide): ChangeSignal[] {
  if (!kind || kind === 'unchanged') return [];
  const signals: ChangeSignal[] = [];
  if ((kind === 'added' || kind === 'added-removed') && side !== 'left') {
    signals.push({ key: 'fallback-added', glyph: '+', label: 'introduced', tone: 'introduced' });
  }
  if ((kind === 'changed' || kind === 'changed-removed') && side !== 'left') {
    signals.push({ key: 'fallback-changed', glyph: '~', label: 'rewritten', tone: 'rewritten' });
  }
  if ((kind === 'removed' || kind === 'added-removed' || kind === 'changed-removed') && side !== 'right') {
    signals.push({ key: 'fallback-removed', glyph: '−', label: 'omitted next', tone: 'omitted' });
  }
  // A left-hand changed value is still useful even though its richer label would normally come
  // from `outgoingChanges` (the fallback has no counterpart value to show).
  if (kind === 'changed' && side === 'left') {
    signals.push({ key: 'fallback-changed-next', glyph: '→', label: 'rewritten next', tone: 'rewritten' });
  }
  return signals;
}

function operationPaths(
  incoming: ChangeMap | undefined,
  outgoing: ChangeMap | undefined,
  diff: DiffMap | undefined,
  side: DiffSide,
): ReadonlySet<string> {
  const paths = new Set<string>();
  if (incoming !== undefined || outgoing !== undefined) {
    for (const [path, change] of incoming ?? []) {
      if (change.kind === 'added' || change.kind === 'changed') paths.add(path);
    }
    for (const [path, change] of outgoing ?? []) {
      if (change.kind === 'removed' || change.kind === 'changed') paths.add(path);
    }
    return paths;
  }
  for (const [path, kind] of diff ?? []) {
    if (fallbackSignals(kind, side).length > 0) paths.add(path);
  }
  return paths;
}

const PAD_BASE = 6;
const PER_DEPTH = 12;
const ROW_HEIGHT = 20;
const VERTICAL_PADDING = 8;
const OVERSCAN = 10;
/** Bound the expanded/search result surface even though only a viewport-sized slice reaches DOM. */
export const JSON_RENDER_LINE_CAP = 10_000;
const INITIAL_RECT = { width: 640, height: 320 };

/** Keep SSR/jsdom and temporarily hidden split panes useful until a non-zero resize arrives. */
function observePaneRect(
  instance: Virtualizer<HTMLDivElement, Element>,
  callback: (rect: Rect) => void,
): (() => void) | undefined {
  return observeElementRect(instance, (rect) => {
    callback({
      width: rect.width > 0 ? rect.width : INITIAL_RECT.width,
      height: rect.height > 0 ? rect.height : INITIAL_RECT.height,
    });
  });
}

export interface JsonPaneProps {
  value: unknown;
  /** Descendant-aware structural classification retained for DOM diagnostics/tests. */
  diff?: DiffMap;
  /** Concise transformations from the previous layer into this layer. */
  incomingChanges?: ChangeMap;
  /** Concise transformations from this layer into the next layer. */
  outgoingChanges?: ChangeMap;
  side?: DiffSide;
  label: string;
  stage?: JsonStage;
  emptyLabel?: string;
  /** Shared search query (from the inspector). Empty ⇒ full document with fold state applied. */
  query?: string;
  /** Show operation roots + context only. A non-empty search always searches the whole body. */
  changesOnly?: boolean;
  className?: string;
  scrollRef?: React.RefObject<HTMLDivElement>;
  onScroll?: React.UIEventHandler<HTMLDivElement>;
  /** Focus-mode hook (inspector zoom): toggles this pane filling the whole main region. When
   * provided, the header gets a ⤢ button and double-clicking the header surface triggers it. */
  onZoom?: () => void;
  /** Whether this pane is currently the zoomed (focus-mode) pane — flips the ⤢ affordance. */
  zoomed?: boolean;
}

export function JsonPane({
  value,
  diff,
  incomingChanges,
  outgoingChanges,
  side = 'right',
  label,
  stage,
  emptyLabel = 'body evicted',
  query = '',
  changesOnly = false,
  className,
  scrollRef,
  onScroll,
  onZoom,
  zoomed = false,
}: JsonPaneProps) {
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());

  const lines = useMemo(() => toJsonLines(value), [value]);
  const model = useMemo(() => buildFoldModel(lines), [lines]);
  const focusedPaths = useMemo(
    () => operationPaths(incomingChanges, outgoingChanges, diff, side),
    [incomingChanges, outgoingChanges, diff, side],
  );
  const { rows, matchCount } = useMemo(
    () => computeRows(lines, model, collapsed, query, changesOnly ? focusedPaths : undefined),
    [lines, model, collapsed, query, changesOnly, focusedPaths],
  );
  const renderedRows = rows.length > JSON_RENDER_LINE_CAP
    ? rows.slice(0, JSON_RENDER_LINE_CAP)
    : rows;
  const lineModelOverCap = lines.length > JSON_RENDER_LINE_CAP;
  const omittedVisibleRows = rows.length - renderedRows.length;

  const internalScrollRef = useRef<HTMLDivElement | null>(null);
  const setScrollElement = useCallback(
    (node: HTMLDivElement | null) => {
      internalScrollRef.current = node;
      if (scrollRef) (scrollRef as MutableRefObject<HTMLDivElement | null>).current = node;
    },
    [scrollRef],
  );
  const virtualizer = useVirtualizer({
    count: renderedRows.length,
    getScrollElement: () => internalScrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: OVERSCAN,
    initialRect: INITIAL_RECT,
    observeElementRect: observePaneRect,
    getItemKey: (index) => renderedRows[index]?.index ?? index,
  });

  const searching = query.trim().length > 0;

  const toggle = useCallback((path: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const allCollapsed = model.containerPaths.length > 0 && collapsed.size >= model.containerPaths.length;
  const toggleAll = useCallback(() => {
    setCollapsed((prev) =>
      prev.size >= model.containerPaths.length ? new Set() : new Set(model.containerPaths),
    );
  }, [model.containerPaths]);

  const hasValue = value !== undefined;

  return (
    <div className={`flex min-h-0 flex-col ${className ?? ''}`} data-testid={`jsonpane-${label}`}>
      <div
        className="flex items-center justify-between gap-2 border-b border-line bg-panel-raised px-3 py-1.5"
        // Double-clicking the header surface zooms (focus mode) — but not double-clicks that
        // landed on the fold/zoom buttons, whose own single-click actions must not also zoom.
        onDoubleClick={
          onZoom
            ? (e) => {
                if ((e.target as HTMLElement).closest('button')) return;
                onZoom();
              }
            : undefined
        }
      >
        <span className="flex min-w-0 items-center gap-2">
          {stage ? (
            <>
              <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full border border-accent/50 font-mono text-[10px] font-semibold text-accent">
                {stage.step}
              </span>
              <span className="min-w-0 leading-tight">
                <span className="block truncate text-[11px] font-semibold uppercase tracking-[0.11em] text-text">
                  {stage.title}
                </span>
                <span className="block truncate text-[9px] uppercase tracking-[0.08em] text-text-muted">
                  {stage.subtitle}
                </span>
              </span>
            </>
          ) : (
            <span className="text-xs font-medium uppercase tracking-[0.12em] text-text-muted">{label}</span>
          )}
          {searching && (
            <span
              className="rounded-sm bg-status-cooling/15 px-1 font-mono text-[10px] tracking-normal text-status-cooling"
              data-testid={`jsonpane-matches-${label}`}
            >
              {matchCount}
            </span>
          )}
          {changesOnly && !searching && (
            <span
              className="shrink-0 rounded-sm border border-accent/30 px-1 font-mono text-[9px] uppercase tracking-normal text-accent"
              data-testid={`jsonpane-change-count-${label}`}
            >
              {focusedPaths.size} ops
            </span>
          )}
        </span>
        <span className="flex items-center gap-1">
          {lineModelOverCap && (
            <span
              role="status"
              className="shrink-0 rounded-sm bg-status-cooling/15 px-1 font-mono text-[9px] tracking-normal text-status-cooling"
              data-testid={`jsonpane-render-cap-${label}`}
              title={`${lines.length.toLocaleString()} source lines exceed the ${JSON_RENDER_LINE_CAP.toLocaleString()}-line render cap`}
            >
              render cap · {JSON_RENDER_LINE_CAP.toLocaleString()}
              {omittedVisibleRows > 0 ? ` · ${omittedVisibleRows.toLocaleString()} omitted` : ''}
            </span>
          )}
          {hasValue && !searching && !changesOnly && model.containerPaths.length > 0 && (
            <button
              type="button"
              onClick={toggleAll}
              className="rounded-sm px-1 font-mono text-[10px] uppercase tracking-wide text-text-muted transition-colors hover:text-accent"
              data-testid={`jsonpane-foldall-${label}`}
            >
              {allCollapsed ? 'expand' : 'collapse'}
            </button>
          )}
          {onZoom && (
            <button
              type="button"
              onClick={onZoom}
              aria-label={zoomed ? `restore pane ${label}` : `zoom pane ${label}`}
              title={zoomed ? 'restore (Esc)' : 'zoom to fill the inspector'}
              className="rounded-sm px-1 text-[11px] leading-none text-text-muted transition-colors hover:text-accent"
              data-testid={`jsonpane-zoom-${label}`}
            >
              ⤢
            </button>
          )}
        </span>
      </div>
      <div
        ref={setScrollElement}
        onScroll={onScroll}
        className="min-h-0 flex-1 overflow-auto bg-panel focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent"
        data-testid={`jsonpane-scroll-${label}`}
        tabIndex={0}
        aria-label={`${stage?.title ?? label} JSON`}
      >
        {hasValue && changesOnly && !searching && renderedRows.length === 0 ? (
          <div
            className="flex h-full flex-col items-center justify-center gap-1 px-4 py-6 text-center"
            data-testid={`jsonpane-no-changes-${label}`}
          >
            <span className="font-mono text-[11px] uppercase tracking-[0.12em] text-text-muted">no operations</span>
            <span className="text-[10px] text-text-muted">This stage carries the request through unchanged.</span>
          </div>
        ) : hasValue ? (
          <code
            className="hljs block font-mono text-xs"
            data-testid={`jsonpane-code-${label}`}
            data-total-lines={lines.length}
            data-render-lines={renderedRows.length}
            style={{
              height: `${virtualizer.getTotalSize() + VERTICAL_PADDING * 2}px`,
              minWidth: '100%',
              position: 'relative',
            }}
          >
            {virtualizer.getVirtualItems().map((item) => {
              const row = renderedRows[item.index];
              if (!row) return null;
              const diffKind = diff?.get(row.line.path);
              const hasRichChanges = incomingChanges !== undefined || outgoingChanges !== undefined;
              const signals = isCloseLine(row.line.text)
                ? []
                : hasRichChanges
                ? [
                    incomingSignal(incomingChanges?.get(row.line.path)),
                    outgoingSignal(outgoingChanges?.get(row.line.path), stage?.nextLabel),
                  ].filter((signal): signal is ChangeSignal => signal !== null)
                : fallbackSignals(diffKind, side);
              return (
                <div
                  key={item.key}
                  data-virtual-index={item.index}
                  style={{
                    height: `${ROW_HEIGHT}px`,
                    left: 0,
                    position: 'absolute',
                    top: 0,
                    transform: `translateY(${item.start + VERTICAL_PADDING}px)`,
                    width: '100%',
                  }}
                >
                  <JsonRow
                    row={row}
                    signals={signals}
                    diffKind={diffKind}
                    searching={searching}
                    focused={changesOnly && !searching}
                    onToggle={toggle}
                  />
                </div>
              );
            })}
          </code>
        ) : (
          <div
            className="flex h-full items-center justify-center px-3 py-6 text-xs italic text-text-muted"
            data-testid={`jsonpane-empty-${label}`}
          >
            {emptyLabel}
          </div>
        )}
      </div>
    </div>
  );
}

function JsonRow({
  row,
  signals,
  diffKind,
  searching,
  focused,
  onToggle,
}: {
  row: FoldRow;
  signals: ChangeSignal[];
  diffKind: DiffKind | undefined;
  searching: boolean;
  focused: boolean;
  onToggle: (path: string) => void;
}) {
  const { line, foldable, folded, isMatch, block } = row;
  const content = line.text.slice(line.depth * 2); // strip indent (paddingLeft renders depth)
  const html = useMemo(() => highlightJson(content), [content]);
  const showChevron = foldable && !searching && !focused;
  const marked = signals.length > 0;

  return (
    <div
      className={`json-line flex h-5 items-start border-l-2 leading-5 ${isMatch ? 'border-l-status-cooling' : marked ? 'border-l-accent/70' : 'border-l-transparent'}`}
      data-path={line.path}
      data-diff={diffKind ? diffKind : undefined}
      data-operation={marked ? signals.map((signal) => signal.tone).join(' ') : undefined}
      style={{ paddingLeft: PAD_BASE + line.depth * PER_DEPTH }}
    >
      {showChevron ? (
        <button
          type="button"
          onClick={() => onToggle(line.path)}
          aria-expanded={!folded}
          aria-label={`${folded ? 'expand' : 'collapse'} ${line.path}`}
          className="mr-0.5 w-3 shrink-0 select-none text-center text-text-muted transition-colors hover:text-accent"
        >
          {folded ? '▸' : '▾'}
        </button>
      ) : focused && folded ? (
        <span className="mr-0.5 w-3 shrink-0 select-none text-center text-accent" aria-hidden>▸</span>
      ) : (
        <span className="mr-0.5 w-3 shrink-0" aria-hidden />
      )}
      <span className="json-line-text whitespace-pre" dangerouslySetInnerHTML={{ __html: html }} />
      {folded && block && (
        <span className="select-none whitespace-pre text-text-muted">
          {` … ${block.bracketClose}${block.closeSuffix}`}
          <span className="ml-1.5 rounded-sm bg-line/60 px-1 text-[10px] text-text-muted">{block.childCount}</span>
        </span>
      )}
      {signals.map((signal) => <ChangeBadge key={signal.key} signal={signal} />)}
    </div>
  );
}

const SIGNAL_CLASSES: Record<SignalTone, string> = {
  introduced: 'border-accent/40 bg-accent/10 text-accent',
  rewritten: 'border-status-cooling/40 bg-status-cooling/10 text-status-cooling',
  omitted: 'border-meta/40 bg-meta/10 text-meta',
};

function ChangeBadge({ signal }: { signal: ChangeSignal }) {
  const explanation = signal.detail ? `${signal.label} ${signal.detail}` : signal.label;
  return (
    <span
      className={`ml-2 inline-flex h-4 shrink-0 items-center gap-1 rounded-sm border px-1 font-mono text-[9px] leading-none ${SIGNAL_CLASSES[signal.tone]}`}
      title={explanation}
      aria-label={explanation}
    >
      <span className="text-[11px] font-semibold" aria-hidden>{signal.glyph}</span>
      <span className="uppercase tracking-[0.06em]">{signal.label}</span>
      {signal.detail && <span className="max-w-64 truncate normal-case tracking-normal text-text-muted">{signal.detail}</span>}
    </span>
  );
}
