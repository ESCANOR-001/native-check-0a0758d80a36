import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, readFile, rm, appendFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { readMissingDesktopUpgradeHeaders, desktopUpgradeAliases, annotateUpgradeAlias } from '../src/android-remote/upgrade-desktop-inventory';

const old = '11111111-1111-4111-8111-111111111111';
const fresh = '22222222-2222-4222-8222-222222222222';
for (const mode of ['settled', 'dormant', 'archived', 'missing-owner', 'live', 'missing-settings', 'changed', 'cursor-cycle', 'missing-known', 'malformed-id', 'foreign-provider', 'changed-listing'] as const) {
  test(`unaliased Desktop inventory is complete and read-only: ${mode}`, async () => {
    const root = await mkdtemp(join(tmpdir(), 'upgrade-inventory-'));
    try {
      await mkdir(join(root, 'sessions'));
      const path = join(root, 'sessions', fresh + '.jsonl');
      const source = [{ type: 'session_meta', payload: { id: fresh } },
        ...(mode === 'missing-settings' ? [] : [{ type: 'turn_context', payload: { cwd: root, model: 'test-native-model', effort: 'high',
          collaboration_mode: { mode: 'plan' }, approval_policy: 'never', sandbox_policy: { type: 'danger-full-access' } } }]),
        { type: 'event_msg', payload: { type: mode === 'live' ? 'task_started' : 'task_complete', turn_id: 'turn' } }].map(x => JSON.stringify(x)).join('\n') + '\n';
      await writeFile(path, source);
      const header = { id: fresh, cwd: root, path, createdAt: 10, updatedAt: 20, modelProvider: mode === 'foreign-provider' ? 'custom' : 'openai', turns: [] };
      const calls: string[] = [];
      const client = { async request(method: string, params: Record<string, any>) {
        calls.push(method);
        if (method === 'thread/list') {
          expect(params.limit).toBe(32);
          if (mode === 'cursor-cycle' && params.cursor) return { data: [], nextCursor: 'same' };
          return { data: [...(!params.archived && mode !== 'missing-known' ? [{ id: old }] : []),
            ...(params.archived === (mode === 'archived') ? [{ ...header, id: mode === 'malformed-id' ? '-'.repeat(36) : fresh, updatedAt: mode === 'changed-listing' ? 99 : 20 }] : [])],
            nextCursor: mode === 'cursor-cycle' ? 'same' : null };
        }
        if (method === 'thread/read') { expect(params.threadId).toBe(fresh); expect(params.includeTurns).toBe(false); return { thread: header }; }
        throw Error('Unexpected mutation');
      } };
      let owners = 0;
      const result = readMissingDesktopUpgradeHeaders(client, root, new Set([old]), async id => {
        expect(id).toBe(fresh);
        if (++owners === 2 && mode === 'changed') await appendFile(path, JSON.stringify({ type: 'event_msg', payload: { type: 'task_started', turn_id: 'next' } }) + '\n');
        return mode === 'missing-owner' ? null : mode !== 'dormant';
      });
      if (mode === 'settled' || mode === 'dormant' || mode === 'archived') {
        const rows = await result;
        expect(rows).toHaveLength(1);
        expect(rows[0]).toMatchObject({ id: fresh, androidRemoteModel: 'test-native-model', androidRemoteInteractionMode: 'plan', androidRemoteNativeEffort: 'high', androidRemoteRuntimeMode: 'full-access' });
        expect(rows[0]?.androidRemoteArchivedAt !== null).toBe(mode === 'archived');
        expect(await readFile(path, 'utf8')).toBe(source);
      } else await expect(result).rejects.toThrow();
      expect(calls.every(method => ['thread/list', 'thread/read'].includes(method))).toBe(true);
    } finally { await rm(root, { recursive: true, force: true }); }
  });
}

test('export-only aliases preserve native IDs, projectless state and actual selections', () => {
  const header = { id: fresh, cwd: '/workspace', modelProvider: 'openai', androidRemoteModel: 'native-model', androidRemoteNativeEffort: 'high' };
  const thread = { id: fresh, projectId: 'codex-project-chats', title: 'Original', modelSelection: { model: 'native-model' },
    runtimeMode: 'approval-required', interactionMode: 'plan', createdAt: '2026-01-01T00:00:00Z', updatedAt: '2026-01-02T00:00:00Z', archivedAt: null };
  const before = JSON.stringify(header);
  const { aliases } = desktopUpgradeAliases([header], { threads: [thread], projects: [] });
  expect(aliases[0]).toMatchObject({ remoteThreadId: fresh, nativeThreadId: fresh, workspaceKind: 'projectless', externalCodex: true, model: 'native-model', nativeReasoningEffort: 'high' });
  expect(annotateUpgradeAlias({ id: fresh }, aliases[0]!)).toMatchObject({ androidRemoteModel: 'native-model', androidRemoteProjectId: 'codex-project-chats' });
  expect(JSON.stringify(header)).toBe(before);
  expect(() => desktopUpgradeAliases([header], { threads: [{ ...thread, modelSelection: { model: 'fallback' } }], projects: [] })).toThrow();
});
