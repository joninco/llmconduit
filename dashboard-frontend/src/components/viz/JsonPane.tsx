/**
 * JsonPane — a per-path COLLAPSIBLE + SEARCHABLE JSON viewer with highlight.js syntax coloring
 * and per-JSON-path diff tints.
 *
 * The structural diff (./FlowDetail/diff) is keyed by JSON PATH, and `toJsonLines` serializes the
 * value into path-tagged lines, so each rendered line looks up its `DiffKind` tint by path. On top
 * of that flat list, `jsonFold` pairs each container's open/close lines and computes the visible
 * rows: collapsed containers render as a single `{ … } N` summary, and an active search query
 * filters to matching lines plus their ancestors (auto-expanded), flagging matches.
 *
 * Rendered with React (not the old imperative highlight build) so the fold chevrons + search state
 * are ordinary event handlers; highlight.js still colors each line (per-line `__html`). The DOM
 * contract is unchanged — `jsonpane-{code,scroll,empty}-<label>` and `.json-line[data-path]`
 * (`[data-diff]` when tinted) — so the diff/scroll-sync tests hold.
 */
import { useCallback, useMemo, useState } from 'react';
import hljs from 'highlight.js/lib/core';
import json from 'highlight.js/lib/languages/json';
import { colors } from '../../design/tokens';
import type { DiffKind, DiffMap } from '../FlowDetail/diff';
import { toJsonLines } from './jsonLines';
import { buildFoldModel, computeRows, type FoldRow } from './jsonFold';

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

/** Background tint (solid color OR gradient) for a path's `DiffKind` on this pane's side. */
function tintFor(kind: DiffKind | undefined, side: DiffSide): string | undefined {
  if (!kind) return undefined;
  if (kind === 'changed') return colors.diffContextBg;
  if (kind === 'added') return side === 'left' ? undefined : colors.diffAddBg;
  if (kind === 'removed') return side === 'right' ? undefined : colors.diffRemoveBg;
  if (kind === 'added-removed' || kind === 'changed-removed') {
    const introduced = kind === 'added-removed' ? colors.diffAddBg : colors.diffContextBg;
    if (side === 'left') return colors.diffRemoveBg;
    if (side === 'right') return introduced;
    return `linear-gradient(${introduced} 0 50%, ${colors.diffRemoveBg} 50% 100%)`;
  }
  return undefined;
}

const PAD_BASE = 6;
const PER_DEPTH = 12;

export interface JsonPaneProps {
  value: unknown;
  diff?: DiffMap;
  side?: DiffSide;
  label: string;
  emptyLabel?: string;
  /** Shared search query (from the inspector). Empty ⇒ full document with fold state applied. */
  query?: string;
  className?: string;
  scrollRef?: React.RefObject<HTMLDivElement>;
  onScroll?: React.UIEventHandler<HTMLDivElement>;
}

export function JsonPane({
  value,
  diff,
  side = 'right',
  label,
  emptyLabel = 'body evicted',
  query = '',
  className,
  scrollRef,
  onScroll,
}: JsonPaneProps) {
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());

  const lines = useMemo(() => toJsonLines(value), [value]);
  const model = useMemo(() => buildFoldModel(lines), [lines]);
  const { rows, matchCount } = useMemo(
    () => computeRows(lines, model, collapsed, query),
    [lines, model, collapsed, query],
  );

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
      <div className="flex items-center justify-between gap-2 border-b border-line bg-panel-raised px-3 py-1.5">
        <span className="flex items-center gap-1.5 text-xs font-medium uppercase tracking-[0.12em] text-text-muted">
          {label}
          {searching && (
            <span
              className="rounded-sm bg-status-cooling/15 px-1 font-mono text-[10px] tracking-normal text-status-cooling"
              data-testid={`jsonpane-matches-${label}`}
            >
              {matchCount}
            </span>
          )}
        </span>
        {hasValue && !searching && model.containerPaths.length > 0 && (
          <button
            type="button"
            onClick={toggleAll}
            className="rounded-sm px-1 font-mono text-[10px] uppercase tracking-wide text-text-muted transition-colors hover:text-accent"
            data-testid={`jsonpane-foldall-${label}`}
          >
            {allCollapsed ? 'expand' : 'collapse'}
          </button>
        )}
      </div>
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="min-h-0 flex-1 overflow-auto bg-panel"
        data-testid={`jsonpane-scroll-${label}`}
      >
        {hasValue ? (
          <code
            className="hljs block py-2 font-mono text-xs leading-relaxed"
            data-testid={`jsonpane-code-${label}`}
          >
            {rows.map((row) => (
              <JsonRow
                key={row.index}
                row={row}
                tint={tintFor(diff?.get(row.line.path), side)}
                diffKind={diff?.get(row.line.path)}
                searching={searching}
                onToggle={toggle}
              />
            ))}
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
  tint,
  diffKind,
  searching,
  onToggle,
}: {
  row: FoldRow;
  tint: string | undefined;
  diffKind: DiffKind | undefined;
  searching: boolean;
  onToggle: (path: string) => void;
}) {
  const { line, foldable, folded, isMatch, block } = row;
  const content = line.text.slice(line.depth * 2); // strip indent (paddingLeft renders depth)
  const html = useMemo(() => highlightJson(content), [content]);
  const showChevron = foldable && !searching;

  return (
    <div
      className={`json-line flex items-start border-l-2 ${isMatch ? 'border-l-status-cooling' : 'border-l-transparent'}`}
      data-path={line.path}
      data-diff={diffKind ? diffKind : undefined}
      style={{ background: tint, paddingLeft: PAD_BASE + line.depth * PER_DEPTH }}
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
    </div>
  );
}
