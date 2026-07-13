const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const REQUESTS = [
  {
    key: 'hero',
    delayMs: 0,
    maxOutputTokens: 1_600,
    effort: 'medium',
    prompt: `You are the principal systems architect for an autonomous Antarctic research station
that may lose satellite connectivity for 30 days. Design a fault-tolerant local AI operations
system that can keep power, thermal control, scientific instruments, and crew communications safe.

Produce a concrete, technically rigorous response with:
1. a short executive summary;
2. an ASCII architecture diagram;
3. a decision table for normal, degraded, isolated, and emergency modes;
4. a compact Rust state-machine example;
5. a failure-mode table with detection signals and mitigations; and
6. a red-team critique that identifies the weakest design choice and revises it.

State your assumptions, make tradeoffs explicit, and keep the final response under 1,000 words.`,
  },
  {
    key: 'sre',
    delayMs: 2_500,
    maxOutputTokens: 700,
    effort: 'low',
    prompt: `Act as the SRE reviewing an autonomous Antarctic research station. Give the five most
dangerous cascading failures in its local AI operations stack. For each, specify the leading
indicator, alert, automated containment, human fallback, recovery-time objective, and one chaos
test. Finish with the single metric you would put above every other metric in the control room.`,
  },
  {
    key: 'rust',
    delayMs: 4_500,
    maxOutputTokens: 800,
    effort: 'low',
    prompt: `Implement a compact but realistic Rust state machine for an isolated Antarctic station.
It must move between Normal, Degraded, Isolated, and Emergency states from typed telemetry events,
apply hysteresis to avoid flapping, preserve an audit trail, and include three focused unit tests.
Explain the two most important safety invariants after the code.`,
  },
  {
    key: 'cancelled',
    delayMs: 6_000,
    maxOutputTokens: 2_000,
    effort: 'high',
    cancelAfterMs: 4_000,
    prompt: `Explore ten substantially different architectures for a self-healing autonomous polar
research station. Analyze every design in depth, compare all pairwise tradeoffs, then synthesize a
complete implementation blueprint. Think carefully before answering.`,
  },
];

async function consumeStreamingResponse({ apiOrigin, model, clientLabel, request, signal }) {
  const response = await fetch(`${apiOrigin}/v1/responses`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'user-agent': clientLabel,
    },
    body: JSON.stringify({
      model,
      input: request.prompt,
      stream: true,
      // A production demo must exercise the backend, not replay a prior response.
      store: false,
      temperature: 0.2,
      max_output_tokens: request.maxOutputTokens,
      reasoning: { effort: request.effort, summary: 'auto' },
    }),
    signal,
  });

  if (!response.ok) {
    const body = await response.text();
    throw new Error(`${request.key} returned HTTP ${response.status}: ${body.slice(0, 300)}`);
  }

  if (!response.body) throw new Error(`${request.key} returned no streaming body`);
  const reader = response.body.getReader();
  let bytes = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    bytes += value.byteLength;
  }
  return bytes;
}

/**
 * Start a bounded set of real streaming Responses API calls. Each request gets a unique
 * User-Agent label so the production recorder can find the corresponding flow without reading
 * captured prompt bodies or sending credentials through the page.
 */
export function startProductionWorkload({ apiOrigin, model, runLabel, onEvent = () => {} }) {
  const controllers = new Map();
  let stopped = false;
  const clients = Object.fromEntries(REQUESTS.map((request) => [request.key, `${runLabel}/${request.key}`]));

  const tasks = REQUESTS.map(async (request) => {
    await sleep(request.delayMs);
    if (stopped) return { key: request.key, status: 'skipped', bytes: 0, elapsedMs: 0 };

    const controller = new AbortController();
    controllers.set(request.key, controller);
    const started = Date.now();
    const cancelTimer = request.cancelAfterMs
      ? setTimeout(() => controller.abort(new Error('planned demo cancellation')), request.cancelAfterMs)
      : null;
    onEvent({ key: request.key, phase: 'started', clientLabel: clients[request.key] });

    try {
      const bytes = await consumeStreamingResponse({
        apiOrigin,
        model,
        clientLabel: clients[request.key],
        request,
        signal: controller.signal,
      });
      const result = { key: request.key, status: 'completed', bytes, elapsedMs: Date.now() - started };
      onEvent({ ...result, phase: 'finished' });
      return result;
    } catch (error) {
      if (controller.signal.aborted) {
        const result = { key: request.key, status: 'cancelled', bytes: 0, elapsedMs: Date.now() - started };
        onEvent({ ...result, phase: 'finished' });
        return result;
      }
      const message = error instanceof Error ? error.message : String(error);
      onEvent({ key: request.key, phase: 'failed', message });
      return { key: request.key, status: 'failed', bytes: 0, elapsedMs: Date.now() - started, message };
    } finally {
      if (cancelTimer) clearTimeout(cancelTimer);
      controllers.delete(request.key);
    }
  });

  return {
    clients,
    finished: Promise.all(tasks),
    abort() {
      stopped = true;
      for (const controller of controllers.values()) controller.abort(new Error('demo finished'));
    },
  };
}
