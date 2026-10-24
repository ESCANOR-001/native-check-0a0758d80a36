import { readdirSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

const directory = 'target/debug/deps';
const bins = readdirSync(directory).filter(name => /^legacy_claude-[a-f0-9]+(?:\.exe)?$/.test(name) && statSync(join(directory, name)).isFile());
if (bins.length !== 1) throw new Error('Expected one built legacy Claude test executable');
const binary = join(directory, bins[0]);
const test = 'first_native_event_after_upgrade_advances_anchor_and_preserves_old_prompt_bubbles';
for (let iteration = 1; iteration <= 500; iteration++) {
  await new Promise((resolve, reject) => {
    let text = '', timedOut = false;
    const child = spawn(binary, ['--exact', test, '--nocapture'], { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    const append = chunk => { text = (text + chunk.toString()).slice(-32768); };
    child.stdout.on('data', append); child.stderr.on('data', append);
    const timer = setTimeout(() => {
      timedOut = true;
      if (process.platform === 'darwin') {
        const sample = spawnSync('/usr/bin/sample', [String(child.pid), '2', '-file', `legacy-hang-${iteration}.txt`], { windowsHide: true, timeout: 5000, maxBuffer: 32768 });
        append(sample.stdout ?? ''); append(sample.stderr ?? '');
      }
      writeFileSync(`legacy-hang-${iteration}.log`, text);
      child.kill('SIGKILL');
    }, 30000);
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.on('close', code => {
      clearTimeout(timer);
      if (code !== 0 || timedOut) reject(new Error(`Projection iteration ${iteration} failed (timeout=${timedOut}): ${text}`));
      else resolve();
    });
  });
}
console.log('500 bounded legacy projection/replay checks passed');
