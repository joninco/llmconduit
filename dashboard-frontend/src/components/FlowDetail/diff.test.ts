import { describe, it, expect } from 'vitest';
import { diffLayers, deepEqual } from './diff';

/**
 * The structural diff is the heart of the inspector. These lock the per-JSON-path contract:
 * added/removed/changed are located at the precise path, key reordering is NOT a change, and a
 * missing layer yields an empty (untinted) map.
 */
describe('diffLayers — structural per-path classification', () => {
  it('classifies added / removed / changed / unchanged at the exact path', () => {
    const left = { model: 'gpt-4o', messages: [{ role: 'user', content: 'Hi' }], temperature: 0.7 };
    const right = { model: 'llama-3.1-70b', messages: [{ role: 'user', content: 'Hi' }], stream: true };
    const d = diffLayers(left, right);

    expect(d.get('$.model')).toBe('changed'); // gpt-4o → llama
    expect(d.get('$.temperature')).toBe('removed'); // dropped by the next layer
    expect(d.get('$.stream')).toBe('added'); // introduced by the next layer
    expect(d.get('$.messages[0].role')).toBe('unchanged');
    expect(d.get('$.messages[0].content')).toBe('unchanged');
    // The unchanged messages container is unchanged; the root changed (model differs).
    expect(d.get('$.messages')).toBe('unchanged');
    expect(d.get('$')).toBe('changed');
  });

  it('does NOT flag reordered object keys as a change (structural, not textual)', () => {
    const left = { a: 1, b: 2 };
    const right = { b: 2, a: 1 };
    const d = diffLayers(left, right);
    expect(d.get('$')).toBe('unchanged');
    expect(d.get('$.a')).toBe('unchanged');
    expect(d.get('$.b')).toBe('unchanged');
  });

  it('flags a type change (scalar→object) at the path without walking replaced children', () => {
    const left = { tool: 'name' };
    const right = { tool: { name: 'x' } };
    const d = diffLayers(left, right);
    expect(d.get('$.tool')).toBe('changed');
    // The whole subtree was replaced; children are not separately tinted.
    expect(d.get('$.tool.name')).toBeUndefined();
  });

  it('treats array element changes positionally', () => {
    const left = { xs: [1, 2, 3] };
    const right = { xs: [1, 9, 3, 4] };
    const d = diffLayers(left, right);
    expect(d.get('$.xs[0]')).toBe('unchanged');
    expect(d.get('$.xs[1]')).toBe('changed');
    expect(d.get('$.xs[2]')).toBe('unchanged');
    expect(d.get('$.xs[3]')).toBe('added');
  });

  it('returns an EMPTY map when a layer is missing (evicted body ⇒ untinted)', () => {
    expect(diffLayers(undefined, { a: 1 }).size).toBe(0);
    expect(diffLayers({ a: 1 }, undefined).size).toBe(0);
  });
});

describe('deepEqual', () => {
  it('compares objects key-wise and arrays positionally', () => {
    expect(deepEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] })).toBe(true);
    expect(deepEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 3 }] })).toBe(false);
    expect(deepEqual([1, 2], [1, 2, 3])).toBe(false);
  });
});
