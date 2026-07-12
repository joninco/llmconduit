import { describe, expect, it } from 'vitest';
import { splitJsonTail } from './riverModel';

describe('splitJsonTail (U8 — tool-card JSON detection)', () => {
  it('splits a prefixed JSON object', () => {
    const r = splitJsonTail('tool arguments chatcmpl-tool-9cd5: {"command": "ls", "description": "list"}');
    expect(r).not.toBeNull();
    expect(r!.prefix).toBe('tool arguments chatcmpl-tool-9cd5:');
    expect(r!.value).toEqual({ command: 'ls', description: 'list' });
  });

  it('handles a bare JSON array', () => {
    const r = splitJsonTail('[1, 2, 3]');
    expect(r!.prefix).toBe('');
    expect(r!.value).toEqual([1, 2, 3]);
  });

  it('returns null for plain text and for malformed JSON', () => {
    expect(splitJsonTail('completed')).toBeNull();
    expect(splitJsonTail('output {not json')).toBeNull();
  });
});
