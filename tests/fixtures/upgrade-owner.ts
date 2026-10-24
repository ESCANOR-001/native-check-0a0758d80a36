// Synthetic source owner for the cross-language lifecycle test. No agent is run.
import { mkdir, realpath, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const [module, root, installation] = process.argv.slice(2);
const { startUpgradeControl } = await import(pathToFileURL(module!).href);
const source = await realpath(root!);
const control = await startUpgradeControl({ sourceRoot: source, installation,
  async exportOwner(directory: string) {
    await mkdir(directory, { mode: 0o700 });
    const closed_owner = join(directory, 'closed-owner.json');
    await writeFile(closed_owner, JSON.stringify({ profile_mode: 'existing', source_hold: join(source, 'native-upgrade-source-hold.json') }), { mode: 0o600 });
    return { version: 1, closed_owner, configuration: join(source, 'configuration.json'),
      bind: '127.0.0.1:1234', public_url: 'http://127.0.0.1:1234', gui: source };
  },
});
console.log(control.descriptor);
// The control releases its last listener after publishing. A normal successful
// run must exit naturally; no forced process kill is part of this success test.
