#!/usr/bin/env node
// Disposable packaging candidate. Does not start or replace the normal rmx service.
import { spawn } from 'node:child_process';
import { createReadStream } from 'node:fs';
import { lstat, open, realpath } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { dirname, join, relative, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';

export const VERSION = '0.0.0';
export function platformKey(platform = process.platform, arch = process.arch, glibc = process.report?.getReport()?.header?.glibcVersionRuntime) {
  if (platform === 'linux' && arch === 'x64' && glibc) return 'linux-x64-gnu';
  if (platform === 'darwin' && arch === 'arm64') return 'darwin-arm64';
  if (platform === 'win32' && arch === 'x64') return 'win32-x64';
  throw new Error(`This isolated native package has no qualified binary for ${platform}/${arch}${platform === 'linux' && !glibc ? ' (non-glibc)' : ''}.`);
}
function versionAtLeast(actual, minimum) {
  const a = actual.split('.').map(Number), b = minimum.split('.').map(Number);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) > (b[i] ?? 0);
  }
  return true;
}
export async function verifyBinary(root, target, glibc = process.report?.getReport()?.header?.glibcVersionRuntime) {
  const metadata = join(root, 'native.json');
  const st = await lstat(metadata);
  if (!st.isFile() || st.isSymbolicLink() || st.size > 4096) throw new Error('Native package metadata is invalid.');
  const handle = await open(metadata, 'r');
  let m;
  try {
    const buffer = Buffer.alloc(4097);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
    if (bytesRead > 4096) throw new Error('Native package metadata is too large.');
    m = JSON.parse(buffer.subarray(0, bytesRead).toString('utf8'));
  } finally { await handle.close(); }
  if (m.formatVersion !== 1 || m.version !== VERSION || m.target !== target || m.migrationComplete !== false ||
      !Number.isSafeInteger(m.bytes) || m.bytes <= 0 || m.bytes > 128 * 1024 * 1024 || !/^[a-f0-9]{64}$/.test(m.sha256)) {
    throw new Error('Native package does not match this launcher.');
  }
  if (target.endsWith('-gnu') && (typeof glibc !== 'string' || typeof m.minimumGlibc !== 'string' || !/^\d+(\.\d+)+$/.test(m.minimumGlibc) || !versionAtLeast(glibc, m.minimumGlibc))) {
    throw new Error('This native artifact requires a newer glibc; do not use an incompatible binary.');
  }
  const path = join(root, target.startsWith('win32-') ? 'remodex-connect-native.exe' : 'remodex-connect-native');
  const before = await lstat(path);
  const resolvedRoot = await realpath(root), resolvedPath = await realpath(path);
  const rel = relative(resolvedRoot, resolvedPath);
  if (!before.isFile() || before.isSymbolicLink() || before.size !== m.bytes || rel.startsWith('..') || isAbsolute(rel)) throw new Error('Native executable is invalid.');
  const hash = createHash('sha256');
  for await (const bytes of createReadStream(path, { highWaterMark: 64 * 1024, end: m.bytes - 1 })) hash.update(bytes);
  const after = await lstat(path);
  if (after.ino !== before.ino || after.size !== before.size || after.mtimeMs !== before.mtimeMs || hash.digest('hex') !== m.sha256) throw new Error('Native executable checksum failed; reinstall the package.');
  return path;
}
export async function main(args = process.argv.slice(2)) {
  const key = platformKey();
  let root;
  try {
    root = dirname(createRequire(import.meta.url).resolve(`@remodex/native-connect-experiment-${key}/package.json`));
  } catch {
    throw new Error(`Native package for ${key} is missing. Reinstall with optional dependencies enabled. No runtime was downloaded or fallback started.`);
  }
  const binary = await verifyBinary(root, key);
  // Newer POSIX Node can replace the bootstrap, leaving no JS process resident.
  // Older Node and Windows keep only this forwarding launcher, not a JS backend.
  if (process.platform !== 'win32' && typeof process.execve === 'function') {
    process.execve(binary, [binary, ...args], process.env);
    throw new Error('Native process replacement failed.');
  }
  // Only argument arrays reach the executable: never a shell, including Windows.
  const child = spawn(binary, args, { stdio: 'inherit', windowsHide: true, shell: false });
  const signals = process.platform === 'win32' ? [] : ['SIGINT', 'SIGTERM', 'SIGHUP'];
  const forwarders = signals.map(signal => [signal, () => child.kill(signal)]);
  for (const [signal, fn] of forwarders) process.on(signal, fn);
  try {
    await new Promise((resolve, reject) => {
      child.once('error', () => reject(new Error('Could not start the isolated native executable.')));
      child.once('exit', (code, signal) => {
        process.exitCode = code ?? (signal === 'SIGINT' ? 130 : 1);
        resolve();
      });
    });
  } finally {
    for (const [signal, fn] of forwarders) process.off(signal, fn);
  }
}
// Resolve both sides: Windows drive casing and npm's bin links can differ from
// the spelling in import.meta.url even when they point at this same module.
if (process.argv[1] && await realpath(process.argv[1]).catch(() => '') === await realpath(fileURLToPath(import.meta.url))) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
