import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
// @ts-expect-error npm modules run directly without a declaration build.
import { needsSourceBootstrap, bootstrapSource } from '../bin/native-bootstrap.mjs';

test('native starts never bootstrap over an existing migration or a retained source hold', async () => {
  const root = await mkdtemp(join(tmpdir(), 'native-bootstrap-'));
  const installation = join(root, 'native-installation');
  try {
    await expect(needsSourceBootstrap(installation)).rejects.toThrow('No existing');
    await writeFile(join(root, 'android-remote.json'), '{}');
    expect(await needsSourceBootstrap(installation)).toBe(true);
    await writeFile(join(root, 'native-upgrade-source-hold.json'), '{}');
    await expect(needsSourceBootstrap(installation)).rejects.toThrow('earlier upgrade');
    await mkdir(installation);
    // Malformed records are handed to native recovery, never treated as absent.
    for (const name of ['npm-source.json', 'npm-upgrade.sqlite']) {
      await writeFile(join(installation, name), 'incomplete');
      expect(await needsSourceBootstrap(installation)).toBe(false);
      await rm(join(installation, name));
    }
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('one-time package bootstrap retires its own source before returning to native startup', async () => {
  const root = await mkdtemp(join(tmpdir(), 'native-bootstrap-lifecycle-'));
  const packageRoot = join(root, 'package'), source = join(root, 'source');
  const installation = join(source, 'native-installation');
  const before = process.env.OPENCODEX_BUN_PATH;
  process.env.OPENCODEX_BUN_PATH = process.execPath;
  try {
    await mkdir(join(packageRoot, 'src/cli'), { recursive: true });
    await mkdir(source);
    await writeFile(join(source, 'android-remote.json'), '{}');
    await writeFile(join(packageRoot, 'src/cli/native-upgrade-source.ts'), `
      import { mkdir, writeFile, access } from 'node:fs/promises';
      import { join } from 'node:path';
      const source=process.env.OPENCODEX_HOME!;
      const installation=join(source,'native-installation');
      await mkdir(installation);
      await writeFile(join(source,'entry.json'),JSON.stringify({argv:process.argv.slice(2),connect:process.env.REMODEX_CONNECT_ONLY,bootstrap:process.env.OCX_NATIVE_UPGRADE_SOURCE}));
      await writeFile(join(installation,'npm-source.json'),'{}');
      while (!await access(join(source,'retire')).then(()=>true,()=>false)) await Bun.sleep(10);
      await writeFile(join(source,'source-exited'),'yes');
    `);
    let preparations = 0;
    await bootstrapSource(packageRoot, installation, async (actual: string) => {
      expect(actual).toBe(installation);
      preparations++;
      const entry = JSON.parse(await readFile(join(source, 'entry.json'), 'utf8'));
      expect(entry).toMatchObject({ connect: '1', bootstrap: '1' });
      expect(entry.argv[0]).toMatch(/^--ocx-internal-launch-proof=/);
      expect(entry.argv[1]).toBe('start');
      await writeFile(join(dirname(actual), 'retire'), 'yes');
    });
    expect(preparations).toBe(1);
    expect(await readFile(join(source, 'source-exited'), 'utf8')).toBe('yes');
    await bootstrapSource(packageRoot, installation, () => { throw new Error('Must not bootstrap twice'); });
  } finally {
    if (before === undefined) delete process.env.OPENCODEX_BUN_PATH;
    else process.env.OPENCODEX_BUN_PATH = before;
    await rm(root, { recursive: true, force: true });
  }
});
