import { describe, expect, it } from 'vitest';
import { formatStaleAge } from './staleAge';

describe('formatStaleAge', () => {
  it('uses the shared fixed-width minute/hour/day clock and clamps future timestamps', () => {
    expect(formatStaleAge(-1)).toBe('00:00');
    expect(formatStaleAge(65_999)).toBe('01:05');
    expect(formatStaleAge(3_661_000)).toBe('01:01:01');
    expect(formatStaleAge(90_061_000)).toBe('1d 01:01:01');
  });
});
