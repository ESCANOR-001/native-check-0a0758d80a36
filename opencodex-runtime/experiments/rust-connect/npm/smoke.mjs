// Offline, disposable npm installation. Never installs globally or touches rmx.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { access, mkdtemp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { buildPackages } from './build.mjs';

if (process.argv.length !== 4) throw new Error('Usage: node npm/smoke.mjs RELEASE_BINARY FIXTURE_BINARY');
const binary = resolve(process.argv[2]), fixture = resolve(process.argv[3]);
await access(binary); await access(fixture);
const root = await mkdtemp(join(tmpdir(), 'native npm check '));
const children = new Set();
const exists = async p => access(p).then(() => true, () => false);
let npmCli;
for (const candidate of [process.env.npm_execpath, join(dirname(process.execPath), 'node_modules/npm/bin/npm-cli.js'), join(dirname(process.execPath), '../lib/node_modules/npm/bin/npm-cli.js')].filter(Boolean)) {
  if (await exists(candidate)) { npmCli = candidate; break; }
}
if (!npmCli) throw new Error('npm CLI is unavailable beside Node; run this test with a standard Node/npm distribution.');
const config = join(root, 'empty-npmrc'); await writeFile(config, '');
const globalConfig = join(root, 'empty-global-npmrc'); await writeFile(globalConfig, '');
const prefix = join(root, 'install'); await mkdir(prefix);
const env = { ...process.env, npm_config_userconfig: config, npm_config_globalconfig: globalConfig, npm_config_cache: join(root, 'cache'), npm_config_offline: 'true', npm_config_audit: 'false', npm_config_fund: 'false', npm_config_update_notifier: 'false' };
const run = (exe, args, cwd = root) => {
  const p = spawnSync(exe, args, { cwd, env, windowsHide: true, shell: false, encoding: 'utf8', timeout: 120_000, maxBuffer: 2 * 1024 * 1024 });
  if (p.status !== 0 || p.error) throw new Error(`Isolated command failed (${p.status}): ${p.stderr || p.error?.message || ''}`);
  return p.stdout;
};
const npm = (args, cwd) => run(process.execPath, [npmCli, ...args, '--ignore-scripts', '--offline', '--no-audit', '--no-fund'], cwd);
const exited = (child, ms = 10_000) => new Promise((resolve, reject) => {
  if (child.exitCode !== null) return resolve(child.exitCode);
  const timer = setTimeout(() => reject(new Error('Candidate did not exit')), ms);
  child.once('exit', code => { clearTimeout(timer); resolve(code); });
});
try {
  const built = await buildPackages(binary, join(root, 'packages'));
  const pack = candidate => [candidate.native, candidate.launcher].map(dir => join(dir, JSON.parse(npm(['pack', '--json'], dir))[0].filename));
  let tarballs = pack(built);
  const mainName = '@remodex/native-connect-experiment';
  const install = () => npm(['install', '--prefix', prefix, '--package-lock=false', ...tarballs]);
  install();
  const launcher = join(prefix, 'node_modules', mainName, 'bin/launcher.mjs');
  const bin = join(prefix, 'node_modules/.bin', process.platform === 'win32' ? 'rmx-native-experiment.cmd' : 'rmx-native-experiment');
  assert(await exists(bin));
  const version = JSON.parse(run(process.execPath, [launcher, '--version']));
  assert.equal(version.runtime, 'rust'); assert.equal(version.migrationComplete, false);
  assert.match(run(process.execPath, [launcher, '--help']), /ABSOLUTE_ISOLATED_STATE_DIR/);
  const state = join(root, 'persisted state'), workspace = join(root, 'workspace');
  const gui = join(root, 'gui'), agentProfile = join(root, 'agent profile');
  for (const p of [state, workspace, gui, agentProfile]) await mkdir(p);
  await writeFile(join(state, 'sentinel'), 'Keep state across package replacement and removal');
  await writeFile(join(gui, 'index.html'), '<html><head></head><body>Synthetic packaging fixture</body></html>');
  const chatConfig = join(root, 'agent.json');
  const agent = { executable: fixture, profile: agentProfile, model: 'test' };
  await writeFile(chatConfig, JSON.stringify({ workspace, codex: agent, claude: agent }));
  const checked = JSON.parse(run(process.execPath, [launcher, '--check-config', chatConfig]));
  assert.equal(checked.configurationValid, true);
  assert.equal(checked.runtimeStarted, false);
  assert.equal(checked.configurationVersion, 1);
  assert.deepEqual(await readdir(agentProfile), []);
  assert.deepEqual(await readdir(state), ['sentinel']);
  let identity, phone;
  for (let attempt = 0; attempt < 2; attempt++) {
    // Exercise both the replacement path and the older-Node forwarding path.
    const preload = attempt === 1 ? ['--import=data:text/javascript,process.execve=undefined'] : [];
    const child = spawn(process.execPath, [...preload, launcher, state, '127.0.0.1:0', 'http://127.0.0.1', chatConfig, gui], { env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true, shell: false });
    children.add(child); child.stderr.resume();
    const line = await new Promise((resolve, reject) => {
      const reader = createInterface({ input: child.stdout });
      const timer = setTimeout(() => { reader.close(); reject(new Error('Installed package did not become ready')); }, 15_000);
      reader.once('line', line => { clearTimeout(timer); reader.close(); resolve(line); });
      child.once('error', e => { clearTimeout(timer); reader.close(); reject(e); });
      child.once('exit', () => { clearTimeout(timer); reader.close(); reject(new Error('Installed candidate exited before readiness')); });
    });
    const ready = JSON.parse(line); assert.equal(ready.ready, true); assert.equal(ready.migrationComplete, false);
    const base = `http://${ready.address}`;
    const req = async (path, init) => {
      const r = await fetch(base + path, { ...init, signal: AbortSignal.timeout(5_000) });
      if (!r.ok) throw new Error(`Candidate endpoint failed: ${r.status}`);
      return r;
    };
    const descriptor = await (await req('/.well-known/t3/environment')).json();
    assert.equal(typeof descriptor.environmentId, 'string');
    if (identity) assert.equal(descriptor.environmentId, identity);
    identity = descriptor.environmentId;
    const invite = JSON.parse(await readFile(ready.invitationFile, 'utf8'));
    if (!phone) {
      const auth = await (await req('/oauth/token', { method: 'POST', body: new URLSearchParams({ grant_type: 'urn:ietf:params:oauth:grant-type:token-exchange', subject_token_type: 'urn:t3:params:oauth:token-type:environment-bootstrap', requested_token_type: 'urn:ietf:params:oauth:token-type:access_token', subject_token: invite.pairingToken }) })).json();
      assert.equal(typeof auth.access_token, 'string'); phone = auth.access_token;
    }
    assert.equal((await (await req('/api/auth/session', { headers: { authorization: `Bearer ${phone}` } })).json()).authenticated, true);
    const html = await (await req('/opencodex-session')).text();
    const meta = key => html.split(`name="${key}" content="`)[1].split('"')[0];
    const headers = { origin: base, 'x-opencodex-gui-origin': base, 'x-opencodex-api-key': meta('opencodex-session-token'), 'x-opencodex-csrf-token': meta('opencodex-session-csrf') };
    const memory = await (await req('/api/system/memory', { headers })).json();
    assert.equal(memory.runtime, 'rust'); assert(memory.rss > 0);
    if (attempt === 0 && process.platform !== 'win32' && typeof process.execve === 'function') assert.equal(memory.pid, child.pid, 'POSIX bootstrap must be replaced by Rust');
    if (attempt === 1) assert.notEqual(memory.pid, child.pid, 'Forwarding fallback must launch the native child');
    assert.equal((await (await req('/api/stop', { method: 'POST', headers: { ...headers, 'x-opencodex-expected-pid': String(memory.pid) } })).json()).success, true);
    assert.equal(await exited(child), 0); children.delete(child);
    assert.equal(await exists(ready.invitationFile), false);
    if (attempt === 0) {
      const next = await buildPackages(binary, join(root, 'packages-upgrade'), '0.0.1');
      tarballs = pack(next);
      install(); // Exercise an actual npm dependency/version change, not a same-version reinstall.
      for (const name of [mainName, built.name]) {
        assert.equal(JSON.parse(await readFile(join(prefix, 'node_modules', name, 'package.json'), 'utf8')).version, '0.0.1');
      }
    }
  }
  npm(['uninstall', '--prefix', prefix, '--package-lock=false', mainName, built.name]);
  assert.equal(await readFile(join(state, 'sentinel'), 'utf8'), 'Keep state across package replacement and removal');
  assert(await exists(join(state, 'native-connect.sqlite')));
  assert.equal(await exists(launcher), false);
  assert.equal((await readdir(state)).filter(n => n.startsWith('pairing-')).length, 0);
  console.log(JSON.stringify({ platform: built.target, offlinePackInstall: true, helpVersion: true, twoStarts: true, samePairing: true, dashboardStop: true, versionUpgrade: '0.0.0 -> 0.0.1', replacementPreservedState: true, uninstallPreservedState: true, noGlobalInstall: true }));
} finally {
  for (const child of children) {
    if (child.exitCode === null) {
      if (process.platform === 'win32') {
        spawnSync(join(process.env.SystemRoot, 'System32/taskkill.exe'), ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, shell: false, stdio: 'ignore', timeout: 10_000 });
      } else child.kill();
    }
    await exited(child).catch(() => {});
  }
  await rm(root, { recursive: true, force: true });
}
