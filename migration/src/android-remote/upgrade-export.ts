/** Source-owner export boundary. There is deliberately no HTTP route: only the
 * local lifecycle owner may reserve admission and stop its own runtime. */
import { createHash } from 'node:crypto';
import { constants } from 'node:fs';
import { mkdir, open, realpath, type FileHandle } from 'node:fs/promises';
import { isAbsolute, relative, resolve, sep } from 'node:path';
import { exportChatAssets, type AssetProof } from './upgrade-export-assets';
import { verifyShutdownBookkeeping } from './upgrade-shutdown-proof';
import { holdUpgradeSource, recordStoppedUpgradeOwner, releaseUnpublishedStoppedExport } from './upgrade-source-hold';
import { desktopTranscriptReference } from './upgrade-desktop-reference';

export const UPGRADE_BUSY = 'Upgrade is waiting for settled chats and exclusive ownership. Finish pending work and retry; no chat was moved.';
export class UpgradeAdmission {
  private reserved = false;
  private requests = 0;
  assertAvailable(): void { if (this.reserved) throw new Error(UPGRADE_BUSY); }
  enter(): () => void {
    if (this.reserved) throw new Error(UPGRADE_BUSY);
    this.requests++;
    let released = false;
    return () => { if (!released) { released = true; this.requests--; } };
  }
  reserve(): () => void {
    if (this.reserved || this.requests) throw new Error(UPGRADE_BUSY);
    this.reserved = true;
    let released = false;
    return () => { if (!released) { released = true; this.reserved = false; } };
  }
}
type Row = Record<string, any>;
/** Mount only an already-proven, locally owned saved chat. Resume sends no
 * prompt, and both snapshots must be idle before a complete export is read. */
export async function readOwnedExportThread(client: { request(method: string, params: Row): Promise<any> }, id: string): Promise<Row> {
  const state = (value: Row | undefined) => typeof value?.status === 'string' ? value.status : value?.status?.type;
  const loaded = (await client.request('thread/loaded/list', {}))?.data;
  if (!Array.isArray(loaded) || loaded.some(id => typeof id !== 'string')) throw new Error(UPGRADE_BUSY);
  if (!loaded.includes(id)) {
    const resumed = (await client.request('thread/resume', { threadId: id, excludeTurns: true }))?.thread;
    if (!resumed || resumed.id !== id || state(resumed) !== 'idle') throw new Error(UPGRADE_BUSY);
  } else {
    const header = (await client.request('thread/read', { threadId: id, includeTurns: false }))?.thread;
    if (!header || header.id !== id || state(header) !== 'idle') throw new Error(UPGRADE_BUSY);
  }
  const full = (await client.request('thread/read', { threadId: id, includeTurns: true }))?.thread;
  if (!full || full.id !== id || state(full) !== 'idle' || !Array.isArray(full.turns)) throw new Error(UPGRADE_BUSY);
  return full;
}
export interface UpgradeExportChat {
  agent: 'codex' | 'claude';
  id: string;
  history: Row;
  transcript: string;
  desktopReference?: boolean;
}
export interface UpgradeExportOwner {
  environment: string;
  sourceRoot?: string;
  assetRoots?: { uploads: string; images: string; claudeUploads?: string };
  connection: unknown;
  projects: unknown;
  claudeSettings: unknown;
  chats: AsyncIterable<UpgradeExportChat>;
  /** Recheck pending operations immediately before stopping. */
  assertSettled(): void;
  /** Graceful shutdown of this owner's workers, never a discovered PID. */
  stop(): Promise<void>;
  /** Release only if shutdown has not begun. After shutdown, a fresh owner is required. */
  release(): void;
}
export function assertSettledExport(thread: Row) {
  if (!thread || !['idle', 'ready', 'disconnected'].includes(thread.session?.status)
    || thread.latestTurn && !['completed', 'interrupted', 'error'].includes(thread.latestTurn.state)
    || thread.hasPendingApprovals || thread.hasPendingUserInput || thread.hasActionableProposedPlan
    || thread.historyPage?.olderCursor || thread.messages?.some((m: Row) => m.streaming || m.phase === 'queued')) {
    throw new Error(UPGRADE_BUSY);
  }
}
/** Native transcript fingerprints catch remote input racing the shutdown. No
 * mismatched source can be marked ready, even when a worker's exit succeeded. */
