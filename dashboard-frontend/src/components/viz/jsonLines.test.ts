import { describe, it, expect } from 'vitest';
import { toJsonLines } from './jsonLines';

/** The serializer bridges the parsed tree (diff walks) to text lines (highlight.js colors). */
describe('toJsonLines — path-tagged pretty-print', () => {
  it('tags each line with the canonical JSON path of its value', () => {
    const lines = toJsonLines({ model: 'gpt-4o', messages: [{ role: 'user' }] });
    const byPath = new Map(lines.map((l) => [l.path, l.text.trim()]));
    expect(byPath.has('$')).toBe(true);
    expect(byPath.get('$.model')).toBe('"model": "gpt-4o",');
    expect(byPath.has('$.messages')).toBe(true);
    expect(byPath.get('$.messages[0].role')).toBe('"role": "user"');
  });

  it('matches JSON.stringify layout (2-space indent) modulo path tags', () => {
    const value = { a: 1, b: [2, 3] };
    const text = toJsonLines(value).map((l) => l.text).join('\n');
    expect(text).toBe(JSON.stringify(value, null, 2));
  });

  it('renders empty containers on one line', () => {
    expect(toJsonLines({ a: {}, b: [] }).map((l) => l.text).join('\n')).toBe(
      JSON.stringify({ a: {}, b: [] }, null, 2),
    );
  });

  it('returns NO lines for undefined (evicted body placeholder handled by caller)', () => {
    expect(toJsonLines(undefined)).toEqual([]);
  });

  it('encodes a key containing a dot the SAME way the diff does (finding 6)', () => {
    // The serializer shares `pathKey` with the diff, so a dotted key tags as `$["a.b"]` —
    // matching the diff map key, so the tint for that line still lands.
    const lines = toJsonLines({ 'a.b': 1 });
    const paths = new Set(lines.map((l) => l.path));
    expect(paths.has('$["a.b"]')).toBe(true);
    expect(paths.has('$.a.b')).toBe(false);
  });
});
