import { describe, expect, it } from 'vitest';
import { formatStaleAge } from './staleAge';

describe('formatStaleAge (U8 — durations, not clock times)', () => {
  it('renders human durations and clamps future timestamps', () => {
    expect(formatStaleAge(-1)).toBe('0s');
    expect(formatStaleAge(42_000)).toBe('42s');
    expect(formatStaleAge(65_999)).toBe('1m 5s');
    expect(formatStaleAge(1_072_000)).toBe('17m 52s');
    expect(formatStaleAge(3_661_000)).toBe('1h 1m');
    expect(formatStaleAge(90_061_000)).toBe('1d 1h');
  });
});
