/** Fail-closed startup marker for an explicitly exported source installation.
 * Its presence blocks the old owner, including after a process restart. Never
 * remove it automatically after a partially successful native activation. */
import { constants, lstatSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { lstat, open, realpath, unlink } from 'node:fs/promises';
import { isAbsolute, join, resolve } from 'node:path';

export const SOURCE_HOLD = 'native-upgrade-source-hold.json';
export const SOURCE_HELD = 'This source installation is held for a native upgrade. Use the selected native service; the old agent workers were not restarted.';
export function assertUpgradeSourceAvailable(root: string) {
  try { lstatSync(join(root, SOURCE_HOLD)); }
  catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return;
    throw new Error(SOURCE_HELD);
  }
  // Even malformed, linked or incomplete markers block startup.
  throw new Error(SOURCE_HELD);
}
export async function holdUpgradeSource(root: string, exportDirectory: string, environment: string) {
  if (!isAbsolute(root) || !isAbsolute(exportDirectory) || resolve(exportDirectory) !== exportDirectory) throw new Error(SOURCE_HELD);
  const canonical = await realpath(root);
  assertUpgradeSourceAvailable(canonical);
  const path = join(canonical, SOURCE_HOLD);
  const contents = JSON.stringify({ version: 1, environment, export: exportDirectory });
  const file = await open(path, 'wx', 0o600);
  try {
    await file.writeFile(contents);
    await file.sync();
  } finally { await file.close(); }
  if (process.platform !== 'win32') {
    const directory = await open(canonical, 'r');
    try { await directory.sync(); } finally { await directory.close(); }
  }
  return { path, contents, directory: canonical, exportDirectory };
}

/** Persist only AFTER the lifecycle owner confirms every owned worker exited.
 * A later local recovery also requires this exporter process to have exited;
 * PID absence alone is never evidence that its workers stopped safely. */
export async function recordStoppedUpgradeOwner(hold: Awaited<ReturnType<typeof holdUpgradeSource>>, environment: string) {
  const path = join(hold.exportDirectory, 'stopped-owner.json');
  const contents = JSON.stringify({ version: 1, phase: 'workers-stopped', pid: process.pid, environment,
    export: hold.exportDirectory, source_hold: { path: hold.path, sha256: createHash('sha256').update(hold.contents).digest('hex') } });
  const file = await open(path, 'wx', 0o600);
  try { await file.writeFile(contents); await file.sync(); } finally { await file.close(); }
  if (process.platform !== 'win32') {
    const directory = await open(hold.exportDirectory, 'r');
    try { await directory.sync(); } finally { await directory.close(); }
  }
}

/** Only the living lifecycle owner may call this, after its own stop resolves
 * and before it begins publishing a receipt. An uncertain stop, process crash,
 * published/partial receipt or newer selection must keep startup inhibited. */
export async function releaseUnpublishedStoppedExport(hold: Awaited<ReturnType<typeof holdUpgradeSource>>) {
  for (const path of [join(hold.exportDirectory, 'closed-owner.json'), join(hold.directory, 'native-upgrade-selected-service.json')]) {
    try { await lstat(path); }
    catch (error) { if ((error as NodeJS.ErrnoException).code === 'ENOENT') continue; throw new Error(SOURCE_HELD); }
    throw new Error(SOURCE_HELD);
  }
  if (await realpath(hold.path) !== hold.path) throw new Error(SOURCE_HELD);
  const file = await open(hold.path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  try {
    const before = await file.stat();
    if (!before.isFile() || before.nlink !== 1 || before.size !== Buffer.byteLength(hold.contents) || before.size > 4096) throw new Error(SOURCE_HELD);
    const buffer = Buffer.alloc(4097);
    const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
    const named = await lstat(hold.path);
    if (bytesRead !== before.size || buffer.subarray(0, bytesRead).toString('utf8') !== hold.contents
      || named.isSymbolicLink() || named.dev !== before.dev || named.ino !== before.ino
      || named.size !== before.size || named.mtimeMs !== before.mtimeMs || named.ctimeMs !== before.ctimeMs) throw new Error(SOURCE_HELD);
    await unlink(hold.path);
  } finally { await file.close(); }
  if (process.platform !== 'win32') {
    const directory = await open(hold.directory, 'r');
    try { await directory.sync(); } finally { await directory.close(); }
  }
}
