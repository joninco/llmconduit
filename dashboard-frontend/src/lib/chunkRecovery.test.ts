import { describe, expect, it } from 'vitest';
import {
  armChunkRecovery, chunkRecoveryStorageKey, clearChunkRecovery, isChunkLoadError,
} from './chunkRecovery';

describe('chunk recovery', () => {
  it.each([
    new Error('Failed to fetch dynamically imported module: /assets/Topology-a1.js'),
    Object.assign(new Error('Loading chunk 42 failed'), { name: 'ChunkLoadError' }),
    new TypeError('Importing a module script failed.'),
    new Error('Unable to preload CSS for /assets/index-a1.css'),
  ])('recognizes deployment-related lazy import failures', (error) => {
    expect(isChunkLoadError(error)).toBe(true);
  });

  it('does not classify unrelated render errors as chunk failures', () => {
    expect(isChunkLoadError(new Error('Cannot read properties of undefined'))).toBe(false);
  });

  it('arms once per exact route/query and clears after success', () => {
    expect(armChunkRecovery(sessionStorage, 'https://argus/#/flows?q=abc', 100)).toBe(true);
    expect(armChunkRecovery(sessionStorage, 'https://argus/#/flows?q=abc', 200)).toBe(false);
    expect(armChunkRecovery(sessionStorage, 'https://argus/#/topology', 200)).toBe(true);
    clearChunkRecovery(sessionStorage);
    expect(sessionStorage.getItem(chunkRecoveryStorageKey)).toBeNull();
  });

  it('expires a stale loop guard', () => {
    expect(armChunkRecovery(sessionStorage, 'https://argus/#/flows', 0)).toBe(true);
    expect(armChunkRecovery(sessionStorage, 'https://argus/#/flows', 6 * 60_000)).toBe(true);
  });
});
