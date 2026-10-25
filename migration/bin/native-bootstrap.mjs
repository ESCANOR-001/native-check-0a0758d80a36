// One-time compatibility owner for installations predating the export hook.
// No service is stopped and no PID is signalled except the child we create.
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { createRequire } from 'node:module';
import { lstat, readFile, realpath } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { isRealBunBinary } from '../src/lib/bun-binary-validator.mjs';

async function exists(path) {
  try { await lstat(path); return true; }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
}
export async function needsSourceBootstrap(installation) {
  // Never fall back after an attempt, including malformed/interrupted attempts.
  if (await exists(join(installation, 'npm-upgrade.sqlite')) || await exists(join(installation, 'npm-source.json'))) return false;
  const source = dirname(installation);
  if (await exists(join(source, 'native-upgrade-source-hold.json'))) throw new Error('An earlier upgrade retained this source. Resume its existing recovery record.');
  const config = join(source, 'android-remote.json');
  const stat = await lstat(config).catch(error => { if (error.code === 'ENOENT') return null; throw error; });
  if (!stat) throw new Error('No existing Remodex installation was found. This native candidate requires an existing installation.');
  const directory = await lstat(source);
  if (!stat.isFile() || stat.isSymbolicLink() || stat.nlink !== 1 || stat.size > 16 * 1024 * 1024
    || !directory.isDirectory() || directory.isSymbolicLink()) throw new Error('The source installation could not be verified.');
  const value = JSON.parse(await readFile(config, 'utf8'));
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Invalid source connection state.');
  return true;
}
function runtime(packageRoot) {
  const override = process.env.OPENCODEX_BUN_PATH?.trim();
  if (override) {
    const path = resolve(override);
    if (!isRealBunBinary(path)) throw new Error('The configured compatibility runtime is unavailable.');
    return path;
  }
  const directory = dirname(createRequire(join(packageRoot, 'package.json')).resolve('bun/package.json'));
  for (const name of ['bun.exe', 'bun']) {
    const path = join(directory, 'bin', name);
    if (isRealBunBinary(path)) return path;
  }
  throw new Error('The one-time upgrade runtime is missing. Reinstall with the package dependencies enabled.');
}
export async function bootstrapSource(packageRoot, installation, prepareSource, launchArguments = []) {
  if (!await needsSourceBootstrap(installation)) return;
  const bun = runtime(packageRoot);
  const script = join(packageRoot, 'src/cli/native-upgrade-source.ts');
  const stat = await lstat(script);
  if (!stat.isFile() || stat.isSymbolicLink() || await realpath(script) !== join(await realpath(packageRoot), 'src/cli/native-upgrade-source.ts')) throw new Error('Invalid one-time source launcher.');
  const proof = randomBytes(32).toString('base64url');
  const env = { ...process.env, OPENCODEX_HOME: dirname(installation), REMODEX_CONNECT_ONLY: '1',
    OCX_NATIVE_UPGRADE_SOURCE: '1', OCX_NODE_LAUNCHER_PATH: process.execPath,
    OCX_BUN_RUNTIME_PATH: bun, OCX_BUN_RUNTIME_SOURCE: process.env.OPENCODEX_BUN_PATH ? 'override' : 'bundled',
    OCX_NODE_LAUNCH_CONTEXT: JSON.stringify({ version: 1, proof, anthropicEnvSlots:
      ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL'].filter(key => !!process.env[key]) }),
  };
  const child = spawn(bun, [script, `--ocx-internal-launch-proof=${proof}`, 'start', ...launchArguments], {
    env, stdio: 'inherit', shell: false, windowsHide: true,
  });
  let exit, failed;
  const ended = new Promise(resolve => {
    child.once('error', error => { failed = error; resolve(); });
    child.once('exit', code => { exit = code ?? 1; resolve(); });
  });
  const abort = new AbortController();
  const forward = () => { abort.abort(); child.kill('SIGTERM'); };
  const signals = process.platform === 'win32' ? ['SIGINT', 'SIGTERM'] : ['SIGINT', 'SIGTERM', 'SIGHUP'];
  for (const signal of signals) process.on(signal, forward);
  try {
    const deadline = Date.now() + 120_000;
    while (!await exists(join(installation, 'npm-source.json'))) {
      if (exit !== undefined || failed || abort.signal.aborted) throw new Error('The source bridge did not start. Any already-running bridge was left alone.');
      if (Date.now() >= deadline) throw new Error('The source did not become ready for upgrade in time.');
      await delay(100, undefined, { signal: abort.signal });
    }
    await prepareSource(installation, abort.signal);
    await Promise.race([ended, delay(20_000, undefined, { signal: abort.signal }).then(() => { throw new Error('The source owner did not retire after its export.'); })]);
    if (failed || exit !== 0) throw new Error('The source owner did not complete a clean handoff.');
  } finally {
    if (exit === undefined && !failed) {
      child.kill('SIGTERM');
      await Promise.race([ended, delay(20_000)]);
    }
    abort.abort();
    for (const signal of signals) process.off(signal, forward);
  }
}
