import { useEffect, useState, type RefObject } from 'react';

/** Width-only ResizeObserver bridge for imperative SVG/canvas surfaces. */
export function useObservedWidth(ref: RefObject<HTMLElement>): number | null {
  const [width, setWidth] = useState<number | null>(null);

  useEffect(() => {
    const element = ref.current;
    if (!element || typeof ResizeObserver !== 'function') return;
    const commit = (next: number) => {
      if (!Number.isFinite(next) || next <= 0) return;
      setWidth((current) => (current !== null && Math.abs(current - next) < 1 ? current : next));
    };
    commit(element.getBoundingClientRect().width);
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) commit(entry.contentRect.width);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref]);

  return width;
}
