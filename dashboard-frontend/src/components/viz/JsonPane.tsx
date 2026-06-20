/**
 * JsonPane — a JSON viewer with highlight.js syntax coloring AND per-JSON-path diff tints.
 *
 * Why imperative (D10 constraint "StrictMode-safe viz; highlight.js keyed to a ref"): highlight.js
 * writes HTML, so we build the pane's DOM imperatively through `useImperativeViz` — setup runs in
 * `useLayoutEffect`, and the REQUIRED cleanup clears the node. React 18 StrictMode mounts →
 * unmounts → remounts in dev; the cleanup makes that idempotent (no duplicated highlighted nodes,
 * no leaked listeners) — the same contract `DemoViz`/`ForceDemoViz` are tested against.
 *
 * Why line-by-line: the structural diff (./FlowDetail/diff) is keyed by JSON PATH, not text line.
 * We serialize the value into path-tagged lines (`toJsonLines`), highlight each line's text, and
 * tint the line's background by its path's `DiffKind`. So syntax color and the add/changed/removed
 * tint coexist without a heavy diff dep. Only the `json` grammar is registered (deterministic,
 * smaller than the full bundle).
 */
import { useRef } from 'react';
import hljs from 'highlight.js/lib/core';
import json from 'highlight.js/lib/languages/json';
import { useImperativeViz } from '../../viz/useImperativeViz';
import { colors } from '../../design/tokens';
import type { DiffKind, DiffMap } from '../FlowDetail/diff';
import { toJsonLines } from './jsonLines';

// Register the JSON grammar exactly once (idempotent — re-registering is a no-op but we guard
// so repeated module loads under HMR/tests don't churn). Using `lib/core` keeps the highlighter
// to the one language we need rather than the full auto-detect bundle.
let registered = false;
function ensureJsonLanguage(): void {
  if (registered) return;
  hljs.registerLanguage('json', json);
  registered = true;
}

/**
 * Which side of a layer comparison this pane is on. `both` is the MIDDLE pane (B): it is the
 * right side of A→B and the left side of B→C at once, so it surfaces added/changed AND removed
 * (the diff is pre-merged by `combineMiddleDiff`) — finding 4.
 */
export type DiffSide = 'left' | 'right' | 'both';

/**
 * Background tint (a CSS `background` value — solid color OR gradient) for a path's `DiffKind`,
 * from the design tokens (palette `diff*`). A `left` pane only surfaces `removed` (a field dropped
 * by the next layer); a `right` pane surfaces `added`/`changed`; the `both` middle pane surfaces
 * all three PLUS the composite kinds. `unchanged` and the irrelevant side get no tint.
 *
 * COMPOSITE (pane B only, finding 5): `added-removed` / `changed-removed` mean the path was
 * introduced/changed by A→B AND dropped toward C. On the `both` pane they render a split gradient
 * (the A→B add/changed tint over the B→C remove tint) so BOTH directions are visible at once. On a
 * `left`/`right` pane (which never receives composites from `combineMiddleDiff`) they degrade to the
 * side-relevant half.
 */
function tintFor(kind: DiffKind | undefined, side: DiffSide): string | undefined {
  if (!kind) return undefined;
  if (kind === 'changed') return colors.diffContextBg; // changed tints on every side
  if (kind === 'added') return side === 'left' ? undefined : colors.diffAddBg;
  if (kind === 'removed') return side === 'right' ? undefined : colors.diffRemoveBg;
  // Composite: the field is both introduced/changed in B and removed toward C.
  if (kind === 'added-removed' || kind === 'changed-removed') {
    const introduced = kind === 'added-removed' ? colors.diffAddBg : colors.diffContextBg;
    if (side === 'left') return colors.diffRemoveBg; // left sees only the removal
    if (side === 'right') return introduced; // right sees only the A→B side
    // The middle (`both`) pane shows BOTH: top half the introduced tint, bottom half the removal.
    return `linear-gradient(${introduced} 0 50%, ${colors.diffRemoveBg} 50% 100%)`;
  }
  return undefined;
}

export interface JsonPaneProps {
  /** The JSON value to render. `undefined` ⇒ the empty/evicted placeholder is shown instead. */
  value: unknown;
  /** Path→DiffKind tints (from `diffLayers`). Optional: no map ⇒ no tinting. */
  diff?: DiffMap;
  /** Which side of the diff this pane renders (controls which kinds tint). Default `right`. */
  side?: DiffSide;
  /** Accessible label / column heading echoed into a `data-` attr for tests. */
  label: string;
  /** Shown (instead of highlighted JSON) when the value is absent — e.g. body evicted. */
  emptyLabel?: string;
  className?: string;
  /** Forwarded to the SCROLLABLE inner element so the parent can scroll-sync the panes. */
  scrollRef?: React.RefObject<HTMLDivElement>;
  onScroll?: React.UIEventHandler<HTMLDivElement>;
}

export function JsonPane({
  value,
  diff,
  side = 'right',
  label,
  emptyLabel = 'body evicted',
  className,
  scrollRef,
  onScroll,
}: JsonPaneProps) {
  const codeRef = useRef<HTMLElement>(null);

  // Build the highlighted, tinted lines imperatively. Re-runs when the value/diff/side change;
  // the cleanup empties the node so a re-run or StrictMode remount never stacks duplicate DOM.
  useImperativeViz(
    codeRef,
    (el) => {
      ensureJsonLanguage();
      const lines = toJsonLines(value);
      const frag = document.createDocumentFragment();
      for (const line of lines) {
        const div = document.createElement('div');
        div.className = 'json-line';
        div.dataset.path = line.path;
        const kind = diff?.get(line.path);
        const tint = tintFor(kind, side);
        if (tint && kind) {
          // `background` (not `backgroundColor`) so a composite kind's gradient applies (finding 5).
          div.style.background = tint;
          div.dataset.diff = kind;
        }
        try {
          div.innerHTML = hljs.highlight(line.text, { language: 'json', ignoreIllegals: true }).value || '​';
        } catch {
          // Defensive: a line highlighted in isolation should never throw, but if it does we
          // fall back to escaped plain text so the pane still renders.
          div.textContent = line.text || '​';
        }
        frag.appendChild(div);
      }
      el.replaceChildren(frag);
      return () => {
        // FULL teardown (StrictMode safety): drop every highlighted node + injected HTML.
        el.replaceChildren();
      };
    },
    [value, diff, side],
  );

  const hasValue = value !== undefined;

  return (
    <div className={`flex min-h-0 flex-col ${className ?? ''}`} data-testid={`jsonpane-${label}`}>
      <div className="flex items-center justify-between border-b border-line bg-panel-raised px-3 py-1.5">
        <span className="text-xs font-medium uppercase tracking-wide text-text-muted">{label}</span>
      </div>
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="min-h-0 flex-1 overflow-auto bg-panel"
        data-testid={`jsonpane-scroll-${label}`}
      >
        {hasValue ? (
          <code
            ref={codeRef}
            className="hljs block whitespace-pre px-3 py-2 font-mono text-xs leading-relaxed"
            data-testid={`jsonpane-code-${label}`}
          />
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
