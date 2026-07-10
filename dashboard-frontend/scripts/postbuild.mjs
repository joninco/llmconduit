import { promises as fs } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { pathToFileURL } from 'node:url';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';

const COMPRESSIBLE_EXTENSIONS = new Set(['.js', '.css', '.json', '.svg']);
const MAX_JS_CHUNK_BYTES = 500 * 1024;
const MAX_INITIAL_JS_GZIP_BYTES = 150 * 1024;

/**
 * Validate the production bundle and write deterministic `.br`/`.gz` siblings for every textual
 * static asset the Rust host negotiates. The function is exported so build tooling/tests can point
 * it at an arbitrary Vite outDir; invoking this file directly defaults to `dist/`.
 */
export async function finalizeBuild(distDir) {
  const indexPath = path.join(distDir, 'index.html');
  const indexHtml = await fs.readFile(indexPath, 'utf8');
  const files = await walk(distDir);

  assertWoff2Only(files);
  await assertBundleBudgets(distDir, indexHtml, files);

  const compressible = files.filter((file) => COMPRESSIBLE_EXTENSIONS.has(path.extname(file)));
  await Promise.all(
    compressible.map(async (file) => {
      const source = await fs.readFile(file);
      const brotli = brotliCompressSync(source, {
        params: {
          [constants.BROTLI_PARAM_MODE]: constants.BROTLI_MODE_TEXT,
          [constants.BROTLI_PARAM_QUALITY]: 11,
        },
      });
      const gzip = gzipSync(source, { level: 9, mtime: 0 });
      await Promise.all([fs.writeFile(`${file}.br`, brotli), fs.writeFile(`${file}.gz`, gzip)]);
    }),
  );

  process.stdout.write(
    `dashboard assets: ${compressible.length} files precompressed; bundle budgets passed\n`,
  );
}

async function assertBundleBudgets(distDir, indexHtml, files) {
  const jsFiles = files.filter((file) => path.extname(file) === '.js');
  for (const file of jsFiles) {
    const { size } = await fs.stat(file);
    if (size > MAX_JS_CHUNK_BYTES) {
      throw new Error(
        `JavaScript chunk ${path.relative(distDir, file)} is ${size} bytes; limit is ${MAX_JS_CHUNK_BYTES} bytes (500 KiB)`,
      );
    }
  }

  const initialFiles = initialJavaScriptFiles(distDir, indexHtml);
  let initialGzipBytes = 0;
  for (const file of initialFiles) {
    const source = await fs.readFile(file);
    initialGzipBytes += gzipSync(source, { level: 9, mtime: 0 }).byteLength;
  }
  if (initialGzipBytes > MAX_INITIAL_JS_GZIP_BYTES) {
    throw new Error(
      `Initial JavaScript is ${initialGzipBytes} gzip bytes; limit is ${MAX_INITIAL_JS_GZIP_BYTES} bytes (150 KiB)`,
    );
  }
}

function initialJavaScriptFiles(distDir, html) {
  const urls = new Set();
  for (const match of html.matchAll(/<(script|link)\b[^>]*>/gi)) {
    const tag = match[0];
    const kind = match[1]?.toLowerCase();
    if (kind === 'link') {
      const rel = attribute(tag, 'rel')?.toLowerCase().split(/\s+/) ?? [];
      if (!rel.includes('modulepreload')) continue;
    }
    const url = attribute(tag, kind === 'script' ? 'src' : 'href');
    if (!url || !/\.js(?:[?#]|$)/i.test(url)) continue;
    urls.add(url);
  }

  if (urls.size === 0) throw new Error('Vite index.html references no initial JavaScript');
  return [...urls].map((url) => localAssetPath(distDir, url));
}

function attribute(tag, name) {
  const match = tag.match(new RegExp(`\\b${name}=["']([^"']+)["']`, 'i'));
  return match?.[1] ?? null;
}

function localAssetPath(distDir, url) {
  let pathname;
  try {
    pathname = decodeURIComponent(new URL(url, 'https://llmconduit.invalid').pathname);
  } catch {
    throw new Error(`Invalid asset URL in Vite index.html: ${url}`);
  }
  const dashboardPrefix = '/dashboard/';
  const relative = pathname.startsWith(dashboardPrefix)
    ? pathname.slice(dashboardPrefix.length)
    : pathname.replace(/^\/+/, '');
  const resolved = path.resolve(distDir, relative);
  const root = `${path.resolve(distDir)}${path.sep}`;
  if (!resolved.startsWith(root)) throw new Error(`Initial asset escapes dist/: ${url}`);
  return resolved;
}

function assertWoff2Only(files) {
  const legacy = files.filter((file) => path.extname(file) === '.woff');
  if (legacy.length > 0) {
    throw new Error(`Legacy WOFF assets emitted despite the WOFF2-only transform: ${legacy.join(', ')}`);
  }
}

async function walk(directory) {
  const out = [];
  for (const entry of await fs.readdir(directory, { withFileTypes: true })) {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(file)));
    else if (entry.isFile() && !file.endsWith('.br') && !file.endsWith('.gz')) out.push(file);
  }
  return out;
}

const invokedPath = process.argv[1] ? pathToFileURL(path.resolve(process.argv[1])).href : '';
if (import.meta.url === invokedPath) {
  const distDir = path.resolve(process.cwd(), process.argv[2] ?? 'dist');
  finalizeBuild(distDir).catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
