/**
 * Fold + search model over the flat, path-tagged lines from `toJsonLines`.
 *
 * The inspector serializes each JSON layer into `JsonLine[]` (text + canonical path + depth).
 * This module turns that flat list into a foldable, searchable view WITHOUT re-parsing: it
 * pairs each container's opening line with its closing line (a bracket-matching stack pass),
 * records each line's ancestor containers, and computes the visible row order given a set of
 * collapsed paths OR an active search query. Pure + synchronous so it unit-tests directly and
 * memoizes cleanly in `JsonPane`.
 */
import type { JsonLine } from './jsonLines';

/** Opening-bracket line (`…{` / `…[`) that is NOT an empty `{}` / `[]` single-liner. */
function isOpenLine(text: string): boolean {
  return /[{[]\s*$/.test(text);
}

/** Closing-bracket line — trimmed text starts with `}` or `]` (a value never legally does). */
export function isCloseLine(text: string): boolean {
  return /^\s*[}\]]/.test(text);
}

/** One foldable container: its open/close line indices, child count, and bracket glyphs. */
export interface LineBlock {
  openIndex: number;
  closeIndex: number;
  /** Number of immediate members (object keys / array elements), for the collapsed summary. */
  childCount: number;
  bracketOpen: '{' | '[';
  bracketClose: '}' | ']';
  /** Trailing punctuation on the close line (`,` or ``), so a folded summary keeps the comma. */
  closeSuffix: string;
}

export interface FoldModel {
  /** Open-line index → its block. A line is foldable iff it is a key here. */
  blocksByOpen: Map<number, LineBlock>;
  /** Per line index → the open-line indices of its ancestor containers (outermost → innermost). */
  ancestorsByLine: number[][];
  /** Canonical paths of every foldable container (for collapse-all / expand-all). */
  containerPaths: string[];
}

/** Bracket-match the flat lines into fold blocks + per-line ancestry. */
export function buildFoldModel(lines: JsonLine[]): FoldModel {
  const blocksByOpen = new Map<number, LineBlock>();
  const ancestorsByLine: number[][] = [];
  const stack: number[] = [];

  lines.forEach((line, i) => {
    ancestorsByLine[i] = stack.slice();
    if (isCloseLine(line.text)) {
      const openIdx = stack.pop();
      if (openIdx !== undefined) {
        const blk = blocksByOpen.get(openIdx);
        if (blk) {
          blk.closeIndex = i;
          blk.closeSuffix = line.text.trim().slice(1); // after `}` / `]` → `,` or ``
        }
      }
    }
    if (isOpenLine(line.text)) {
      const open = line.text.trimEnd().slice(-1) === '{' ? '{' : '[';
      blocksByOpen.set(i, {
        openIndex: i,
        closeIndex: i,
        childCount: 0,
        bracketOpen: open,
        bracketClose: open === '{' ? '}' : ']',
        closeSuffix: '',
      });
      stack.push(i);
    }
  });

  // Immediate members = lines one level deeper that are not themselves close brackets.
  for (const blk of blocksByOpen.values()) {
    const depth = lines[blk.openIndex]!.depth;
    let count = 0;
    for (let k = blk.openIndex + 1; k < blk.closeIndex; k++) {
      const l = lines[k]!;
      if (l.depth === depth + 1 && !isCloseLine(l.text)) count++;
    }
    blk.childCount = count;
  }

  const containerPaths = [...blocksByOpen.values()].map((b) => lines[b.openIndex]!.path);
  return { blocksByOpen, ancestorsByLine, containerPaths };
}

/** A row to render: the source line plus its fold/search state. */
export interface FoldRow {
  index: number;
  line: JsonLine;
  /** This line opens a container (render a chevron). */
  foldable: boolean;
  /** Rendered collapsed (descendants hidden, show summary). Never true in search mode. */
  folded: boolean;
  /** Matched the active query (search mode only). */
  isMatch: boolean;
  block?: LineBlock;
}

export interface RowsResult {
  rows: FoldRow[];
  matchCount: number;
}

/**
 * The visible rows in render order.
 *  - Search mode (non-empty query): every line whose text contains the query (case-insensitive)
 *    PLUS its ancestor container lines (for context), fold state ignored, matches flagged.
 *  - Fold mode: the full document, except collapsed containers render as a single summary row
 *    and their descendants (and close line) are skipped.
 */
export function computeRows(
  lines: JsonLine[],
  model: FoldModel,
  collapsed: ReadonlySet<string>,
  query: string,
): RowsResult {
  const q = query.trim().toLowerCase();

  if (q.length > 0) {
    const matched = new Set<number>();
    lines.forEach((line, i) => {
      if (line.text.toLowerCase().includes(q)) matched.add(i);
    });
    const keep = new Set<number>();
    for (const m of matched) {
      keep.add(m);
      for (const a of model.ancestorsByLine[m] ?? []) keep.add(a);
    }
    const rows = [...keep]
      .sort((a, b) => a - b)
      .map((i) => ({
        index: i,
        line: lines[i]!,
        foldable: model.blocksByOpen.has(i),
        folded: false,
        isMatch: matched.has(i),
        block: model.blocksByOpen.get(i),
      }));
    return { rows, matchCount: matched.size };
  }

  const rows: FoldRow[] = [];
  let i = 0;
  while (i < lines.length) {
    const block = model.blocksByOpen.get(i);
    const line = lines[i]!;
    if (block && collapsed.has(line.path)) {
      rows.push({ index: i, line, foldable: true, folded: true, isMatch: false, block });
      i = block.closeIndex + 1;
    } else {
      rows.push({ index: i, line, foldable: !!block, folded: false, isMatch: false, block });
      i++;
    }
  }
  return { rows, matchCount: 0 };
}
