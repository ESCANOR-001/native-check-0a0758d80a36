import { basename, isAbsolute } from 'node:path';
import { UPGRADE_BUSY } from './upgrade-export';
type Row = Record<string, any>;

/** The native registry and Android's mirror are separate stores. Export both,
 * with the same public IDs, instead of losing Desktop-only project labels. */
export function upgradeProjects(groups: readonly (readonly Row[])[], fallbackDate: string): Row[] {
  const projects = new Map<string, Row>();
  for (const group of groups) for (const row of group) {
    if (row.id === 'codex-project-chats') continue;
    const root = row.workspaceRoot ?? row.cwd;
    if (typeof row.id !== 'string' || !row.id || typeof root !== 'string' || !isAbsolute(root)) throw new Error(UPGRADE_BUSY);
    const old = projects.get(row.id);
    if (old && old.workspaceRoot !== root) throw new Error(UPGRADE_BUSY);
    const createdAt = row.createdAt ?? old?.createdAt ?? fallbackDate;
    projects.set(row.id, { ...old, ...row, workspaceRoot: root,
      title: row.title || old?.title || basename(root), createdAt,
      updatedAt: row.updatedAt ?? old?.updatedAt ?? createdAt });
    if (projects.size > 256) throw new Error(UPGRADE_BUSY);
  }
  return [...projects.values()];
}
