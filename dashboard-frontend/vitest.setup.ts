import '@testing-library/jest-dom/vitest';

/**
 * jsdom does not implement `window.matchMedia`. uPlot calls it at construction time
 * (`setPxRatio`), and `prefersReducedMotion()` consults it. Provide a benign default
 * (`matches: false`) so viz tests can construct uPlot; individual tests that need a specific
 * media result (e.g. reduced-motion) override it with `vi.stubGlobal('matchMedia', …)`.
 */
if (typeof window !== 'undefined' && !window.matchMedia) {
  window.matchMedia = (query: string): MediaQueryList =>
    ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as unknown as MediaQueryList;
}

/**
 * jsdom implements neither `HTMLCanvasElement.prototype.getContext('2d')` nor `Path2D`. uPlot
 * (the sparkline renderer) constructs a 2D context and draws via Path2D on a DEFERRED commit
 * (after the test body), so per-test stubs would race teardown. We install permanent NO-OP
 * canvas polyfills here instead — harmless to non-canvas tests, and they let the real uPlot
 * create/draw/`destroy()` lifecycle run so the StrictMode dispose contract is testable without
 * pulling in the native `canvas` dependency.
 */
if (typeof globalThis !== 'undefined' && typeof (globalThis as { Path2D?: unknown }).Path2D === 'undefined') {
  class Path2DStub {
    addPath() {}
    moveTo() {}
    lineTo() {}
    rect() {}
    arc() {}
    closePath() {}
    bezierCurveTo() {}
    quadraticCurveTo() {}
  }
  (globalThis as { Path2D: unknown }).Path2D = Path2DStub;
}

// jsdom DEFINES `getContext` but it throws "Not implemented", so we replace it unconditionally
// (the test env never has a real 2D context) rather than guarding on its mere existence.
if (typeof HTMLCanvasElement !== 'undefined') {
  const ctx = new Proxy(
    {
      canvas: { width: 0, height: 0 },
      font: '',
      fillStyle: '',
      strokeStyle: '',
      lineWidth: 1,
      globalAlpha: 1,
      measureText: () => ({ width: 0 }),
      getImageData: () => ({ data: new Uint8ClampedArray(4) }),
      createLinearGradient: () => ({ addColorStop: () => {} }),
    } as Record<string, unknown>,
    {
      get(target, prop) {
        if (prop in target) return target[prop as string];
        // Any other 2D-context member is a no-op function (clearRect, beginPath, stroke, …).
        return () => undefined;
      },
      set(target, prop, value) {
        target[prop as string] = value;
        return true;
      },
    },
  );
  HTMLCanvasElement.prototype.getContext = function getContext(this: HTMLCanvasElement, type: string) {
    return type === '2d' ? (ctx as unknown as CanvasRenderingContext2D) : null;
  } as typeof HTMLCanvasElement.prototype.getContext;
}
