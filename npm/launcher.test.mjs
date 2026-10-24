import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { platformKey, verifyBinary, nativeArguments } from './launcher.mjs';

test('Upgrade runs the native coordinator only on explicit launch and keeps paths literal', () => {
  assert.deepEqual(nativeArguments(['--version']), ['--version']);
  assert.deepEqual(nativeArguments(['upgrade', 'C:\\Users\\A B\\install', 'C:\\$(literal)\\plan.json']),
    ['--npm-upgrade', 'run', 'C:\\Users\\A B\\install', 'C:\\$(literal)\\plan.json']);
  assert.throws(() => nativeArguments(['upgrade']), /Usage/);
  assert.throws(() => nativeArguments(['upgrade', 'a', 'b', 'extra']), /Usage/);
  assert.deepEqual(nativeArguments(['start','C:\\Users\\A B\\installation']), ['--npm-upgrade','resume','C:\\Users\\A B\\installation']);
  assert.throws(() => nativeArguments(['start']), /Usage/);
  assert.deepEqual(nativeArguments(['start','a','b']), ['--npm-upgrade','automatic','a','b']);
  assert.deepEqual(nativeArguments(['migrate','a','b']), ['--npm-upgrade','automatic','a','b']);
  assert.throws(() => nativeArguments(['migrate','a']), /Usage/);
  assert.throws(() => nativeArguments(['start','a','b','c']), /Usage/);
});

test('Only qualified platforms are selected; non-glibc and unknown architectures fail clearly', () => {
  assert.equal(platformKey('linux', 'x64', '2.39'), 'linux-x64-gnu');
  assert.equal(platformKey('darwin', 'arm64', ''), 'darwin-arm64');
  assert.equal(platformKey('win32', 'x64', ''), 'win32-x64');
  for (const args of [['linux', 'x64', ''], ['linux', 'arm64', '2.39'], ['darwin', 'x64', ''], ['win32', 'arm64', '']]) assert.throws(() => platformKey(...args), /no qualified binary/);
});
test('Checksums, platform, metadata bounds and libc floor are checked before execution', async () => {
  const root = await mkdtemp(join(tmpdir(), 'native-package-check-'));
  try {
    const body = Buffer.from('Synthetic executable bytes; never executed');
    const executable = join(root, 'remodex-connect-native');
    await writeFile(executable, body);
    const manifest = { formatVersion: 1, version: '0.0.0', migrationComplete: false, target: 'linux-x64-gnu', minimumGlibc: '2.31', bytes: body.length, sha256: createHash('sha256').update(body).digest('hex') };
    const save = m => writeFile(join(root, 'native.json'), JSON.stringify(m));
    await save(manifest);
    assert.equal(await verifyBinary(root, 'linux-x64-gnu', '2.39'), executable);
    await assert.rejects(verifyBinary(root, 'linux-x64-gnu', '2.30'), /newer glibc/);
    await assert.rejects(verifyBinary(root, 'win32-x64', ''), /does not match/);
    await save({ ...manifest, version: '1.0.0' });
    await assert.rejects(verifyBinary(root, 'linux-x64-gnu', '2.39'), /does not match/);
    await save({ ...manifest, sha256: '0'.repeat(64) });
    await assert.rejects(verifyBinary(root, 'linux-x64-gnu', '2.39'), /checksum failed/);
    await writeFile(join(root, 'native.json'), 'x'.repeat(4097));
    await assert.rejects(verifyBinary(root, 'linux-x64-gnu', '2.39'), /metadata is invalid/);
    await save(manifest); await writeFile(executable, Buffer.concat([body, Buffer.from('extra')]));
    await assert.rejects(verifyBinary(root, 'linux-x64-gnu', '2.39'), /executable is invalid/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
