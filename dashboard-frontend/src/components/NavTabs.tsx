import { ROUTES, navigate, type RouteName } from '../router/useHashRoute';
import { Button } from './ui/Button';
import { cn } from '../lib/cn';
import { useRef, type KeyboardEvent } from 'react';

const LABELS: Record<RouteName, string> = {
  flows: 'Flows',
  topology: 'Topology',
  sankey: 'Sankey',
  theater: 'Theater',
  overview: 'Overview',
};

/** The Argus eye — the hundred-eyed watchman's iris, the brand mark. Keeps a slow watch. */
function ArgusEye({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 24 24" fill="none" className={className} aria-hidden="true">
      <path
        d="M1.6 12S5.2 5.6 12 5.6 22.4 12 22.4 12 18.8 18.4 12 18.4 1.6 12 1.6 12Z"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
      />
      <circle cx="12" cy="12" r="3.4" stroke="currentColor" strokeWidth="1.5" />
      <circle cx="12" cy="12" r="1.25" fill="currentColor" />
    </svg>
  );
}

export function NavTabs({ active, onLogout }: { active: RouteName; onLogout: () => void }) {
  const tabs = useRef<Array<HTMLButtonElement | null>>([]);

  function onTabKeyDown(event: KeyboardEvent<HTMLButtonElement>, index: number): void {
    let next = index;
    if (event.key === 'ArrowRight') next = (index + 1) % ROUTES.length;
    else if (event.key === 'ArrowLeft') next = (index - 1 + ROUTES.length) % ROUTES.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = ROUTES.length - 1;
    else return;
    event.preventDefault();
    const route = ROUTES[next]!;
    navigate(route);
    requestAnimationFrame(() => tabs.current[next]?.focus());
  }

  return (
    <nav className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-line bg-panel px-3 py-2.5 sm:flex-nowrap sm:px-5">
      {/* Masthead: the Argus eye + tracked wordmark; llmconduit rides below as the eyebrow. */}
      <div className="flex items-center gap-2.5 pr-1">
        <ArgusEye className="argus-eye h-[18px] w-[18px] text-accent" />
        <div className="leading-none">
          <div className="font-ui text-sm font-bold tracking-[0.24em] text-text">ARGUS</div>
          <div className="mt-1 font-mono text-[9px] uppercase tracking-[0.22em] text-text-muted">
            llmconduit · watch
          </div>
        </div>
      </div>
      <div className="order-3 flex w-full snap-x snap-mandatory items-center gap-1 overflow-x-auto sm:order-none sm:w-auto" role="tablist" aria-label="Dashboard views">
        {ROUTES.map((r, index) => (
          <button
            key={r}
            ref={(element) => { tabs.current[index] = element; }}
            onClick={() => navigate(r)}
            onKeyDown={(event) => onTabKeyDown(event, index)}
            role="tab"
            aria-selected={r === active}
            aria-current={r === active ? 'page' : undefined}
            tabIndex={r === active ? 0 : -1}
            className={cn(
              'shrink-0 snap-start rounded-md px-3 py-1.5 text-xs font-medium uppercase tracking-[0.14em] transition-colors',
              r === active
                ? 'bg-accent/12 text-accent'
                : 'text-text-muted hover:bg-line/40 hover:text-text',
            )}
          >
            {LABELS[r]}
          </button>
        ))}
      </div>
      <Button variant="ghost" className="ml-auto" onClick={onLogout}>
        Logout
      </Button>
    </nav>
  );
}
