import { test, expect } from 'bun:test';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { createHash } from 'node:crypto';
import { verifyShutdownBookkeeping } from '../src/android-remote/upgrade-shutdown-proof';
test('only unchanged history plus known shutdown bookkeeping is accepted', async () => {
  const root = await mkdtemp(join(tmpdir(), 'shutdown-proof-'));
  try {
    const file = join(root, 'native.jsonl');
    const old = JSON.stringify({ type: 'user', message: 'keep me' }) + '\n';
    const hash = createHash('sha256').update(old).digest('hex');
    for (const [agent, row, accept] of [
      ['codex', { type: 'event_msg', payload: { type: 'thread_settings_applied' } }, true],
      ['claude', { type: 'bridge-session' }, true], ['claude', { type: 'cost-state' }, true],
      ['codex', { type: 'event_msg', payload: { type: 'user_message' } }, false],
      ['claude', { type: 'assistant', message: 'new answer' }, false],
      ['claude', { type: 'user', message: 'new prompt' }, false],
      ['claude', { type: 'unknown' }, false],
    ] as const) {
      await writeFile(file, old + JSON.stringify(row) + '\n');
      const result = verifyShutdownBookkeeping(file, agent, Buffer.byteLength(old), hash);
      if (accept) await result; else await expect(result).rejects.toThrow();
    }
    await writeFile(file, old.replace('keep me', 'edit me') + '{"type":"cost-state"}\n');
    await expect(verifyShutdownBookkeeping(file, 'claude', Buffer.byteLength(old), hash)).rejects.toThrow();
    await writeFile(file, old + '{"type":"cost-state"}');
    await expect(verifyShutdownBookkeeping(file, 'claude', Buffer.byteLength(old), hash)).rejects.toThrow();
  } finally { await rm(root, { recursive: true, force: true }); }
});
