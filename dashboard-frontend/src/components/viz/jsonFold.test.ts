import { describe, it, expect } from 'vitest';
import { toJsonLines } from './jsonLines';
import { buildFoldModel, computeRows } from './jsonFold';

const VALUE = { a: 1, b: { c: 2 }, d: [1, 2] };

describe('buildFoldModel — bracket matching + ancestry', () => {
  const lines = toJsonLines(VALUE);
  const model = buildFoldModel(lines);

  it('pairs each container open line with its close + counts immediate members', () => {
    const byPath = (p: string) => [...model.blocksByOpen.values()].find((b) => lines[b.openIndex].path === p);
    expect(model.containerPaths.sort()).toEqual(['$', '$.b', '$.d']);
    expect(byPath('$')?.childCount).toBe(3); // a, b, d
    expect(byPath('$.b')?.childCount).toBe(1); // c
    expect(byPath('$.d')?.childCount).toBe(2); // [0], [1]
    const root = byPath('$')!;
    expect(lines[root.closeIndex].text.trim()).toBe('}');
  });

  it('records ancestor container open-indices per line', () => {
    const cIdx = lines.findIndex((l) => l.path === '$.b.c');
    const ancestors = model.ancestorsByLine[cIdx].map((i) => lines[i].path);
    expect(ancestors).toEqual(['$', '$.b']);
  });
});

describe('computeRows — fold mode', () => {
  const lines = toJsonLines(VALUE);
  const model = buildFoldModel(lines);

  it('renders the full document expanded by default', () => {
    const { rows, matchCount } = computeRows(lines, model, new Set(), '');
    expect(rows).toHaveLength(lines.length);
    expect(matchCount).toBe(0);
  });

  it('collapses a container to one summary row and hides its descendants + close', () => {
    const { rows } = computeRows(lines, model, new Set(['$.b']), '');
    const paths = rows.map((r) => r.line.path);
    expect(paths).not.toContain('$.b.c'); // descendant hidden
    const bRow = rows.find((r) => r.line.path === '$.b' && r.folded);
    expect(bRow?.folded).toBe(true);
    expect(bRow?.block?.childCount).toBe(1);
    // exactly one row carries the $.b path now (the folded open line; close folded in)
    expect(paths.filter((p) => p === '$.b')).toHaveLength(1);
  });
});

describe('computeRows — search mode', () => {
  const lines = toJsonLines(VALUE);
  const model = buildFoldModel(lines);

  it('keeps matches plus their ancestor containers, ignoring fold state', () => {
    const { rows, matchCount } = computeRows(lines, model, new Set(['$.b']), 'c');
    expect(matchCount).toBe(1);
    const paths = rows.map((r) => r.line.path);
    expect(paths).toEqual(['$', '$.b', '$.b.c']); // match + ancestors, in order
    expect(rows.find((r) => r.line.path === '$.b.c')?.isMatch).toBe(true);
    expect(rows.find((r) => r.line.path === '$')?.isMatch).toBe(false); // ancestor context only
  });

  it('is case-insensitive and matches on value text', () => {
    const { matchCount } = computeRows(toJsonLines({ Model: 'GPT-4o' }), buildFoldModel(toJsonLines({ Model: 'GPT-4o' })), new Set(), 'gpt');
    expect(matchCount).toBe(1);
  });
});
