/** Upgrade-only reader. Never resume a Desktop-owned chat in a second CLI. */
import { constants } from 'node:fs';
import { open } from 'node:fs/promises';
import { transcriptFingerprint, UPGRADE_BUSY } from './upgrade-export';

type Row = Record<string, any>;
type Client = { request(method: string, params: Row): Promise<any> };
const LIMIT = 12 * 1024 * 1024;
const LINE = 8 * 1024 * 1024;
function object(v: any): v is Row { return v !== null && typeof v === 'object' && !Array.isArray(v); }
function id(v: unknown): string {
  if (typeof v !== 'string' || !v || v.length > 256 || /[\x00-\x1f]/u.test(v)) throw new Error(UPGRADE_BUSY);
  return v;
}
function cursor(v: unknown): string | null {
  if (v === null || v === undefined) return null;
  if (typeof v !== 'string' || !v || v.length > 8192) throw new Error(UPGRADE_BUSY);
  return v;
}
async function lifecycle(path: string, native: string, deadline: number): Promise<{ latest: { turn: string; status: string } | null; settings: Row | null }> {
  const file = await open(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  try {
    const meta = await file.stat();
    if (!meta.isFile() || meta.nlink !== 1 || meta.size > 512 * 1024 * 1024) throw new Error(UPGRADE_BUSY);
    let pending = Buffer.alloc(0), first = true, latest: { turn: string; status: string } | null = null, bytes = 0;
    let settings: Row | null = null;
    const chunk = Buffer.alloc(65536);
    for (;;) {
      if (Date.now() > deadline) throw new Error(UPGRADE_BUSY);
      const { bytesRead } = await file.read(chunk);
      if (!bytesRead) break;
      bytes += bytesRead;
      if (bytes > meta.size) throw new Error(UPGRADE_BUSY);
      pending = Buffer.concat([pending, chunk.subarray(0, bytesRead)]);
      let end: number;
      while ((end = pending.indexOf(10)) >= 0) {
        if (end > LINE) throw new Error(UPGRADE_BUSY);
        const row = JSON.parse(pending.subarray(0, end).toString('utf8'));
        pending = pending.subarray(end + 1);
        if (first) {
          if (row.type !== 'session_meta' || row.payload?.id !== native) throw new Error(UPGRADE_BUSY);
          first = false;
        }
        if (row.type === 'turn_context') {
          const p = row.payload;
          if (!object(p)) throw new Error(UPGRADE_BUSY);
          // Retain only last-used selections, never system/environment text.
          settings = { model: p.model, cwd: p.cwd, effort: p.effort ?? p.collaboration_mode?.settings?.reasoning_effort ?? null,
            serviceTier: p.service_tier ?? null, mode: p.collaboration_mode?.mode,
            runtimeMode: p.approval_policy === 'never' && p.sandbox_policy?.type === 'danger-full-access'
              ? 'full-access' : 'approval-required' };
        }
        if (row.type !== 'event_msg') continue;
        const status = ({ task_started: 'inProgress', task_complete: 'completed', turn_aborted: 'interrupted' } as Row)[row.payload?.type];
        if (status) latest = { turn: id(row.payload.turn_id), status };
      }
      if (pending.length > LINE) throw new Error(UPGRADE_BUSY);
    }
    if (first || pending.length || bytes !== meta.size) throw new Error(UPGRADE_BUSY);
    return { latest, settings };
  } finally { await file.close(); }
}

/** Cap every native page and the accumulated projection; reject partial exports
 * and racing remote prompts. Saved readers can falsely call a live turn stopped,
 * so the final native lifecycle event, not that projection, decides readiness. */
export async function readDesktopExportThread(client: Client, native: string, profile: string, ownerConnected: () => Promise<boolean | null>): Promise<Row> {
  const owner = await ownerConnected();
  if (owner === null) throw new Error(UPGRADE_BUSY);
  const thread = (await client.request('thread/read', { threadId: native, includeTurns: false }))?.thread;
  if (!object(thread) || thread.id !== native || typeof thread.path !== 'string' || thread.turns?.length) throw new Error(UPGRADE_BUSY);
  const before = await transcriptFingerprint(profile, thread.path);
  const deadline = Date.now() + 120_000;
  const { latest } = await lifecycle(thread.path, native, deadline);
  if (latest?.status === 'inProgress') throw new Error(UPGRADE_BUSY);
  const turns: Row[] = [], ids = new Set<string>(), cursors = new Set<string>();
  let next: string | null = null, bytes = 0;
  do {
    if (Date.now() > deadline || turns.length >= 10000) throw new Error(UPGRADE_BUSY);
    const page = await client.request('thread/turns/list', { threadId: native, cursor: next, limit: 1, sortDirection: 'asc', itemsView: 'notLoaded' });
    if (!Array.isArray(page?.data) || page.data.length > 1) throw new Error(UPGRADE_BUSY);
    for (const turn of page.data) {
      if (!object(turn) || ids.has(id(turn.id)) || !['completed', 'interrupted', 'failed'].includes(turn.status)) throw new Error(UPGRADE_BUSY);
      ids.add(turn.id);
      const items: Row[] = [], seen = new Set<string>(), pages = new Set<string>();
      let token: string | null = null;
      do {
        if (Date.now() > deadline || items.length >= 32768) throw new Error(UPGRADE_BUSY);
        const part = await client.request('thread/items/list', { threadId: native, turnId: turn.id, cursor: token, limit: 8, sortDirection: 'asc' });
        if (!Array.isArray(part?.data) || part.data.length > 8) throw new Error(UPGRADE_BUSY);
        for (const row of part.data) {
          const item = row?.item ?? row;
          if (!object(item) || seen.has(id(item.id))) throw new Error(UPGRADE_BUSY);
          seen.add(item.id);
          bytes += Buffer.byteLength(JSON.stringify(item));
          if (bytes > LIMIT) throw new Error('This history exceeds the bounded upgrade projection; all source history is retained.');
          items.push(item);
        }
        token = cursor(part.nextCursor);
        if (token && (!part.data.length || pages.has(token))) throw new Error(UPGRADE_BUSY);
        if (token) pages.add(token);
      } while (token);
      turns.push({ ...turn, items, itemsView: 'full', ...(latest && latest.turn === turn.id ? { status: latest.status } : {}) });
    }
    next = cursor(page.nextCursor);
    if (next && (!page.data.length || cursors.has(next))) throw new Error(UPGRADE_BUSY);
    if (next) cursors.add(next);
  } while (next);
  if ((latest === null) !== (turns.length === 0) || latest && turns.at(-1)?.id !== latest.turn) throw new Error(UPGRADE_BUSY);
  if (await ownerConnected() !== owner) throw new Error(UPGRADE_BUSY);
  const after = await transcriptFingerprint(profile, thread.path);
  if (before.sha256 !== after.sha256 || before.bytes !== after.bytes) throw new Error(UPGRADE_BUSY);
  return { ...thread, turns, status: { type: 'idle' } };
}

/** Unaliased Desktop chats keep the native UUID as their public ID. Capture
 * actual last-used selections; never use the UI's fallback model for migration.
 * Future turns inherit the live Desktop owner's settings through its IPC API. */
export async function readDesktopExportHeader(client: Client, native: string, profile: string, ownerConnected: () => Promise<boolean | null>): Promise<Row> {
  const owner = await ownerConnected();
  if (owner === null) throw new Error(UPGRADE_BUSY);
  const thread = (await client.request('thread/read', { threadId: native, includeTurns: false }))?.thread;
  if (!object(thread) || thread.id !== native || typeof thread.path !== 'string'
    || thread.turns?.length || !['openai', 'codex'].includes(thread.modelProvider)) throw new Error(UPGRADE_BUSY);
  const before = await transcriptFingerprint(profile, thread.path);
  const { latest, settings } = await lifecycle(thread.path, native, Date.now() + 120_000);
  if (latest?.status === 'inProgress' || !settings || settings.cwd !== thread.cwd
    || typeof settings.model !== 'string' || !settings.model || settings.model.length > 256
    || !['default', 'plan'].includes(settings.mode)
    || [settings.effort, settings.serviceTier].some(v => v !== null && (typeof v !== 'string' || !v || v.length > 128))) throw new Error(UPGRADE_BUSY);
  if (await ownerConnected() !== owner) throw new Error(UPGRADE_BUSY);
  const after = await transcriptFingerprint(profile, thread.path);
  if (before.sha256 !== after.sha256 || before.bytes !== after.bytes) throw new Error(UPGRADE_BUSY);
  return { ...thread, status: { type: 'idle' }, androidRemoteModel: settings.model,
    androidRemoteRuntimeMode: settings.runtimeMode, androidRemoteInteractionMode: settings.mode,
    androidRemoteNativeEffort: settings.effort, androidRemoteNativeServiceTier: settings.serviceTier };
}
