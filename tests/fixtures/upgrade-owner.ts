// Synthetic source owner for the cross-language lifecycle test. No agent is run.
import { mkdir, realpath, writeFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';
const [module, root, installation, mode] = process.argv.slice(2);
const { startUpgradeControl } = await import(pathToFileURL(module!).href);
const source = await realpath(root!);
let prepared: Promise<string> | undefined;
const control = await startUpgradeControl({ sourceRoot: source, installation,
  ...(mode ? {prepareConfiguration(){return prepared ??= (async()=>{
    for(const agent of ['codex','claude'])await mkdir(join(source,agent));
    const bytes=JSON.stringify({workspace:source,codex:{profile:join(source,'codex'),executable:mode==='bad-config'?join(source,'missing'):process.execPath,model:'test'},claude:{profile:join(source,'claude'),executable:process.execPath,model:'test'}});
    await writeFile(join(dirname(control.descriptor),'configuration.json'),bytes,{mode:0o600});
    return createHash('sha256').update(bytes).digest('hex');
  })()}} : {}),
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
