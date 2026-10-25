// Package-local native selection. npm install never invokes this module. An
// explicit rmx start discovers the source handoff in this installation's state.
import { lstat, readFile, realpath } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { bootstrapSource, needsSourceBootstrap } from './native-bootstrap.mjs';

export function installationDirectory(env = process.env, home = homedir()) {
  const raw = env.OPENCODEX_HOME?.trim() || join(home, '.remodex-connect');
  const expanded = raw === '~' ? home : /^~[/\\]/.test(raw) ? join(home, raw.slice(2)) : raw;
  return join(resolve(expanded), 'native-installation');
}
export function packageArguments(args, installation) {
  if (args.length === 0 || args.length === 1 && ['help','--help','-h'].includes(args[0])) return ['--help'];
  if (args.length === 3 && args[0] === 'start' && args[1] === '--port' && /^[1-9][0-9]{0,4}$/.test(args[2]) && Number(args[2]) <= 65535) return ['--npm-upgrade', 'start', installation, args[2]];
  if (args.length !== 1) throw new Error('Native Remodex expects one command; installation paths come from OPENCODEX_HOME.');
  switch (args[0]) {
    case 'start': return ['start', installation];
    case 'status': case 'stop': case 'restart': return ['--npm-upgrade', args[0], installation];
    case 'version': case '--version': case '-v': return ['--version'];
    default: throw new Error(`'${args[0]}' is not yet qualified for the native package. No legacy backend was started.`);
  }
}
export async function tryNativeConnect(packageRoot, args = process.argv.slice(2)) {
  const metadata = join(packageRoot,'bin/native-runtime.json');
  let stat;
  try { stat = await lstat(metadata); }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
  if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 1024) throw new Error('Invalid native package selection.');
  const m = JSON.parse(await readFile(metadata,'utf8'));
  const pkg = JSON.parse(await readFile(join(packageRoot,'package.json'),'utf8'));
  if (m.formatVersion !== 1 || m.version !== pkg.version || m.backend !== 'rust' || m.migrationComplete !== false) throw new Error('Native package selection does not match this release.');
  const modulePath=join(packageRoot,'bin/native/launcher.mjs');
  const moduleStat=await lstat(modulePath);
  if (!moduleStat.isFile() || moduleStat.isSymbolicLink() || await realpath(modulePath) !== join(await realpath(packageRoot),'bin/native/launcher.mjs')) throw new Error('Invalid native launcher path.');
  const launcher=await import(pathToFileURL(modulePath).href);
  if (launcher.VERSION !== m.version) throw new Error('Native launcher version mismatch.');
  const installation = installationDirectory();
  const nativeArgs = packageArguments(args, installation);
  if (args[0] === 'start' && await needsSourceBootstrap(installation)) {
    await bootstrapSource(packageRoot, installation, launcher.prepareSource, args.slice(1));
  }
  await launcher.main(nativeArgs);
  return true;
}
