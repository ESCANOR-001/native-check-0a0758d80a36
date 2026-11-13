import { writeFileSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
import { freemem, release } from 'node:os';

// Keep the original 30-second assertion. Repeated runs and bounded progress
// evidence distinguish a stalled projection from a slow fixture preparation.
const built = spawnSync('cargo', ['+1.98.0', 'test', '--test', 'chat', '--no-run', '--message-format=json'], {
  windowsHide: true, encoding: 'utf8', timeout: 180000, maxBuffer: 4 * 1024 * 1024,
});
if (built.status !== 0) throw new Error(`Cannot identify current chat test artifact: ${built.stderr}`);
const artifacts = built.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line))
  .filter(value => value.reason === 'compiler-artifact' && value.target?.name === 'chat' && value.executable);
if (artifacts.length !== 1) throw new Error('Expected one current chat test executable');
const test = 'prompt_edit_preserves_a_revision_and_retries_without_reediting';
for (let iteration = 1; iteration <= 10; iteration++) {
  const started = Date.now();
  await new Promise((resolve, reject) => {
    let output = '', timedOut = false;
    const child = spawn(artifacts[0].executable, ['--exact', test, '--nocapture'], {
      windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
    });
    const append = chunk => { output = (output + chunk.toString()).slice(-65536); };
    child.stdout.on('data', append);
    child.stderr.on('data', append);
    const timer = setTimeout(() => {
      timedOut = true;
      if (process.platform === 'win32') {
        spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], {
          windowsHide: true, timeout: 10000, maxBuffer: 8192,
        });
      }
      child.kill('SIGKILL');
    }, 90000);
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.on('close', code => {
      clearTimeout(timer);
      if (code === 0 && !timedOut) { resolve(); return; }
      writeFileSync(`history-preparation-${iteration}.log`, output);
      writeFileSync(`history-preparation-${iteration}.json`, JSON.stringify({
        iteration, code, timedOut, elapsedMs: Date.now() - started,
        platform: process.platform, release: release(), availableBytes: freemem(),
      }, null, 2));
      reject(new Error(`Prompt-edit preparation failed on iteration ${iteration}: ${output}`));
    });
  });
  console.log(`Prompt-edit history check ${iteration}/10 passed in ${Date.now() - started}ms`);
}
