import http from 'node:http';
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const frontend = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repo = path.resolve(frontend, '..');
const hostPort = Number(process.env.LLMCONDUIT_E2E_PORT ?? 5274);
const upstreamPort = Number(process.env.LLMCONDUIT_E2E_UPSTREAM_PORT ?? 5275);

await run('npm', ['run', 'build'], { cwd: frontend });
await run('cargo', ['build', '--bin', 'llmconduit'], {
  cwd: repo,
  env: { ...process.env, LLMCONDUIT_DASHBOARD_DIST: path.join(frontend, 'dist') },
});

const upstream = http.createServer(async (request, response) => {
  const url = new URL(request.url ?? '/', `http://${request.headers.host ?? 'localhost'}`);
  if (request.method === 'GET' && url.pathname === '/v1/models') {
    return json(response, 200, {
      object: 'list',
      data: [{ id: 'mock-model', object: 'model', owned_by: 'real-host-e2e', context_length: 32768 }],
    });
  }
  if (request.method === 'POST' && url.pathname === '/v1/chat/completions') {
    for await (const _chunk of request) { /* drain request body */ }
    response.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    });
    response.write('data: {"id":"chatcmpl-real-e2e","object":"chat.completion.chunk","created":1,"model":"mock-model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello from upstream"},"finish_reason":null}]}\n\n');
    response.write('data: {"id":"chatcmpl-real-e2e","object":"chat.completion.chunk","created":1,"model":"mock-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}\n\n');
    response.end('data: [DONE]\n\n');
    return;
  }
  json(response, 404, { error: { message: `unhandled mock route ${request.method} ${url.pathname}` } });
});
await listen(upstream, upstreamPort);

const temp = await mkdtemp(path.join(tmpdir(), 'llmconduit-dashboard-e2e-'));
const configPath = path.join(temp, 'config.yaml');
await writeFile(configPath, [
  `bind_addr: 127.0.0.1:${hostPort}`,
  `upstream_base_url: http://127.0.0.1:${upstreamPort}/v1`,
  'upstream_model: mock-model',
  'request_timeout_secs: 10',
  'connect_timeout_secs: 2',
  'min_completion_tokens: 1',
  'price_table:',
  '  mock-model:',
  '    input_per_1k: 0.001',
  '    output_per_1k: 0.002',
  '    cached_per_1k: 0.0005',
  '',
].join('\n'));

const binary = spawn(
  path.join(repo, 'target', 'debug', 'llmconduit'),
  ['--with-debug-ui', 'start', '--config', configPath],
  {
    cwd: repo,
    stdio: ['ignore', 'inherit', 'inherit'],
    env: {
      ...process.env,
      LLMCONDUIT_DASHBOARD_TOKEN: 'real-host-token',
      // Base64 for the deterministic 32-byte test key `real-host-playwright-session-key`.
      LLMCONDUIT_DASHBOARD_SESSION_KEY: 'cmVhbC1ob3N0LXBsYXl3cmlnaHQtc2Vzc2lvbi1rZXk=',
      LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS: '1',
      RUST_LOG: 'llmconduit=info',
    },
  },
);

let stopping = false;
const stop = () => {
  if (stopping) return;
  stopping = true;
  binary.kill('SIGTERM');
  upstream.close();
};
process.once('SIGINT', stop);
process.once('SIGTERM', stop);
process.once('exit', stop);
binary.once('exit', (code, signal) => {
  upstream.close();
  if (!stopping) {
    process.stderr.write(`llmconduit real-host process exited early (${code ?? signal})\n`);
    process.exit(code ?? 1);
  }
});

// Keep the webServer command alive for Playwright's lifetime.
await new Promise(() => {});

function json(response, status, body) {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(JSON.stringify(body));
}

function listen(server, port) {
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', resolve);
  });
}

function run(command, args, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: 'inherit', ...options });
    child.once('error', reject);
    child.once('exit', (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`${command} ${args.join(' ')} failed (${code ?? signal})`));
    });
  });
}
