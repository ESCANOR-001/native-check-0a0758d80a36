/** Recover uploads omitted by old Claude public messages, using native UUIDs.
 * Prompt-text equality is never used to choose a message or a source file. */
import { constants } from 'node:fs';
import { open, readdir } from 'node:fs/promises';
import { join, relative, isAbsolute, sep } from 'node:path';
import { createHash } from 'node:crypto';
type Row = Record<string, any>;
const INVALID = 'Native attachment evidence is incomplete or ambiguous; keep the original installation and retry the export.';
type Proof = (root: string, path: string) => Promise<{ sha256: string; bytes: number }>;
export async function claudeNativeUploads(path: string, history: Row, root: string, proof: Proof): Promise<{ path: string; message: string }[]> {
  const messages = new Map<string, Row>((history.thread.messages ?? []).filter((m: Row) => m.role === 'user').map((m: Row) => [m.id, m]));
  const native = new Map<string, Row>(messages);
  for (const d of history.deliveries ?? []) if (d.state !== 'cancelled' && messages.has(d.messageId)) native.set(d.uuid, messages.get(d.messageId)!);
  const found = new Map<string, string>(), wanted: { hash: string; bytes: number; message: string }[] = [];
  const file = await open(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  const before = await file.stat();
  const seen = new Set<string>();
  const deadline = Date.now() + 120_000;
  const pending = Buffer.allocUnsafe(8 * 1024 * 1024 + 64 * 1024);
  let length = 0, total = 0, lines = 0;
  try {
    if (!before.isFile() || before.nlink !== 1 || before.size > 512 * 1024 * 1024) throw new Error(INVALID);
    for (;;) {
      if (Date.now() >= deadline) throw new Error(INVALID);
      const { bytesRead } = await file.read(pending, length, 64 * 1024, null);
      if (!bytesRead) break;
      total += bytesRead;
      if (total > before.size) throw new Error(INVALID);
      length += bytesRead;
      let start = 0;
      for (;;) {
        const end = pending.subarray(0, length).indexOf(10, start);
        if (end < 0) break;
        if (end - start > 8 * 1024 * 1024 || ++lines > 200_000) throw new Error(INVALID);
        const line = pending.subarray(start, end).toString(); start = end + 1;
        if (!line.trim()) continue;
        let value: Row; try { value = JSON.parse(line); } catch { throw new Error(INVALID); }
        if (value.type !== 'user' || value.isMeta || value.isSynthetic || value.isSidechain || value.parent_tool_use_id || value.sessionId !== history.nativeId || value.message?.role !== 'user') continue;
        const blocks = value.message.content;
        if (!Array.isArray(blocks) || !blocks.length || blocks[0].type !== 'text') continue;
        const text = blocks[0].text;
        if (typeof text !== 'string') throw new Error(INVALID);
        if (!text.includes('\n\nAttached files on this PC:\n') && !blocks.some((b: Row) => b.type === 'image')) continue;
        const message = native.get(value.uuid);
        if (!message || seen.has(value.uuid) || blocks.slice(1).some((b: Row) => b.type !== 'image')) throw new Error(INVALID);
        seen.add(value.uuid);
        const prompt = message.text || 'Inspect the attached files.';
        if (text !== prompt) {
          const prefix = `${prompt}\n\nAttached files on this PC:\n`;
          if (!text.startsWith(prefix)) throw new Error(INVALID);
          for (const path of text.slice(prefix.length).split('\n')) {
            const name = relative(root, path);
            if (!isAbsolute(path) || !name || name.startsWith('..' + sep) || name === '..' || isAbsolute(name)) throw new Error(INVALID);
            if (!found.has(path)) found.set(path, message.id);
          }
        }
        for (const image of blocks.slice(1)) {
          const source = image.source;
          if (source?.type !== 'base64' || typeof source.data !== 'string' || source.data.length > 14 * 1024 * 1024) throw new Error(INVALID);
          const bytes = Buffer.from(source.data, 'base64');
          if (!bytes.length || bytes.length > 10 * 1024 * 1024 || bytes.toString('base64') !== source.data) throw new Error(INVALID);
          wanted.push({ hash: createHash('sha256').update(bytes).digest('hex'), bytes: bytes.length, message: message.id });
        }
        if (found.size + wanted.length > 2048) throw new Error(INVALID);
      }
      if (start) { pending.copyWithin(0, start, length); length -= start; }
      if (length > 8 * 1024 * 1024) throw new Error(INVALID);
    }
    const after = await file.stat();
    if (total !== before.size || length || after.mtimeMs !== before.mtimeMs || after.ctimeMs !== before.ctimeMs) throw new Error(INVALID);
  } finally { await file.close(); }
  if (wanted.length) {
    const hashes = new Set(wanted.map(w => w.hash));
    const matches = new Map<string, { path: string; bytes: number }[]>();
    let count = 0, budget = 0;
    for (const client of await readdir(root, { withFileTypes: true })) {
      if (!client.isDirectory() || !/^[a-zA-Z0-9._-]{1,256}$/.test(client.name)) continue;
      for (const turn of await readdir(join(root, client.name), { withFileTypes: true })) {
        if (!turn.isDirectory() || !/^[a-f0-9-]{36}$/i.test(turn.name)) continue;
        for (const entry of await readdir(join(root, client.name, turn.name), { withFileTypes: true })) {
          if (++count > 20_000 || Date.now() >= deadline) throw new Error(INVALID);
          if (!entry.isFile()) continue;
          const path = join(root, client.name, turn.name, entry.name);
          const p = await proof(root, path); budget += p.bytes;
          if (budget > 512 * 1024 * 1024) throw new Error(INVALID);
          if (hashes.has(p.sha256)) matches.set(p.sha256, [...(matches.get(p.sha256) ?? []), { path, bytes: p.bytes }]);
        }
      }
    }
    for (const w of wanted) {
      const candidates = matches.get(w.hash)?.filter(p => p.bytes === w.bytes);
      if (!candidates || candidates.length !== 1) throw new Error(INVALID);
      if (!found.has(candidates[0]!.path)) found.set(candidates[0]!.path, w.message);
    }
  }
  return [...found].map(([path, message]) => ({ path, message }));
}
