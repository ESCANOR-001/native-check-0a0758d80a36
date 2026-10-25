import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, readFile, realpath, rm, stat, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { startUpgradeControl } from '../src/android-remote/upgrade-control';
import { startUpgradeSource } from '../src/android-remote/upgrade-startup';

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
    expect(JSON.parse(await readFile(join(root, 'installation/npm-source.json'), 'utf8')))
      .toEqual({ version: 1, descriptor: control.descriptor });
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
    expect((await stat(join(root, 'installation/npm-source.json'))).isFile()).toBe(true);
    finish();
    await control.close();
    const plan = JSON.parse(await readFile(join(dirname(control.descriptor), 'npm-plan.json'), 'utf8'));
    expect(plan.closed_owner).toBe(join(dirname(control.descriptor), 'export', 'closed-owner.json'));
    expect(calls).toBe(1);
    if (process.platform !== 'win32') expect((await stat(control.descriptor)).mode & 0o077).toBe(0);
  } finally { finish(); await control.close(); await rm(root, { recursive: true, force: true }); }
});

test('idle shutdown releases discovery but competing source startup never replaces a registered owner', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-registration-'));
  const options = { sourceRoot: root, installation: join(root, 'installation'),
    async exportOwner(): Promise<never> { throw new Error('No export requested'); } };
  const first = await startUpgradeControl(options);
  try {
    const path = join(options.installation, 'npm-source.json');
    const registered = await readFile(path, 'utf8');
    await expect(startUpgradeControl(options)).rejects.toThrow();
    expect(await readFile(path, 'utf8')).toBe(registered);
    await first.close();
    expect(await stat(path).then(() => true, () => false)).toBe(false);
    const next = await startUpgradeControl(options);
    expect(next.descriptor).not.toBe(first.descriptor);
    await next.close();
  } finally { await first.close(); await rm(root, { recursive: true, force: true }); }
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

test('source registration canonicalizes parent aliases but rejects a linked installation directory', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-path-alias-'));
  const real = join(root, 'real'), alias = join(root, 'alias');
  await mkdir(real);
  await symlink(real, alias, process.platform === 'win32' ? 'junction' : 'dir');
  const options = { sourceRoot: real, installation: join(alias, 'installation'),
    async exportOwner(): Promise<never> { throw new Error('No export requested'); } };
  try {
    const control = await startUpgradeControl(options);
    try {
      const descriptor = JSON.parse(await readFile(control.descriptor, 'utf8'));
      expect(descriptor.installation).toBe(await realpath(options.installation));
    } finally { await control.close(); }
    const linked = join(root, 'linked-installation');
    await symlink(options.installation, linked, process.platform === 'win32' ? 'junction' : 'dir');
    await expect(startUpgradeControl({ ...options, installation: linked })).rejects.toThrow();
  } finally { await rm(root, { recursive: true, force: true }); }
});


