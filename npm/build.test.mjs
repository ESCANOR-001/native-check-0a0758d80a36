import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, mkdir, readFile, rm, writeFile, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { buildPackages, buildPublishedPackage } from './build.mjs';

test('a version-changing package binds the launcher, native manifest and optional dependencies', async () => {
  const root = await mkdtemp(join(tmpdir(), 'native-package-version-'));
  try {
    const binary = join(root, 'binary');
    await writeFile(binary, 'Synthetic checksum fixture; never executed');
    const source = join(root, 'source');
    for (const directory of ['bin', 'src', 'gui/dist']) await mkdir(join(source, directory), { recursive: true });
    await writeFile(join(source, 'package.json'), JSON.stringify({ name: '@remodex/rmx', version: '1.2.27', bin: { rmx: 'bin/ocx.mjs' }, dependencies: {} }));
    for (const version of ['0.0.0', '0.0.1']) {
      const built = await buildPackages(binary, join(root, `packages-${version}`), version);
      const published = await buildPublishedPackage(source, built);
      const launcher = await import(pathToFileURL(join(published, 'bin/native/launcher.mjs')).href);
      assert.equal(launcher.VERSION, version);
      assert(await launcher.verifyBinary(built.native, built.target));
      for (const directory of [built.native, built.launcher, published]) {
        const pkg = JSON.parse(await readFile(join(directory, 'package.json'), 'utf8'));
        assert.equal(pkg.version, version);
        assert.equal(pkg.private, true);
        for (const selected of Object.values(pkg.optionalDependencies ?? {})) assert.equal(selected, version);
      }
      assert.equal(JSON.parse(await readFile(join(published, 'bin/native-runtime.json'), 'utf8')).version, version);
    }
    const old = await import(pathToFileURL(join(root, 'packages-0.0.0/launcher/bin/launcher.mjs')).href);
    const next = join(root, 'packages-0.0.1/native');
    const target = JSON.parse(await readFile(join(next, 'native.json'), 'utf8')).target;
    await assert.rejects(old.verifyBinary(next, target), /does not match/);
    for (const version of ['01.2.3', '1.2', '../escape', "1.2.3';throw 1;//"]) {
      const destination = join(root, 'invalid');
      await assert.rejects(buildPackages(binary, destination, version), /canonical stable package version/);
      await assert.rejects(access(destination));
    }
  } finally { await rm(root, { recursive: true, force: true }); }
});
