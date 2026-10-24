import { writeFileSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';

// Ask Cargo for the exact current artifact; restored caches can contain stale
// hashes and must never choose which executable gets qualified.
const built = spawnSync('cargo', ['+1.98.0', 'test', '--test', 'legacy_claude', '--no-run', '--message-format=json'], { windowsHide: true, encoding: 'utf8', timeout: 180000, maxBuffer: 4 * 1024 * 1024 });
if (built.status !== 0) throw new Error(`Cannot identify current legacy test artifact: ${built.stderr}`);
const artifacts = built.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line)).filter(v => v.reason === 'compiler-artifact' && v.target?.name === 'legacy_claude' && v.executable);
if (artifacts.length !== 1) throw new Error('Expected one current legacy Claude test executable');
const binary = artifacts[0].executable;
const test = 'first_native_event_after_upgrade_advances_anchor_and_preserves_old_prompt_bubbles';
console.log('Qualified current pinned-toolchain artifact; starting 500 bounded runs');
const started = Date.now();
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
  if (iteration % 50 === 0) console.log(`${iteration}/500 passed in ${Math.round((Date.now()-started)/1000)}s`);
}
console.log('500 bounded legacy projection/replay checks passed');
