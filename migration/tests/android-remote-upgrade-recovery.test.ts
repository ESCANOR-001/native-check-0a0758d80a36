import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { exportOwnedUpgrade } from '../src/android-remote/upgrade-export';
import { assertUpgradeSourceAvailable, SOURCE_HOLD } from '../src/android-remote/upgrade-source-hold';

for (const mode of ['validation-failed', 'stop-uncertain', 'newer-hold', 'selected', 'receipt-exists', 'publishing-started'] as const) {
  test(`failed source export recovery preserves ownership: ${mode}`, async () => {
    const root = await mkdtemp(join(tmpdir(), 'upgrade-source-recovery-'));
    try {
      const source = join(root, 'source'), codex = join(root, 'codex'), claude = join(root, 'claude'), output = join(root, 'export');
      await Promise.all([source, codex, claude].map(p => mkdir(p)));
      const transcript = join(codex, 'session.jsonl');
      await writeFile(transcript, 'original native history\n');
      let stopped = false, released = false;
      await expect(exportOwnedUpgrade(output, { codex, claude }, {
        environment: 'test', sourceRoot: source, connection: {}, projects: [], claudeSettings: {},
        assertSettled() {}, release() { released = true; },
        chats: (async function* () { yield { id: 'chat', agent: 'codex' as const, transcript, history: { messages: ['KEEP ORIGINAL'] } }; })(),
        async stop() {
          expect(() => assertUpgradeSourceAvailable(source)).toThrow();
          if (mode === 'stop-uncertain') throw new Error('Unknown worker exit');
          stopped = true;
          if (mode === 'newer-hold') await writeFile(join(source, SOURCE_HOLD), '{"newer":"DO NOT REMOVE"}');
          if (mode === 'selected') await writeFile(join(source, 'native-upgrade-selected-service.json'), 'DO NOT REMOVE');
          if (mode === 'receipt-exists' || mode === 'publishing-started') await writeFile(join(output, 'closed-owner.json'), 'DO NOT REPLACE');
          if (mode !== 'publishing-started') await writeFile(transcript, 'new remote history retained\n');
        },
      })).rejects.toThrow();
      expect(stopped).toBe(mode !== 'stop-uncertain');
      // The closed controller cannot resume admitting requests. Its fresh
      // lifecycle replacement may restart only the definitely stopped case.
      expect(released).toBe(false);
      if (mode === 'validation-failed') assertUpgradeSourceAvailable(source);
      else expect(() => assertUpgradeSourceAvailable(source)).toThrow();
      expect(JSON.parse(await readFile(join(output, 'codex-0.json'), 'utf8'))).toEqual({ messages: ['KEEP ORIGINAL'] });
      expect(await readFile(transcript, 'utf8')).toBe(['stop-uncertain', 'publishing-started'].includes(mode) ? 'original native history\n' : 'new remote history retained\n');
      if (mode === 'newer-hold') expect(await readFile(join(source, SOURCE_HOLD), 'utf8')).toBe('{"newer":"DO NOT REMOVE"}');
      if (mode === 'selected') expect(await readFile(join(source, 'native-upgrade-selected-service.json'), 'utf8')).toBe('DO NOT REMOVE');
      if (mode === 'receipt-exists' || mode === 'publishing-started') expect(await readFile(join(output, 'closed-owner.json'), 'utf8')).toBe('DO NOT REPLACE');
      else await expect(readFile(join(output, 'closed-owner.json'))).rejects.toThrow();
    } finally { await rm(root, { recursive: true, force: true }); }
  });
}
