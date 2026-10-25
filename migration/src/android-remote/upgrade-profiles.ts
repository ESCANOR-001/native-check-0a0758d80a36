/** Upgrade-only profile discovery. Absence is explicit; unreadable or broken
 * configured profiles must stop an upgrade rather than silently lose history. */
import { lstat, realpath, stat } from 'node:fs/promises';

export type UpgradeProfiles = { codex?: string; claude?: string };

export async function optionalUpgradeProfile(path: string, explicitlyConfigured: boolean): Promise<string | undefined> {
  try {
    await lstat(path);
  } catch (error) {
    if (!explicitlyConfigured && (error as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw error;
  }
  // A dangling link or a file at the profile location is a broken installation,
  // not an absent agent. Resolving it must not create a substitute directory.
  const resolved = await realpath(path);
  if (!(await stat(resolved)).isDirectory()) throw new Error('The configured agent profile is not a directory. The current bridge remains active.');
  return resolved;
}
