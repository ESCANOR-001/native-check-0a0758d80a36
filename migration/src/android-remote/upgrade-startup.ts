/** Source lifecycle used by the published CLI as well as isolated rehearsals.
 * Registration never exports or stops anything. Only the local authenticated
 * native launcher may request the fixed handoff for this installation. */
import { constants } from 'node:fs';
import { open, realpath } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { createHash } from 'node:crypto';
import { startUpgradeControl } from './upgrade-control';

export interface UpgradeRuntime {
  configuration: Record<string, unknown>;
  profiles: { codex: string; claude: string };
  bind: string;
  publicUrl: string;
  gui: string;
}
export interface UpgradeStartupOwner {
  sourceRoot: string;
  runtime(): Promise<UpgradeRuntime>;
  /** Must include the gateway, dashboard listener and process-owned cleanup. */
  export(directory: string, profiles: UpgradeRuntime['profiles']): Promise<{ receipt: string }>;
  published?(): void;
}

export async function startUpgradeSource(owner: UpgradeStartupOwner) {
  const sourceRoot = await realpath(owner.sourceRoot);
  let prepared: Promise<{ runtime: UpgradeRuntime; configuration: string; bytes: Buffer; hash: string }> | undefined;
  let configurationDirectory: string;
  const prepare = () => {
    if (prepared) return prepared;
    let created = false;
    prepared = (async () => {
      const runtime = await owner.runtime();
      const configuration = join(configurationDirectory, 'configuration.json');
      const bytes = Buffer.from(JSON.stringify(runtime.configuration));
      if (!bytes.length || bytes.length > 256 * 1024) throw new Error('Native upgrade configuration is unavailable');
      const file = await open(configuration, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL, 0o600);
      created = true;
      try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
      if (process.platform !== 'win32') {
        const parent = await open(dirname(configuration), 'r');
        try { await parent.sync(); } finally { await parent.close(); }
      }
      return { runtime, configuration, bytes, hash: createHash('sha256').update(bytes).digest('hex') };
    })().catch(error => {
      // A failed capability probe did not change the owner or publish any
      // configuration. Let a later explicit start retry (e.g. after sign-in).
      // Once a file was created, retain the failed attempt for recovery rather
      // than overwriting a possibly partial durable configuration.
      if (!created) prepared = undefined;
      throw error;
    });
    return prepared;
  };
  const control = await startUpgradeControl({
    sourceRoot, installation: join(sourceRoot, 'native-installation'),
    async prepareConfiguration() { return (await prepare()).hash; },
    async exportOwner(directory) {
      // Resolve capabilities before closing admission. A missing CLI, GUI or
      // unsupported connection must not retire a working source process.
      const { runtime, configuration } = await prepare();
      const current = await owner.runtime();
      if (JSON.stringify(current) !== JSON.stringify(runtime)) throw new Error('Source configuration changed before upgrade');
      const result = await owner.export(directory, runtime.profiles);
      return { version: 1, closed_owner: result.receipt, configuration,
        bind: runtime.bind, public_url: runtime.publicUrl, gui: runtime.gui };
    },
    onPublished: owner.published,
  });
  configurationDirectory = dirname(control.descriptor);
  return control;
}
