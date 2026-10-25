import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, readFile, rm, realpath } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
// @ts-expect-error Node npm entry has no separate declaration build.
import { installationDirectory, packageArguments, tryNativeConnect } from '../bin/native-connect.mjs';

test('published bin keeps the Connect state location and literal paths', () => {
  const home=resolve(tmpdir(),'native home');
  expect(installationDirectory({},home)).toBe(join(home,'.remodex-connect','native-installation'));
  expect(installationDirectory({OPENCODEX_HOME:'~/custom $(literal)'},home)).toBe(join(home,'custom $(literal)','native-installation'));
  const root=join(home,'native-installation');
  expect(packageArguments(['start'],root)).toEqual(['start',root]);
  expect(packageArguments(['start','--port','10100'],root)).toEqual(['--npm-upgrade','start',root,'10100']);
  expect(() => packageArguments(['start','--port','0'],root)).toThrow();
  expect(() => packageArguments(['start','--port','65536'],root)).toThrow();
  expect(packageArguments(['status'],root)).toEqual(['--npm-upgrade','status',root]);
  expect(packageArguments(['--version'],root)).toEqual(['--version']);
  expect(() => packageArguments(['start','foreign'],root)).toThrow();
  expect(packageArguments(['restart'],root)).toEqual(['--npm-upgrade','restart',root]);
  expect(packageArguments(['stop'],root)).toEqual(['--npm-upgrade','stop',root]);
  expect(() => packageArguments(['stop','foreign'],root)).toThrow();
});

test('existing Bun service entry forwards its port without starting the legacy server', async () => {
  const root=await realpath(await mkdtemp(join(tmpdir(),'native-service-entry-')));
  try {
    await mkdir(join(root,'src/cli'),{recursive:true});await mkdir(join(root,'bin'));
    await writeFile(join(root,'src/cli/connect.ts'),await readFile(resolve(import.meta.dir,'../src/cli/connect.ts')));
    await writeFile(join(root,'src/cli/index.ts'),"throw new Error('Legacy server must not start after native selection');");
    await writeFile(join(root,'bin/native-connect.mjs'),"export async function tryNativeConnect(root,args) { console.log(JSON.stringify({root,args})); return true; }");
    const child=spawnSync(process.execPath,[join(root,'src/cli/connect.ts'),'--ocx-internal-launch-proof=fixture','start','--port','19043'],{encoding:'utf8',windowsHide:true,shell:false,timeout:5000});
    expect(child.status).toBe(0);
    expect(JSON.parse(child.stdout)).toEqual({root,args:['start','--port','19043']});
  } finally {await rm(root,{recursive:true,force:true});}
});

test('native package selection is explicit, version-bound, and cannot fall back on failure', async () => {
  const root=await mkdtemp(join(tmpdir(),'native-entry-'));
  try {
    expect(await tryNativeConnect(root,['start'])).toBe(false);
    await mkdir(join(root,'bin/native'),{recursive:true});
    await writeFile(join(root,'package.json'),JSON.stringify({version:'0.0.0'}));
    const manifest=join(root,'bin/native-runtime.json');
    await writeFile(manifest,JSON.stringify({formatVersion:1,version:'wrong',backend:'rust',migrationComplete:false}));
    await expect(tryNativeConnect(root,['start'])).rejects.toThrow('does not match');
    await writeFile(manifest,JSON.stringify({formatVersion:1,version:'0.0.0',backend:'rust',migrationComplete:false}));
    await writeFile(join(root,'bin/native/launcher.mjs'),"export const VERSION='0.0.0'; export async function main() { throw new Error('native refused'); }");
    await expect(tryNativeConnect(root,['version'])).rejects.toThrow('native refused');
  } finally {await rm(root,{recursive:true,force:true});}
});
