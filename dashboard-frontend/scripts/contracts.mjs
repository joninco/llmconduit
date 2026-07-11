#!/usr/bin/env node

import Ajv2020 from 'ajv/dist/2020.js';
import standaloneCode from 'ajv/dist/standalone/index.js';
import { compile } from 'json-schema-to-typescript';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const frontend = resolve(here, '..');
const repo = resolve(frontend, '..');
const checkedIn = join(frontend, 'src', 'api', 'generated');
const command = process.argv[2];

if (command !== 'generate' && command !== 'check') {
  console.error('usage: node scripts/contracts.mjs <generate|check>');
  process.exit(2);
}

const scratch = await mkdtemp(join(tmpdir(), 'llmconduit-contracts-'));
try {
  const generated = join(scratch, 'generated');
  await generate(generated);
  if (command === 'generate') {
    await rm(checkedIn, { recursive: true, force: true });
    await mkdir(dirname(checkedIn), { recursive: true });
    await copyTree(generated, checkedIn);
  } else {
    const drift = await compareTrees(generated, checkedIn);
    if (drift.length > 0) {
      console.error('dashboard contracts are stale; run `npm run contracts:generate`:');
      for (const item of drift) console.error(`  ${item}`);
      process.exitCode = 1;
    }
  }
} finally {
  await rm(scratch, { recursive: true, force: true });
}

async function generate(output) {
  const schemas = join(output, 'schemas');
  await mkdir(schemas, { recursive: true });
  execFileSync('cargo', ['run', '--quiet', '--bin', 'dashboard-contracts', '--', schemas], {
    cwd: repo,
    stdio: 'inherit',
  });

  const declarationsSchema = JSON.parse(await readFile(join(schemas, 'contracts.schema.json'), 'utf8'));
  const declarations = await compile(declarationsSchema, 'DashboardContracts', {
    additionalProperties: false,
    bannerComment:
      '/* eslint-disable */\n/** Generated from Rust dashboard DTOs. Do not edit by hand. */',
    unknownAny: true,
  });
  await writeFile(join(output, 'contracts.d.ts'), declarations);

  const roots = [
    { file: 'bootstrap', exported: 'validateBootstrap', type: 'DashboardBootstrap', definition: 'DashboardBootstrap', group: 'initial' },
    { file: 'catalog', exported: 'validateCatalog', type: 'CatalogEntry[]', definition: null, group: 'rest' },
    { file: 'flow-detail', exported: 'validateFlowDetail', type: 'FlowDetailBody', definition: 'FlowDetailBody', group: 'rest' },
    { file: 'flows', exported: 'validateFlows', type: 'FlowsResponse', definition: 'FlowsResponse', group: 'rest' },
    { file: 'history', exported: 'validateHistory', type: 'HistoryResponse', definition: 'HistoryResponse', group: 'rest' },
    { file: 'kill', exported: 'validateKill', type: 'KillResponse', definition: 'KillResponse', group: 'rest' },
    { file: 'metrics', exported: 'validateMetrics', type: 'MetricsSnapshot', definition: 'MetricsSnapshot', group: 'rest' },
    { file: 'overview', exported: 'validateOverview', type: 'OverviewResponse', definition: 'OverviewResponse', group: 'rest' },
    { file: 'snapshot', exported: 'validateSnapshot', type: 'SnapshotResponse', definition: 'SnapshotResponse', group: 'rest' },
    { file: 'topology', exported: 'validateTopology', type: 'TopologySnapshot', definition: 'TopologySnapshot', group: 'rest' },
    { file: 'ws-frame', exported: 'validateWsFrame', type: 'DashboardFrame', definition: 'DashboardFrame', group: 'initial' },
    { file: 'ws-snapshot', exported: 'validateWsSnapshot', type: 'SnapshotMessage', definition: 'SnapshotMessage', group: 'initial' },
  ];
  const commonId = 'urn:llmconduit:dashboard:contracts:v3';
  declarationsSchema.$id = commonId;
  for (const group of ['initial', 'rest']) {
    const selected = roots.filter((root) => root.group === group);
    const ajv = new Ajv2020({
      allErrors: true,
      strict: false,
      strictNumbers: true,
      validateFormats: false,
      code: { source: true, esm: true, optimize: 2 },
    });
    ajv.addSchema(declarationsSchema, commonId);
    const exports = {};
    for (const root of selected) {
      const id = `urn:llmconduit:dashboard:${root.file}:v3`;
      const target = root.definition
        ? { $ref: `${commonId}#/$defs/${root.definition}` }
        : { type: 'array', items: { $ref: `${commonId}#/$defs/CatalogEntry` } };
      ajv.addSchema({ $id: id, ...target }, id);
      exports[root.exported] = id;
    }
    const base = `validators-${group}`;
    await writeFile(
      join(output, `${base}.js`),
      `/* eslint-disable */\n/** Generated CSP-safe ${group} validators. Do not edit by hand. */\n${standaloneCode(ajv, exports)}\n`,
    );
    const importedTypes = [...new Set(selected.flatMap((root) =>
      root.type === 'CatalogEntry[]' ? ['CatalogEntry'] : [root.type],
    ))].join(', ');
    const validatorDecls = [
      '/* eslint-disable */',
      `/** Generated CSP-safe standalone ${group} validator declarations. */`,
      `import type { ${importedTypes} } from './contracts';`,
      "import type { ContractValidator } from '../contractValidator';",
      ...selected.map((root) => `export const ${root.exported}: ContractValidator<${root.type}>;`),
      '',
    ].join('\n');
    await writeFile(join(output, `${base}.d.ts`), validatorDecls);
  }
}

async function copyTree(from, to) {
  await mkdir(to, { recursive: true });
  for (const entry of await readdir(from, { withFileTypes: true })) {
    const source = join(from, entry.name);
    const destination = join(to, entry.name);
    if (entry.isDirectory()) await copyTree(source, destination);
    else await writeFile(destination, await readFile(source));
  }
}

async function compareTrees(actual, expected, prefix = '') {
  const readNames = async (dir) => {
    try {
      return await readdir(dir, { withFileTypes: true });
    } catch {
      return [];
    }
  };
  const actualEntries = await readNames(actual);
  const expectedEntries = await readNames(expected);
  const names = [...new Set([...actualEntries, ...expectedEntries].map((entry) => entry.name))].sort();
  const drift = [];
  for (const name of names) {
    const a = actualEntries.find((entry) => entry.name === name);
    const e = expectedEntries.find((entry) => entry.name === name);
    const label = prefix ? `${prefix}/${name}` : name;
    if (!a) drift.push(`unexpected checked-in file: ${label}`);
    else if (!e) drift.push(`missing checked-in file: ${label}`);
    else if (a.isDirectory() !== e.isDirectory()) drift.push(`type changed: ${label}`);
    else if (a.isDirectory()) {
      drift.push(...(await compareTrees(join(actual, name), join(expected, name), label)));
    } else {
      const [aBytes, eBytes] = await Promise.all([
        readFile(join(actual, name)),
        readFile(join(expected, name)),
      ]);
      if (!aBytes.equals(eBytes)) drift.push(`content changed: ${label}`);
    }
  }
  return drift;
}
