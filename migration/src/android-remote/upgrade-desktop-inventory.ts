/** Upgrade inventory is complete or rejected. Sidebar pagination caps must not
 * silently remove Desktop chats which never acquired an Android alias. */
import { UPGRADE_BUSY } from './upgrade-export';
import { readDesktopExportHeader } from './upgrade-desktop-history';
import type { AndroidRemoteThreadAlias } from './store';
type Row = Record<string, any>;
type Client = { request(method: string, params: Row): Promise<any> };

export async function readMissingDesktopUpgradeHeaders(
  client: Client, profile: string, known: ReadonlySet<string>, ownerConnected: (id: string) => Promise<boolean | null>,
): Promise<Row[]> {
  const headers: Row[] = [], seen = new Set<string>();
  const deadline = Date.now() + 120_000;
  for (const archived of [false, true]) {
    let cursor: string | null = null;
    const cursors = new Set<string>();
    do {
      if (Date.now() >= deadline) throw new Error(UPGRADE_BUSY);
      const result = await client.request('thread/list', { limit: 32, cursor, archived, modelProviders: [], sortKey: 'created_at', sortDirection: 'asc' });
      if (!Array.isArray(result?.data) || result.data.length > 32) throw new Error(UPGRADE_BUSY);
      for (const row of result.data) {
        const id = row?.id;
        if (typeof id !== 'string' || !/^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(id) || seen.has(id)) throw new Error(UPGRADE_BUSY);
        seen.add(id);
        if (seen.size > 1024) throw new Error('This installation exceeds the bounded automatic chat inventory; all source chats are retained.');
        if (known.has(id)) continue;
        const header = await readDesktopExportHeader(client, id, profile, () => ownerConnected(id));
        if (header.cwd !== row.cwd || header.updatedAt !== row.updatedAt) throw new Error(UPGRADE_BUSY);
        if (!Number.isFinite(header.createdAt) || !Number.isFinite(header.updatedAt)) throw new Error(UPGRADE_BUSY);
        headers.push({ ...header, androidRemoteArchivedAt: archived ? new Date(header.updatedAt * 1000).toISOString() : null });
      }
      const next = result.nextCursor;
      if (next == null) cursor = null;
      else {
        if (typeof next !== 'string' || !next || next.length > 8192 || cursors.has(next) || !result.data.length) throw new Error(UPGRADE_BUSY);
        cursors.add(next); cursor = next;
      }
    } while (cursor);
  }
  for (const id of known) if (!seen.has(id)) throw new Error(UPGRADE_BUSY);
  return headers;
}

/** Produce export-only aliases. Never write discoveries into the source bridge,
 * or replace settings on a Desktop-owned task to manufacture an Android ID. */
export function desktopUpgradeAliases(headers: Row[], shell: Row): {
  aliases: (AndroidRemoteThreadAlias & { externalCodex: true; archivedAt: string | null })[];
  projects: Row[];
} {
  const threads = shell.threads as Row[];
  if (!Array.isArray(threads) || threads.length !== headers.length || !Array.isArray(shell.projects)) throw new Error(UPGRADE_BUSY);
  const aliases = headers.map((header, index) => {
    const thread = threads[index]!;
    if (!header.androidRemoteModel || thread.id !== header.id
      || thread.modelSelection?.model !== header.androidRemoteModel) throw new Error(UPGRADE_BUSY);
    return {
      remoteThreadId: header.id, nativeThreadId: header.id, projectId: thread.projectId,
      ...(thread.projectId === 'codex-project-chats' ? { workspaceKind: 'projectless' as const } : {}),
      title: thread.title, cwd: header.cwd, instanceId: 'openai', nativeModelProvider: header.modelProvider,
      model: header.androidRemoteModel, nativeReasoningEffort: header.androidRemoteNativeEffort,
      nativeServiceTier: header.androidRemoteNativeServiceTier,
      runtimeMode: thread.runtimeMode, interactionMode: thread.interactionMode,
      createdAt: thread.createdAt, updatedAt: thread.updatedAt, archivedAt: thread.archivedAt,
      externalCodex: true as const,
    };
  });
  return { aliases, projects: shell.projects as Row[] };
}

export function annotateUpgradeAlias(native: Row, alias: AndroidRemoteThreadAlias): Row {
  return { ...native, id: alias.remoteThreadId, androidRemoteNativeThreadId: alias.nativeThreadId,
    androidRemoteProjectId: alias.projectId, androidRemoteTitle: alias.title,
    androidRemoteWorkspaceKind: alias.workspaceKind,
    androidRemoteProviderInstanceId: alias.instanceId, androidRemoteModel: alias.model,
    androidRemoteRuntimeMode: alias.runtimeMode, androidRemoteInteractionMode: alias.interactionMode };
}
