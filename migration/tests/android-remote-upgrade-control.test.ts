import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { startUpgradeControl } from '../src/android-remote/upgrade-control';

test('private source control exports once, retains completion, and refuses browser and wrong-instance requests', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-control-'));
  let finish!: () => void, calls = 0;
  const wait = new Promise<void>(r => { finish = r; });
  let entered!: () => void;
  const exporting = new Promise<void>(r => { entered = r; });
  const control = await startUpgradeControl({ sourceRoot: root, installation: join(root, 'installation'),
    async exportOwner(directory) {
      calls++; entered(); await wait;
      await mkdir(directory);
      const receipt = join(directory, 'closed-owner.json');
      await writeFile(receipt, '{}', { mode: 0o600 });
      return { version: 1, closed_owner: receipt, configuration: join(root, 'config.json'), bind: '127.0.0.1:1234', public_url: 'http://127.0.0.1:1234', gui: root };
    },
  });
  try {
    const descriptor = JSON.parse(await readFile(control.descriptor, 'utf8'));
    const headers = { authorization: `Bearer ${descriptor.token}`, 'x-remodex-upgrade-instance': descriptor.instance };
    expect((await fetch(descriptor.endpoint)).status).toBe(403);
    for (const extra of [{ origin: 'http://localhost' }, { 'sec-fetch-site': 'same-origin' }, { 'x-remodex-upgrade-instance': 'foreign' }]) {
      expect((await fetch(descriptor.endpoint, { method: 'POST', headers: { ...headers, ...extra } })).status).toBe(403);
    }
    expect((await fetch(descriptor.endpoint, { method: 'POST', headers, body: 'arbitrary destination' })).status).toBe(400);
    expect((await fetch(descriptor.endpoint, { headers }).then(r => r.json())).phase).toBe('idle');
    const pending = await Promise.all(Array.from({ length: 5 }, () => fetch(descriptor.endpoint, { method: 'POST', headers }).then(r => r.json())));
    expect(pending.every(r => r.phase === 'exporting')).toBe(true);
    await exporting;
    expect(calls).toBe(1);
    finish();
    await control.close();
    const plan = JSON.parse(await readFile(join(dirname(control.descriptor), 'npm-plan.json'), 'utf8'));
    expect(plan.closed_owner).toBe(join(dirname(control.descriptor), 'export', 'closed-owner.json'));
    expect(calls).toBe(1);
    if (process.platform !== 'win32') expect((await stat(control.descriptor)).mode & 0o077).toBe(0);
  } finally { finish(); await control.close(); await rm(root, { recursive: true, force: true }); }
});

test('failed source export is not replayed and never publishes a plan or sensitive error', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-failure-'));
  let calls = 0;
  const control = await startUpgradeControl({ sourceRoot: root, installation: join(root, 'installation'),
    async exportOwner() { calls++; throw new Error('private owner internals'); },
  });
  try {
    const descriptor = JSON.parse(await readFile(control.descriptor, 'utf8'));
    const headers = { authorization: `Bearer ${descriptor.token}`, 'x-remodex-upgrade-instance': descriptor.instance };
    await fetch(descriptor.endpoint, { method: 'POST', headers });
    let result: any;
    for (let i = 0; i < 30; i++) {
      result = await fetch(descriptor.endpoint, { headers }).then(r => r.json());
      if (result.phase === 'failed') break;
      await Bun.sleep(10);
    }
    expect(result.phase).toBe('failed');
    expect(JSON.stringify(result)).not.toContain('private');
    expect((await fetch(descriptor.endpoint, { method: 'POST', headers }).then(r => r.json())).phase).toBe('failed');
    expect(calls).toBe(1);
    expect(await stat(join(dirname(control.descriptor), 'npm-plan.json')).then(() => true, () => false)).toBe(false);
    expect(await stat(join(dirname(control.descriptor), 'attempt.json')).then(() => true, () => false)).toBe(true);
  } finally { await control.close(); await rm(root, { recursive: true, force: true }); }
});
