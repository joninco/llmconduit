import { ROUTES, navigate, type RouteName } from '../router/useHashRoute';
import { Button } from './ui/Button';
import { cn } from '../lib/cn';

const LABELS: Record<RouteName, string> = {
  flows: 'Flows',
  topology: 'Topology',
  sankey: 'Sankey',
  theater: 'Theater',
};

export function NavTabs({ active, onLogout }: { active: RouteName; onLogout: () => void }) {
  return (
    <nav className="flex items-center gap-1 border-b border-line bg-panel px-4 py-2">
      <span className="mr-4 font-semibold text-text">llmconduit</span>
      {ROUTES.map((r) => (
        <button
          key={r}
          onClick={() => navigate(r)}
          className={cn(
            'rounded-md px-3 py-1.5 text-sm transition-colors',
            r === active ? 'bg-accent/15 text-accent' : 'text-text-muted hover:text-text',
          )}
        >
          {LABELS[r]}
        </button>
      ))}
      <Button variant="ghost" className="ml-auto" onClick={onLogout}>
        Logout
      </Button>
    </nav>
  );
}
