/**
 * Scrubber SLOT (under the stats strip). D9 provides the slot + the LIVE/seek toggle
 * wired to `DashboardSocket.seek()/live()`; the full time-travel timeline (snapshot
 * fetch, shadow-buffer depth indicator) is D11.
 */
import { useDashboard } from '../store/hooks';
import { Button } from './ui/Button';
import type { DashboardSocket } from '../api/ws';

export function Scrubber({ socket }: { socket: DashboardSocket }) {
  const connection = useDashboard((s) => s.connection);
  const paused = connection === 'seeking';

  return (
    <div className="mx-4 mt-2 flex items-center gap-3 rounded-md border border-line bg-panel px-3 py-2">
      <span className="text-xs uppercase tracking-wide text-text-muted">time-travel</span>
      {paused ? (
        <Button variant="default" onClick={() => socket.live()}>
          ▶ LIVE
        </Button>
      ) : (
        <Button variant="ghost" onClick={() => socket.seek()}>
          ⏸ pause
        </Button>
      )}
      <div className="h-1 flex-1 rounded-full bg-panel-raised">
        <div className="h-1 w-full rounded-full bg-line" />
      </div>
      <span className="tabular-nums text-xs text-text-muted">
        {paused ? `buffered ${socket.shadowBufferLength()}` : 'live'}
      </span>
    </div>
  );
}
