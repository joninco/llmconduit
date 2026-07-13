import { chromium } from '@playwright/test';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { startProductionWorkload } from './production-workload.mjs';
import { encodeDeliveryMp4, HighQualityRecorder } from './high-quality-recorder.mjs';
import {
  clearSpotlight,
  fancyClick,
  installDemoEffects,
  showCaption,
  showChapter,
  showSlate,
  spotlight,
} from './demo-effects.mjs';

const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
const origin = (process.env.ARGUS_PRODUCTION_ORIGIN || 'http://192.168.1.15:5022').replace(/\/$/, '');
const apiOrigin = (process.env.ARGUS_API_ORIGIN || origin).replace(/\/$/, '');
const dashboardToken = process.env.LLMCONDUIT_DASHBOARD_TOKEN;
const outputRoot = resolve(process.env.ARGUS_DEMO_OUTPUT_DIR || 'test-results/production-demo');
const runId = new Date().toISOString().replace(/[:.]/g, '-');
const runDir = resolve(outputRoot, runId);
const sourcePath = resolve(runDir, 'argus-production-demo-lossless.mkv');
const mp4Path = resolve(runDir, 'argus-production-demo.mp4');
const bitrateKbps = Number.parseInt(process.env.ARGUS_DEMO_VIDEO_KBPS || '2150', 10);

if (!dashboardToken) {
  throw new Error('LLMCONDUIT_DASHBOARD_TOKEN is required (source /etc/llmconduit/dashboard.env without printing it)');
}

await mkdir(runDir, { recursive: true });

async function resolveLiveModel() {
  const response = await fetch(`${apiOrigin}/v1/models`);
  if (!response.ok) throw new Error(`model catalog returned HTTP ${response.status}`);
  const payload = await response.json();
  const model = process.env.ARGUS_DEMO_MODEL || payload?.data?.[0]?.id;
  if (!model) throw new Error('production model catalog is empty');
  return model;
}

const VIEW_IDS = {
  Overview: 'overview-view',
  Theater: 'theater-view',
  Flows: 'flows-view',
  Topology: 'topology-view',
  Sankey: 'sankey-view',
};

async function openView(page, name) {
  await fancyClick(page, page.getByRole('navigation').getByRole('tab', { name, exact: true }));
  await page.getByTestId(VIEW_IDS[name]).first().waitFor({ state: 'visible', timeout: 15_000 });
  await sleep(1_200);
}

async function spotlightCaption(page, locator, title, detail, holdMs, kicker) {
  await spotlight(page, locator);
  await showCaption(page, title, detail, holdMs, kicker);
  await clearSpotlight(page);
}

async function waitForFlow(requestContext, clientLabel, timeoutMs = 45_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const response = await requestContext.get(
      `${origin}/dashboard/api/flows?client=${encodeURIComponent(clientLabel)}&limit=5&sort=started&direction=desc`,
    );
    if (response.ok()) {
      const payload = await response.json();
      const flow = payload?.flows?.find((item) => item.client_label === clientLabel);
      if (flow) return flow;
    }
    await sleep(1_000);
  }
  throw new Error(`timed out waiting for production flow ${clientLabel}`);
}

const model = await resolveLiveModel();
const runLabel = `argus-demo-${runId.slice(0, 19)}`;
const browser = await chromium.launch({ headless: process.env.ARGUS_DEMO_HEADED !== '1' });
const context = await browser.newContext({
  viewport: { width: 1920, height: 1080 },
  colorScheme: 'dark',
});

const consoleErrors = [];
const workloadEvents = [];
let workload = null;
let page = null;
let recorder = null;
let runError = null;
let workloadResults = [];

