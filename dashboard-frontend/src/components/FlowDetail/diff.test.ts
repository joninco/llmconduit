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

  it('marks EVERY descendant of an added subtree (finding 3 — nested tinting)', () => {
    const left = { model: 'x' };
    const right = { model: 'x', tools: [{ name: 'search', params: { q: 'hi' } }] };
    const d = diffLayers(left, right);
    // The whole `tools` subtree is new: the parent AND every nested path are `added`,
    // so JsonPane tints each line of the added subtree (not just its opening bracket).
    expect(d.get('$.tools')).toBe('added');
    expect(d.get('$.tools[0]')).toBe('added');
    expect(d.get('$.tools[0].name')).toBe('added');
    expect(d.get('$.tools[0].params')).toBe('added');
    expect(d.get('$.tools[0].params.q')).toBe('added');
  });

  it('marks EVERY descendant of a removed subtree (finding 3)', () => {
    const left = { model: 'x', meta: { tags: ['a', 'b'] } };
    const right = { model: 'x' };
    const d = diffLayers(left, right);
    expect(d.get('$.meta')).toBe('removed');
    expect(d.get('$.meta.tags')).toBe('removed');
    expect(d.get('$.meta.tags[0]')).toBe('removed');
    expect(d.get('$.meta.tags[1]')).toBe('removed');
  });

  it('does NOT collide keys containing a dot with nested paths (finding 6)', () => {
    // `{"a.b": 1}` and `{"a": {"b": 2}}` would BOTH be `$.a.b` under naive dotting; the
    // dotted key must encode distinctly so the two are classified independently.
    const left = { 'a.b': 1, a: { b: 2 } };
    const right = { 'a.b': 1, a: { b: 999 } }; // only the NESTED a.b changed
    const d = diffLayers(left, right);
    // The literal-dotted key is unchanged…
    expect(d.get('$["a.b"]')).toBe('unchanged');
    // …while the genuinely nested path is the one flagged changed (no cross-contamination).
    expect(d.get('$.a.b')).toBe('changed');
  });

  it('encodes keys containing brackets distinctly too (finding 6)', () => {
    const d = diffLayers({ 'x[0]': 1 }, { 'x[0]': 2 });
    expect(d.get('$["x[0]"]')).toBe('changed');
    // It must NOT be mistaken for an array index path.
    expect(d.get('$.x[0]')).toBeUndefined();
  });
});

describe('deepEqual', () => {
  it('compares objects key-wise and arrays positionally', () => {
    expect(deepEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] })).toBe(true);
    expect(deepEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 3 }] })).toBe(false);
    expect(deepEqual([1, 2], [1, 2, 3])).toBe(false);
  });
});
