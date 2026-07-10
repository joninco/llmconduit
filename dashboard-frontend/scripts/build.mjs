import { spawnSync } from 'node:child_process';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';
import { finalizeBuild } from './postbuild.mjs';

const projectDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const viteCli = path.join(projectDir, 'node_modules', 'vite', 'bin', 'vite.js');
const viteArgs = process.argv.slice(2);

const result = spawnSync(process.execPath, [viteCli, 'build', ...viteArgs], {
  cwd: projectDir,
  env: process.env,
  stdio: 'inherit',
});

if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);

await finalizeBuild(resolveOutDir(viteArgs));

function resolveOutDir(args) {
  let outDir = 'dist';
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === '--outDir' && args[i + 1]) {
      outDir = args[i + 1];
      i += 1;
    } else if (arg?.startsWith('--outDir=')) {
      outDir = arg.slice('--outDir='.length);
    }
  }
  return path.resolve(projectDir, outDir);
}
