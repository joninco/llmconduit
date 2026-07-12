import { describe, expect, it } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { GLOSSARY, Term, type GlossaryTerm } from './glossary';

describe('glossary (U10)', () => {
  it('every term has a non-trivial definition', () => {
    for (const [term, def] of Object.entries(GLOSSARY)) {
      expect(def.length, term).toBeGreaterThan(20);
    }
  });

  it('Term renders a focusable abbr with the definition as tooltip + accessible description', () => {
    const { getByTestId } = render(<Term term="server cut">cut</Term>);
    const el = getByTestId('term-server-cut');
    expect(el.tagName).toBe('ABBR');
    expect(el.getAttribute('title')).toBe(GLOSSARY['server cut']);
    expect(el.getAttribute('tabindex')).toBe('0');
    expect(el.getAttribute('aria-label')).toContain('cut:');
    cleanup();
  });

  it('derives an aria-label from the term key when the child is an element', () => {
    const { getByTestId } = render(<Term term="measured"><span>MEASURED</span></Term>);
    expect(getByTestId('term-measured').getAttribute('aria-label')).toBe(`measured: ${GLOSSARY.measured}`);
    cleanup();
  });

  it('falls back to the term itself as content', () => {
    const { getByTestId } = render(<Term term={'m1' as GlossaryTerm} />);
    expect(getByTestId('term-m1').textContent).toBe('m1');
    cleanup();
  });
});
