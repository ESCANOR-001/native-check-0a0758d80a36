import { createReadStream } from 'node:fs';
import { chmod, copyFile, cp, mkdir, lstat, readFile, realpath, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { platformKey, VERSION } from './launcher.mjs';

export async function buildPackages(binary, destination) {
  const target = platformKey();
  const source = resolve(binary), root = resolve(destination);
  const stat = await lstat(source);
  if (!stat.isFile() || stat.isSymbolicLink() || stat.size <= 0 || stat.size > 128 * 1024 * 1024) throw new Error('Invalid release executable');
  await mkdir(root); // Never overwrite an earlier package or install tree.
  const native = join(root, 'native'), launcher = join(root, 'launcher');
  await mkdir(native); await mkdir(launcher); await mkdir(join(launcher, 'bin'));
  const name = `@remodex/native-connect-experiment-${target}`;
  const executable = target.startsWith('win32-') ? 'remodex-connect-native.exe' : 'remodex-connect-native';
  const path = join(native, executable);
  await copyFile(source, path); await chmod(path, 0o755);
  const hash = createHash('sha256');
  for await (const b of createReadStream(path, { highWaterMark: 64 * 1024 })) hash.update(b);
  const manifest = { formatVersion: 1, version: VERSION, target, migrationComplete: false, bytes: stat.size, sha256: hash.digest('hex') };
  if (target.endsWith('-gnu')) manifest.minimumGlibc = process.report.getReport().header.glibcVersionRuntime; // Conservative tested build baseline.
  await writeFile(join(native, 'native.json'), JSON.stringify(manifest, null, 2));
  await writeFile(join(native, 'package.json'), JSON.stringify({ name, version: VERSION, private: true, os: [process.platform], cpu: [process.arch], files: [executable, 'native.json'], exports: { './package.json': './package.json' } }, null, 2));
  const optionalDependencies = Object.fromEntries(['linux-x64-gnu', 'win32-x64', 'darwin-arm64'].map(t => [`@remodex/native-connect-experiment-${t}`, VERSION]));
  await writeFile(join(launcher, 'package.json'), JSON.stringify({ name: '@remodex/native-connect-experiment', version: VERSION, private: true, type: 'module', bin: { 'rmx-native-experiment': 'bin/launcher.mjs' }, files: ['bin'], engines: { node: '>=20.9.0' }, optionalDependencies }, null, 2));
  await copyFile(join(dirname(fileURLToPath(import.meta.url)), 'launcher.mjs'), join(launcher, 'bin/launcher.mjs'));
  await chmod(join(launcher, 'bin/launcher.mjs'), 0o755);
  return { native, launcher, target, name };
}
// Rehearse the actual published bin aliases without publishing or installing
// globally. Keep compatibility source files for inspection/recovery, but native
// selection cannot fall back to them after its durable handoff.
export async function buildPublishedPackage(runtimeRoot, packages) {
  const root=resolve(runtimeRoot), destination=join(dirname(packages.launcher),'published');
  await mkdir(destination);
  for (const name of ['bin','src']) await cp(join(root,name),join(destination,name),{recursive:true,errorOnExist:true,force:false});
  const pkg=JSON.parse(await readFile(join(root,'package.json'),'utf8'));
  await mkdir(join(destination,'bin/native'));
  await copyFile(join(dirname(fileURLToPath(import.meta.url)),'launcher.mjs'),join(destination,'bin/native/launcher.mjs'));
  await writeFile(join(destination,'bin/native-runtime.json'),JSON.stringify({formatVersion:1,version:VERSION,backend:'rust',migrationComplete:false}));
  await writeFile(join(destination,'package.json'),JSON.stringify({name:pkg.name,version:VERSION,private:true,type:'module',bin:pkg.bin,files:['bin','src'],engines:pkg.engines,
    optionalDependencies:Object.fromEntries(['linux-x64-gnu','win32-x64','darwin-arm64'].map(t=>[`@remodex/native-connect-experiment-${t}`,VERSION]))},null,2));
  return destination;
}
if (process.argv[1] && await realpath(process.argv[1]).catch(() => '') === await realpath(fileURLToPath(import.meta.url))) {
  if (process.argv.length !== 4) throw new Error('Usage: node build.mjs RELEASE_BINARY NEW_OUTPUT_DIRECTORY');
  console.log(JSON.stringify(await buildPackages(process.argv[2], process.argv[3])));
}
