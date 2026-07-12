import { describe, it, expect } from 'vitest';
import { parseBootstrap } from './env';
import { GOLDEN_BOOTSTRAP } from '../api/ws.fixtures';

describe('bootstrap parsing — frozen field names (finding 6)', () => {
  it('parses the GOLDEN bootstrap D7 embeds (authenticated/csrf_token/mutations_enabled)', () => {
    const boot = parseBootstrap(GOLDEN_BOOTSTRAP);
    expect(boot).toEqual({
      authenticated: true,
      csrf_token: 'csrf-abc123',
      mutations_enabled: true,
      schema_version: 6,
    });
  });

  it('accepts only the server-authored schema-v5 bootstrap', () => {
    expect(
      parseBootstrap({
        authenticated: true,
        csrf_token: 'csrf',
        mutations_enabled: false,
        schema_version: 6,
      }),
    ).toEqual({ authenticated: true, csrf_token: 'csrf', mutations_enabled: false, schema_version: 6 });
  });

  it('surfaces malformed contracts instead of silently coercing them', () => {
    expect(() => parseBootstrap({})).toThrow(/contract validation failed/);
    expect(() => parseBootstrap(null)).toThrow(/contract validation failed/);
    expect(() =>
      parseBootstrap({ authenticated: 'yes', csrf_token: 123, mutations_enabled: 1, schema_version: 6 }),
    ).toThrow(/contract validation failed/);
  });
});
