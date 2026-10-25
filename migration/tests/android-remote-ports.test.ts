import { expect, test } from 'bun:test';
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';

test('alternate gateway ports isolate rehearsals and invalid values cannot silently select a normal port', () => {
  const source = resolve(import.meta.dir, '../src/android-remote/ports.ts');
  for (const value of [undefined, '19051', '0', '65536', '10105;bad', '']) {
    const env = { ...process.env };
    delete env.REMODEX_ANDROID_GATEWAY_PORT;
    delete env.REMODEX_CODEX_APP_SERVER_PORT;
    if (value !== undefined) env.REMODEX_ANDROID_GATEWAY_PORT = value;
    const result = spawnSync(process.execPath, ['-e', `import { DEFAULT_ANDROID_GATEWAY_PORT } from ${JSON.stringify(source)}; console.log(DEFAULT_ANDROID_GATEWAY_PORT);`], {
      env, encoding: 'utf8', windowsHide: true, shell: false, timeout: 5000,
    });
    if (value === undefined || value === '19051') {
      expect(result.status).toBe(0);
      expect(result.stdout.trim()).toBe(value ?? '10105');
    } else expect(result.status).not.toBe(0);
  }
});
