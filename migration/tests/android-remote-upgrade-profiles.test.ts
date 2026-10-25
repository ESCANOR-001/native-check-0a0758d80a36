import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, symlink, rm, realpath, readdir, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { optionalUpgradeProfile } from '../src/android-remote/upgrade-profiles';
import { exportOwnedUpgrade, type UpgradeExportOwner } from '../src/android-remote/upgrade-export';

test('upgrade distinguishes an absent default profile from explicit and broken profiles', async () => {
  const root = await mkdtemp(join(tmpdir(), 'upgrade-optional-profile-'));
  try {
    const missing = join(root, 'missing');
    expect(await optionalUpgradeProfile(missing, false)).toBeUndefined();
    await expect(optionalUpgradeProfile(missing, true)).rejects.toThrow();
    const dir = join(root, 'valid'); await mkdir(dir);
    expect(await optionalUpgradeProfile(dir, false)).toBe(await realpath(dir));
    const file = join(root, 'file'); await writeFile(file, 'not a profile');
    await expect(optionalUpgradeProfile(file, false)).rejects.toThrow();
    if (process.platform !== 'win32') {
      const link = join(root, 'broken-link'); await symlink(missing, link);
      await expect(optionalUpgradeProfile(link, false)).rejects.toThrow();
    }
    expect(await readdir(root)).not.toContain('missing');
  } finally { await rm(root, { recursive: true, force: true }); }
});

for (const agent of ['codex', 'claude'] as const) {
  test(`${agent}-only export retains history and refuses an omitted profile before stopping`, async () => {
    const root = await mkdtemp(join(tmpdir(), 'upgrade-single-agent-'));
    try {
      const profile = join(root, agent); await mkdir(profile);
      const transcript = join(profile, 'session.jsonl'); await writeFile(transcript, 'retained history\n');
      for (const present of [true, false]) {
        let stopped = false, released = false;
        const messages = [{ role: 'user', text: 'original prompt' }, { role: 'assistant', text: 'original reply' }];
        const history = agent === 'claude' ? { thread: { messages } } : { messages };
        const owner: UpgradeExportOwner = {
          environment: 'test-only', connection: { clients: [] }, projects: [], claudeSettings: {},
          chats: (async function* () { yield { agent, id: 'saved-chat', transcript, history }; })(),
          assertSettled() {}, async stop() { stopped = true; }, release() { released = true; },
        };
        const destination = join(root, present ? 'accepted' : 'refused');
        if (present) {
          const result = await exportOwnedUpgrade(destination, { [agent]: profile }, owner);
          const receipt = JSON.parse(await readFile(result.receipt, 'utf8'));
          expect(receipt.profiles).toEqual({ [agent]: profile });
          expect(receipt.chats).toHaveLength(1);
          expect(JSON.parse(await readFile(join(destination, `${agent}-0.json`), 'utf8'))).toEqual(history);
          expect(stopped).toBe(true); expect(released).toBe(false);
        } else {
          await expect(exportOwnedUpgrade(destination, {}, owner)).rejects.toThrow('profile missing');
          expect(stopped).toBe(false); expect(released).toBe(true);
          expect(await readdir(destination)).not.toContain('closed-owner.json');
        }
        expect(await readFile(transcript, 'utf8')).toBe('retained history\n');
      }
    } finally { await rm(root, { recursive: true, force: true }); }
  });
}
