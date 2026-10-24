/** Permit only append-only native bookkeeping flushed during graceful stop.
 * Any conversation/tool event or edit to the previously verified prefix fails. */
import { constants } from 'node:fs';
import { open } from 'node:fs/promises';
import { createHash } from 'node:crypto';
const ERROR = 'Native history changed during shutdown. Export was retained but not activated; capture it again from its original owner.';
export async function verifyShutdownBookkeeping(path: string, agent: string, bytes: number, sha256: string): Promise<void> {
  const file = await open(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  try {
    const before = await file.stat();
    if (!before.isFile() || before.nlink !== 1 || before.size < bytes || before.size - bytes > 2 * 1024 * 1024) throw new Error(ERROR);
    const hash = createHash('sha256'), buffer = Buffer.allocUnsafe(64 * 1024);
    const deadline = Date.now() + 120_000;
    let position = 0, last = 10;
    while (position < bytes) {
      if (Date.now() >= deadline) throw new Error(ERROR);
      const result = await file.read(buffer, 0, Math.min(buffer.length, bytes - position), position);
      if (!result.bytesRead) throw new Error(ERROR);
      hash.update(buffer.subarray(0, result.bytesRead)); position += result.bytesRead; last = buffer[result.bytesRead - 1]!;
    }
    if (hash.digest('hex') !== sha256 || last !== 10) throw new Error(ERROR);
    const tail = Buffer.alloc(before.size - bytes);
    let read = 0;
    while (read < tail.length) {
      const result = await file.read(tail, read, tail.length - read, bytes + read);
      if (!result.bytesRead) throw new Error(ERROR); read += result.bytesRead;
    }
    if (tail.length && tail.at(-1) !== 10) throw new Error(ERROR);
    const lines = tail.toString('utf8').split('\n');
    if (lines.length > 4096) throw new Error(ERROR);
    for (const line of lines) {
      if (!line) continue;
      let row; try { row = JSON.parse(line); } catch { throw new Error(ERROR); }
      const bookkeeping = agent === 'codex'
        ? row.type === 'event_msg' && row.payload?.type === 'thread_settings_applied'
        : agent === 'claude' && ['bridge-session', 'cost-state'].includes(row.type);
      if (!bookkeeping) throw new Error(ERROR);
    }
    const after = await file.stat();
    if (after.size !== before.size || after.mtimeMs !== before.mtimeMs || after.ctimeMs !== before.ctimeMs) throw new Error(ERROR);
  } finally { await file.close(); }
}