export async function transcriptFingerprint(profile: string, path: string, destination?: string, limit = 512 * 1024 * 1024): Promise<{ relative: string; sha256: string; bytes: number }> {
  const root = await realpath(profile);
  if (!isAbsolute(path) || resolve(path) !== path) throw new Error(UPGRADE_BUSY);
  const local = relative(resolve(profile), path);
  if (!local || local === '..' || local.startsWith('..' + sep) || isAbsolute(local)) throw new Error(UPGRADE_BUSY);
  // Check every component, including directory links on Windows.
  let current = root;
  for (const part of local.split(sep)) {
    current = resolve(current, part);
    if (await realpath(current) !== current) throw new Error(UPGRADE_BUSY);
  }
  const checkedPath = current;
  const file = await open(checkedPath, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  let output: FileHandle | undefined;
  try {
    const before = await file.stat();
    if (!before.isFile() || before.nlink !== 1 || before.size > limit || destination && before.size === 0) throw new Error(UPGRADE_BUSY);
    if (destination) output = await open(destination, 'wx', 0o600);
    const hash = createHash('sha256');
    const buffer = Buffer.allocUnsafe(64 * 1024);
    let size = 0;
    const deadline = Date.now() + 120_000;
    for (;;) {
      if (Date.now() >= deadline) throw new Error(UPGRADE_BUSY);
      const { bytesRead } = await file.read(buffer, 0, buffer.length, null);
      if (!bytesRead) break;
      size += bytesRead;
      if (size > before.size) throw new Error(UPGRADE_BUSY);
      hash.update(buffer.subarray(0, bytesRead));
      await output?.writeFile(buffer.subarray(0, bytesRead));
    }
    const after = await file.stat();
    const named = await open(checkedPath, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
    try {
      const now = await named.stat();
      if (size !== before.size || after.size !== size || before.mtimeMs !== after.mtimeMs
        || before.ctimeMs !== after.ctimeMs || now.ino !== before.ino || now.dev !== before.dev) throw new Error(UPGRADE_BUSY);
    } finally { await named.close(); }
    await output?.sync();
    return { relative: local.split(sep).join('/'), sha256: hash.digest('hex'), bytes: size };
  } finally { await output?.close(); await file.close(); }
}
async function writePrivate(path: string, value: unknown) {
  const bytes = Buffer.from(JSON.stringify(value));
  if (bytes.length > 16 * 1024 * 1024) throw new Error('Export exceeds the safe history limit; source history remains intact.');
  const file = await open(path, 'wx', 0o600);
  try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
}
/** Write an immutable, closed-owner receipt. Native snapshotting/activation is
 * separate: an interrupted export never has a completed receipt. */
export async function exportOwnedUpgrade(
  directory: string,
  profiles: { codex: string; claude: string },
  owner: UpgradeExportOwner,
): Promise<{ receipt: string; chats: number }> {
  let stopping = false, stopped = false, publishing = false;
  let hold: Awaited<ReturnType<typeof holdUpgradeSource>> | undefined;
  try {
    if (!isAbsolute(directory) || resolve(directory) !== directory) throw new Error(UPGRADE_BUSY);
    const parent = await realpath(resolve(directory, '..'));
    const target = resolve(parent, directory.split(sep).at(-1)!);
    for (const path of Object.values(profiles)) {
      const source = await realpath(path);
      const rel = relative(source, target), reverse = relative(target, source);
      if (!rel || !reverse || !rel.startsWith('..' + sep) && !isAbsolute(rel)
        || !reverse.startsWith('..' + sep) && !isAbsolute(reverse)) throw new Error(UPGRADE_BUSY);
    }
    owner.assertSettled();
    await mkdir(target, { mode: 0o700 });
    await writePrivate(resolve(target, 'connection.json'), owner.connection);
    await writePrivate(resolve(target, 'codex-projects.json'), owner.projects);
    await writePrivate(resolve(target, 'claude-settings.json'), owner.claudeSettings);
    const chats: { agent: string; id: string; history: string; transcript: string; sha256: string; bytes: number; desktopReference?: boolean; nativeId?: string; uploads?: Row; images?: Row }[] = [];
    const assets: AssetProof[] = [];
    let assetBytes = 0;
    const seen = new Set<string>();
    let total = 0;
    for await (const chat of owner.chats) {
      if (chats.length >= 10_000 || seen.has(chat.id)) throw new Error(UPGRADE_BUSY);
      seen.add(chat.id);
      const history = `${chat.agent}-${chats.length}.json`;
      total += Buffer.byteLength(JSON.stringify(chat.history));
      if (total > 512 * 1024 * 1024) throw new Error(UPGRADE_BUSY);
      if (chat.desktopReference && chat.agent !== 'codex') throw new Error(UPGRADE_BUSY);
      const proof = chat.desktopReference
        ? await desktopTranscriptReference(profiles.codex, chat.transcript, chat.history.nativeId)
        : await transcriptFingerprint(profiles[chat.agent], chat.transcript);
      const inventory = await exportChatAssets(target, chats.length, chat, owner.assetRoots, transcriptFingerprint, bytes => {
        assetBytes += bytes; if (assetBytes > 512 * 1024 * 1024) throw new Error(UPGRADE_BUSY);
      });
      if (assets.length + inventory.proofs.length > 20_000) throw new Error(UPGRADE_BUSY);
      assets.push(...inventory.proofs);
      await writePrivate(resolve(target, history), chat.desktopReference ? {
        version: 1, threadId: chat.id, nativeId: chat.history.nativeId, profile: profiles.codex,
        transcript: proof.relative, firstLineSha256: proof.sha256, firstLineBytes: proof.bytes,
      } : chat.history);
      chats.push({ agent: chat.agent, id: chat.id, history, transcript: proof.relative, sha256: proof.sha256, bytes: proof.bytes,
        ...(chat.desktopReference ? { desktopReference: true, nativeId: chat.history.nativeId } : {}),
        uploads: inventory.uploads, images: inventory.images });
    }
    owner.assertSettled();
    stopping = true;
    hold = owner.sourceRoot ? await holdUpgradeSource(owner.sourceRoot, target, owner.environment) : undefined;
    owner.assertSettled();
    await owner.stop();
    stopped = true;
    if (hold) await recordStoppedUpgradeOwner(hold, owner.environment);
    for (const chat of chats) {
      const agent = chat.agent as 'codex' | 'claude';
      if (chat.desktopReference) {
        const proof = await desktopTranscriptReference(profiles.codex, resolve(profiles.codex, chat.transcript), chat.nativeId!);
        if (proof.sha256 !== chat.sha256 || proof.bytes !== chat.bytes) throw new Error(UPGRADE_BUSY);
        continue;
      }
      const proof = await transcriptFingerprint(profiles[agent], resolve(profiles[agent], chat.transcript));
      if (proof.sha256 !== chat.sha256) {
        await verifyShutdownBookkeeping(resolve(profiles[agent], chat.transcript), agent, chat.bytes, chat.sha256);
        const stable = await transcriptFingerprint(profiles[agent], resolve(profiles[agent], chat.transcript));
        if (stable.sha256 !== proof.sha256) throw new Error(UPGRADE_BUSY);
        chat.sha256 = proof.sha256;
      }
    }
    for (const asset of assets) {
      const proof = await transcriptFingerprint(asset.root, asset.path, undefined, 16 * 1024 * 1024);
      if (proof.sha256 !== asset.sha256 || proof.bytes !== asset.bytes) throw new Error('An attachment changed during shutdown. The export was retained but not activated.');
    }
    const receipt = resolve(target, 'closed-owner.json');
    publishing = true;
    await writePrivate(receipt, { version: 1, profile_mode: 'existing', environment: owner.environment, source_hold: hold?.path, profiles, chats: chats.map(({ bytes, nativeId, ...chat }) => chat),
      connection: 'connection.json', codex_projects: 'codex-projects.json', claude_settings: 'claude-settings.json' });
    if (process.platform !== 'win32') {
      const dir = await open(target, 'r'); try { await dir.sync(); } finally { await dir.close(); }
    }
    return { receipt, chats: chats.length };
  } catch (error) {
    // No native generation can be prepared before a receipt exists. A living
    // owner that definitely stopped all its workers may undo this attempt's
    // hold when final validation fails, retaining all source/partial copies.
    // Keep this controller closed: only a fresh lifecycle owner may restart.
    if (stopped && !publishing && hold) await releaseUnpublishedStoppedExport(hold);
    throw error;
  } finally { if (!stopping) owner.release(); }
}