test('published source startup binds its configuration before shutdown and exits only after a durable plan', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-startup-'));
  const events: string[] = [];
  const profiles = { codex: join(root, 'codex'), claude: join(root, 'claude') };
  let published!: () => void;
  const done = new Promise<void>(resolve => { published = resolve; });
  const control = await startUpgradeSource({ sourceRoot: root,
    async runtime() { events.push('resolve'); return { configuration: { profiles, max_active_chats: 4 }, profiles,
      bind: '127.0.0.1:1234', publicUrl: 'http://127.0.0.1:1234', gui: root }; },
    async export(directory, actual) {
      events.push('shutdown'); expect(actual).toEqual(profiles);
      expect(JSON.parse(await readFile(join(dirname(directory), 'configuration.json'), 'utf8')).profiles).toEqual(profiles);
      await mkdir(directory); const receipt = join(directory, 'closed-owner.json');
      await writeFile(receipt, '{}'); return { receipt };
    },
    published() { events.push('published'); published(); },
  });
  try {
    expect(events).toEqual([]);
    const d=JSON.parse(await readFile(control.descriptor, 'utf8'));
    const headers = { authorization: `Bearer ${d.token}`, 'x-remodex-upgrade-instance':d.instance };
    expect((await fetch(d.endpoint, { method: 'POST', headers })).status).toBe(409);
    const preflight = await fetch(d.endpoint.replace('/upgrade', '/configuration'), { headers }).then(r=>r.json());
    await fetch(d.endpoint, { method: 'POST', headers: { ...headers, 'x-remodex-configuration-sha256': preflight.configurationHash } });
    await Promise.race([done, Bun.sleep(3000).then(() => { throw new Error('Handoff not published'); })]);
    expect(events).toEqual(['resolve','resolve','shutdown','published']);
    const plan=JSON.parse(await readFile(join(dirname(control.descriptor), 'npm-plan.json'), 'utf8'));
    expect(plan.configuration).toBe(join(dirname(control.descriptor), 'configuration.json'));
    expect(plan.bind).toBe('127.0.0.1:1234');
    expect(await fetch(d.endpoint).then(() => true, () => false)).toBe(false);
    expect(JSON.parse(await readFile(join(root,'native-installation/npm-source.json'),'utf8')).descriptor).toBe(control.descriptor);
  } finally { await control.close(); await rm(root, { recursive: true, force: true }); }
});

test('unsupported source configuration keeps its owner running and publishes no completion', async () => {
  const root=await mkdtemp(join(tmpdir(),'upgrade-unsupported-'));
  let exported=false,published=false;
  const control=await startUpgradeSource({sourceRoot:root,
    async runtime():Promise<never> { throw new Error('Unsupported tunnel'); },
    async export():Promise<never> { exported=true; throw new Error('Must not retire source'); },
    published() { published=true; },
  });
  try {
    const d=JSON.parse(await readFile(control.descriptor,'utf8'));
    const headers={authorization:`Bearer ${d.token}`,'x-remodex-upgrade-instance':d.instance};
    expect((await fetch(d.endpoint,{method:'POST',headers})).status).toBe(409);
    expect((await fetch(d.endpoint,{headers}).then(r=>r.json())).phase).toBe('idle');
    expect(exported).toBe(false); expect(published).toBe(false);
    expect(await stat(join(dirname(control.descriptor),'npm-plan.json')).then(()=>true,()=>false)).toBe(false);
  } finally { await control.close(); await rm(root,{recursive:true,force:true}); }
});

test('a transient configuration probe can recover without restarting its source owner', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-retry-'));
  let probes = 0, exports = 0;
  const profiles = { codex: join(root, 'codex'), claude: join(root, 'claude') };
  const control = await startUpgradeSource({ sourceRoot: root,
    async runtime() {
      if (++probes === 1) throw new Error('Temporary capability discovery failure');
      return { configuration: { profiles }, profiles, bind: '127.0.0.1:1234', publicUrl: 'http://127.0.0.1:1234', gui: root };
    },
    async export(): Promise<never> { exports++; throw new Error('No export requested'); },
  });
  try {
    const d = JSON.parse(await readFile(control.descriptor, 'utf8'));
    const headers = { authorization: `Bearer ${d.token}`, 'x-remodex-upgrade-instance': d.instance };
    const endpoint = d.endpoint.replace('/upgrade', '/configuration');
    expect((await fetch(endpoint, { headers })).status).toBe(409);
    const response = await fetch(endpoint, { headers });
    expect(response.status).toBe(200);
    expect((await response.json()).configurationHash).toMatch(/^[a-f0-9]{64}$/);
    expect(probes).toBe(2);
    expect(exports).toBe(0);
    expect((await fetch(d.endpoint, { headers }).then(r => r.json())).phase).toBe('idle');
    expect(await stat(join(dirname(control.descriptor), 'attempt.json')).then(() => true, () => false)).toBe(false);
  } finally { await control.close(); await rm(root, { recursive: true, force: true }); }
});
