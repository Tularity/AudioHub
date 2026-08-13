#!/usr/bin/env node
// Fail the build if `tsconfig.test.json` has stopped actually typechecking the
// tests.
//
// # The failure this exists to catch
//
// `tsconfig.test.json` extends `tsconfig.app.json`, and `exclude` is INHERITED
// through `extends` unless the child restates it. The app project excludes
// `src/**/*.test.ts` (so that application code keeps `"types": ["vite/client"]`
// and cannot reach for `process`/`fs`). A child that only sets `include` gets
// the parent's `exclude` applied on top of it -- and that exclude cancels every
// path the child just asked for.
//
// The result is not an error. It is a project containing exactly one file
// (`vitest.config.ts`), which typechecks clean, in a fraction of a second, with
// exit code 0. `tsc -b` stays green, `npm run build` stays green, and the seven
// test files are simply never looked at. It stayed that way through a whole
// commit that advertised the opposite.
//
// A comment saying "tests must typecheck too" cannot detect this. This can:
// ask the compiler which files it actually loaded, and compare that against the
// test files on disk.

import { execFileSync } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const frontendDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const project = process.argv[2] ?? 'tsconfig.test.json';
const srcDir = join(frontendDir, 'src');
const tscEntry = join(frontendDir, 'node_modules', 'typescript', 'bin', 'tsc');

/** Every `*.test.ts` actually sitting on disk under `src/`. */
function testFilesOnDisk(dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules') continue;
      out.push(...testFilesOnDisk(full));
    } else if (/\.test\.tsx?$/.test(entry.name)) {
      out.push(full);
    }
  }
  return out;
}

// `--listFiles` prints the loaded files even when the project has type errors,
// and type errors make tsc exit non-zero. We care only about the file list here
// -- `npm run typecheck` is what judges the errors -- so capture stdout either
// way and let a genuinely broken invocation fall out as "no files listed".
function listedFiles() {
  try {
    return execFileSync(
      process.execPath,
      [tscEntry, '-p', project, '--listFiles', '--noEmit'],
      { cwd: frontendDir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] },
    );
  } catch (err) {
    if (typeof err.stdout === 'string' && err.stdout.length > 0) return err.stdout;
    throw err;
  }
}

const listed = new Set(
  listedFiles()
    .split('\n')
    .map((line) => line.trim())
    // Diagnostics share this stream; real entries are absolute paths.
    .filter((line) => isAbsolute(line) && /\.tsx?$/.test(line))
    .map((line) => resolve(line)),
);

const onDisk = testFilesOnDisk(srcDir);
const missing = onDisk.filter((f) => !listed.has(f));

if (onDisk.length === 0) {
  console.error(`check-test-project: no *.test.ts found under ${srcDir}.`);
  console.error('Either the tests were deleted or this script is looking in the wrong place.');
  process.exit(1);
}

if (missing.length > 0) {
  console.error(`check-test-project: ${project} is NOT typechecking these test files:\n`);
  for (const f of missing) console.error(`  ${f}`);
  console.error(`\n${missing.length} of ${onDisk.length} test file(s) invisible to the compiler.`);
  console.error('\nMost likely cause: `exclude` is inherited through `extends`, and the parent');
  console.error("project excludes `src/**/*.test.ts`. Restate `\"exclude\": []` in the child.");
  console.error('Until this passes, the tests are NOT type-safe no matter what tsc exits with.');
  process.exit(1);
}

console.log(`check-test-project: ${project} typechecks all ${onDisk.length} test file(s).`);