try {
  // Authenticate through the context request client before a page exists, so the dashboard token
  // is never painted into the recording. BrowserContext.request shares its cookie jar with pages.
  const login = await context.request.post(`${origin}/dashboard/login`, {
    headers: { origin, referer: `${origin}/dashboard` },
    data: { token: dashboardToken },
    timeout: 15_000,
  });
  if (!login.ok()) throw new Error(`production dashboard login returned HTTP ${login.status()}`);

  page = await context.newPage();
  page.on('console', (message) => {
    if (message.type() === 'error') consoleErrors.push(message.text());
  });
  page.on('pageerror', (error) => consoleErrors.push(`pageerror: ${error.message}`));
  await page.goto(`${origin}/dashboard#/overview`, { waitUntil: 'domcontentloaded', timeout: 30_000 });
  await page.getByRole('tab', { name: 'Overview', exact: true }).waitFor({ state: 'visible', timeout: 20_000 });
  await page.getByTestId('overview-view').waitFor({ state: 'visible', timeout: 20_000 });
  await installDemoEffects(page, model);
  recorder = await HighQualityRecorder.start(page, sourcePath, {
    width: 1920,
    height: 1080,
    quality: 100,
  });

  await showSlate(
    page,
    'ARGUS · Live Production Tour',
    `${model} serving real streaming requests through llmconduit`,
    4_000,
    'LIVE SYSTEM CINEMATIC',
  );

  workload = startProductionWorkload({
    apiOrigin,
    model,
    runLabel,
    onEvent: (event) => {
      workloadEvents.push({ at: new Date().toISOString(), ...event });
      console.log('WORKLOAD', event.key, event.phase, event.status || '');
    },
  });

  await showChapter(page, '01', 'CONTROL ROOM', 'ONE CUT · THE WHOLE SYSTEM');
  await spotlightCaption(
    page,
    page.getByRole('region', { name: 'Operational picture' }),
    'Control room overview',
    'A single server-authored cut combines gateway outcomes, provider attempts, engine telemetry, cost, clients, and token pressure.',
    2_800,
    'SYSTEM PULSE',
  );
  await sleep(6_000);

  await showChapter(page, '02', 'LIVE MODEL THEATER', 'REASONING · TOKENS · TOOLS');
  await openView(page, 'Theater');
  await spotlightCaption(
    page,
    page.getByTestId('theater-grid').first(),
    'Live model theater',
    'Real GLM 5.2 reasoning and output deltas stream into bounded per-request rivers while the companion workload runs.',
    2_800,
    'STREAMING NOW',
  );
  await sleep(12_000);

  await showChapter(page, '03', 'FLOW FORENSICS', 'EVERY HOP · EVERY TOKEN · EVERY MILLISECOND');
  await openView(page, 'Flows');
  await spotlightCaption(
    page,
    page.getByTestId('flow-table-scroll'),
    'Every request becomes an operational flow',
    'Newest-first rows expose client attribution, route identity, status, token economics, cost confidence, and end-to-end latency.',
    2_800,
    'REQUEST MATRIX',
  );
  const heroFlow = await waitForFlow(context.request, workload.clients.hero);
  const heroRow = page.getByTitle(heroFlow.api_call_id, { exact: true });
  await heroRow.waitFor({ state: 'visible', timeout: 15_000 });
  await fancyClick(page, heroRow);
  await page.getByTestId('flow-detail').waitFor({ state: 'visible', timeout: 15_000 });
  await spotlightCaption(
    page,
    page.getByTestId('request-summary-grid'),
    'Transformation inspector',
    'The inspector aligns inbound, normalized, and upstream JSON with measured phase timing, routing attempts, usage, and streamed deltas.',
    4_200,
    'REQUEST X-RAY',
  );
  await sleep(2_500);

  const timelineTab = page.getByRole('tab', { name: 'Timeline', exact: true });
  await fancyClick(page, timelineTab);
  await spotlightCaption(
    page,
    page.getByTestId('timeline'),
    'Measured lifecycle timeline',
    'Accept, normalization, routing, upstream bytes, client content, and terminal state are correlated on one request clock.',
    2_800,
    'PHASE TRACE',
  );
  await sleep(2_500);

  await fancyClick(page, page.getByRole('tab', { name: 'Captured I/O', exact: true }));
  await spotlightCaption(
    page,
    page.locator('#flow-detail-drawer-panel-captures'),
    'Bounded, redacted capture',
    'Operators can inspect diagnostic request and response surfaces without putting raw secrets or unbounded bodies in historical snapshots.',
    2_800,
    'SAFE INSPECTION',
  );
  await sleep(2_000);
  await fancyClick(page, page.getByTestId('detail-back'));

  await showChapter(page, '04', 'ROUTE TOPOLOGY', 'CLIENTS → GATEWAY → PROVIDERS');
  await openView(page, 'Topology');
  await spotlightCaption(
    page,
    page.getByTestId('topology-view'),
    'Provider topology',
    'Client populations connect to the gateway and provider health; latency percentiles count every attempt, including failed primaries.',
    2_800,
    'ROUTING FIELD',
  );
  await sleep(4_500);

  await showChapter(page, '05', 'TOKEN ECONOMICS', 'VOLUME · ROUTING · COST');
  await openView(page, 'Sankey');
  await spotlightCaption(
    page,
    page.getByTestId('sankey-view'),
    'Token and cost flow',
    'The Sankey keeps volume, model routing, providers, and cost confidence tied to the same scoped terminal population.',
    2_800,
    'ECONOMIC FLOW',
  );
  await sleep(4_500);

  await showChapter(page, '06', 'ROLLUP + TIME TRAVEL', 'LIVE NOW · DURABLE THEN');
  await openView(page, 'Overview');
  await spotlightCaption(
    page,
    page.getByRole('region', { name: 'Flow and provider health' }),
    'The completed workload rolls up live',
    'Success, cancellation, tokens, latency, throughput, and priced usage update from the real requests generated during this recording.',
    2_800,
    'LIVE ROLLUP',
  );
  await sleep(4_000);

  const scrubber = page.getByTestId('scrubber-track');
  await spotlight(page, scrubber);
  await scrubber.focus();
  await scrubber.press('ArrowLeft');
  await page.getByTestId('live-toggle').waitFor({ state: 'visible', timeout: 15_000 });
  await showCaption(
    page,
    'Durable time travel',
    'Seeking freezes flow summaries, metrics, topology, and transcripts at one historical cut while live frames buffer safely in the background.',
    4_000,
    'HISTORICAL CUT',
  );
  await clearSpotlight(page);
  await fancyClick(page, page.getByTestId('live-toggle'));
  await page.getByTestId('live-indicator').waitFor({ state: 'visible', timeout: 15_000 });
  await sleep(2_500);

  workloadResults = await Promise.race([
    workload.finished,
    sleep(40_000).then(() => null),
  ]);
  if (workloadResults === null) {
    workload.abort();
    workloadResults = await workload.finished;
  }

  await showSlate(
    page,
    'Real traffic. One coherent operational story.',
    'ARGUS · llmconduit · live production telemetry',
    4_000,
    'MISSION COMPLETE',
  );
} catch (error) {
  runError = error;
  console.error('DEMO_ERROR', error instanceof Error ? error.stack : error);
} finally {
  workload?.abort();
  if (workload) workloadResults = await workload.finished;
  try {
    await recorder?.stop();
  } catch (error) {
    runError ??= error;
  }
  await context.close();
  await browser.close();
}

const delivery = await encodeDeliveryMp4({
  sourcePath,
  outputPath: mp4Path,
  preset: process.env.ARGUS_DEMO_VIDEO_PRESET || 'slow',
  bitrateKbps,
});
const keepSource = process.env.ARGUS_DEMO_KEEP_SOURCE === '1';
if (!keepSource) await rm(sourcePath, { force: true });

await writeFile(resolve(runDir, 'run.json'), JSON.stringify({
  runId,
  origin,
  apiOrigin,
  model,
  runLabel,
  workloadResults,
  workloadEvents,
  consoleErrors,
  error: runError instanceof Error ? runError.message : runError,
  video: {
    width: 1920,
    height: 1080,
    fps: 25,
    source_quality: 100,
    delivery_bitrate_kbps: delivery.bitrateKbps,
    delivery_size_mb: delivery.sizeMb,
    target_size_mb: [20, 30],
  },
  artifacts: { source: keepSource ? sourcePath : null, mp4: mp4Path },
}, null, 2));

console.log('VIDEO_MP4', mp4Path);
console.log('CONSOLE_ERRORS', consoleErrors.length);
if (runError) throw runError;
