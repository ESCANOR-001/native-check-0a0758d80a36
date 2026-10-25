/** Private, source-owned upgrade rendezvous. No route is added to the public
 * dashboard. A package launcher can request only this owner's fixed export. */
import { randomBytes, randomUUID, timingSafeEqual } from 'node:crypto';
import { constants } from 'node:fs';
import { lstat, mkdir, open, realpath, unlink } from 'node:fs/promises';
import { isAbsolute, join, resolve } from 'node:path';

export interface UpgradeControlOptions {
  sourceRoot: string;
  installation: string;
  /** Called once. Must use the owner's admission/shutdown boundary, and return
   * only after its public listener and owned workers have stopped. */
  exportOwner(directory: string): Promise<{
    version: 1; closed_owner: string; configuration: string;
    bind: string; public_url: string; gui: string;
  }>;
  /** The CLI can exit only after its receipt and plan are durable and this
   * last listener is closed. Never called for an incomplete handoff. */
  onPublished?(): void;
  /** Stage only configuration; no admission drain or owner shutdown. */
  prepareConfiguration?(): Promise<string>;
}
const FAILURE = 'The source upgrade did not complete. Existing data and recovery files were retained.';
async function writeNew(path: string, value: unknown) {
  const bytes = Buffer.from(JSON.stringify(value));
  if (bytes.length > 32768) throw new Error(FAILURE);
  const file = await open(path, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL, 0o600);
  try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
  if (process.platform !== 'win32') {
    const parent = await open(resolve(path, '..'), 'r');
    try { await parent.sync(); } finally { await parent.close(); }
  }
}
export async function startUpgradeControl(options: UpgradeControlOptions) {
  if (!isAbsolute(options.sourceRoot) || !isAbsolute(options.installation)
    || resolve(options.sourceRoot) !== options.sourceRoot || resolve(options.installation) !== options.installation) throw new Error(FAILURE);
  const sourceRoot = await realpath(options.sourceRoot);
  const instance = randomUUID(), token = randomBytes(32).toString('hex');
  const directory = join(sourceRoot, `native-upgrade-${instance}`);
  await mkdir(directory, { mode: 0o700 });
  const planPath = join(directory, 'npm-plan.json');
  let phase: 'idle' | 'exporting' | 'ready' | 'failed' = 'idle';
  let flight: Promise<void> | undefined;
  let closed = false;
  const server = Bun.serve({
    hostname: '127.0.0.1', port: 0, maxRequestBodySize: 1024, idleTimeout: 10,
    async fetch(request) {
      const auth = Buffer.from(request.headers.get('authorization') ?? '');
      const expected = Buffer.from(`Bearer ${token}`);
      // Not a browser API: Origin, Fetch Metadata and redirects have no role in
      // local lifecycle control. Never echo credentials or callback errors.
      if (auth.length !== expected.length || !timingSafeEqual(auth, expected)
        || request.headers.has('origin') || request.headers.has('sec-fetch-site')
        || request.headers.get('x-remodex-upgrade-instance') !== instance) return new Response(null, { status: 403 });
      const url = new URL(request.url);
      if (url.pathname === '/configuration' && !url.search && !url.hash && request.method === 'GET' && options.prepareConfiguration) {
        if (phase !== 'idle' || closed) return new Response(null, { status: 409 });
        try {
          const configurationHash = await options.prepareConfiguration();
          return Response.json({ version: 1, instance, configurationHash }, { headers: { 'cache-control': 'no-store' } });
        } catch { return new Response(null, { status: 409 }); }
      }
      if (url.pathname !== '/upgrade' || url.search || url.hash
        || request.headers.get('transfer-encoding')
        || ![null, '0'].includes(request.headers.get('content-length'))) return new Response(null, { status: 400 });
      if (!['GET', 'POST'].includes(request.method)) return new Response(null, { status: 405 });
      if (closed) return new Response(null, { status: 503 });
      if (request.method === 'POST' && phase === 'idle') {
        if (options.prepareConfiguration) {
          try {
            if (request.headers.get('x-remodex-configuration-sha256') !== await options.prepareConfiguration()) return new Response(null, { status: 409 });
          } catch { return new Response(null, { status: 409 }); }
          if (closed || phase !== 'idle') return Response.json({ version: 1, instance, phase });
        }
        // Set synchronously before any await. Repeated requests join the same
        // durable attempt; they never replay shutdown or overwrite an export.
        phase = 'exporting';
        flight = (async () => {
          await writeNew(join(directory, 'attempt.json'), { version: 1, instance });
          const plan = await options.exportOwner(join(directory, 'export'));
          if (plan.version !== 1 || plan.closed_owner !== join(directory, 'export', 'closed-owner.json')) throw new Error(FAILURE);
          await writeNew(planPath, plan);
          phase = 'ready';
          // Completion is durable. The launcher can recover it even if this
          // process exits now; do not retain an idle TypeScript server after
          // its workers and public listener have already been released.
          await server.stop(true);
          closed = true;
          options.onPublished?.();
        })().catch(() => { phase = 'failed'; });
      }
      return Response.json({ version: 1, instance, phase }, { headers: { 'cache-control': 'no-store' } });
    },
  });
  const descriptor = join(directory, 'source.json');
  // The installed launcher needs only its persistent installation directory.
  // Never overwrite a previous registration: it may describe an interrupted
  // export, and PID/port absence is not proof that its workers stopped.
  const registration = join(options.installation, 'npm-source.json');
  let registered = false;
  let registrationIdentity: { dev: number; ino: number } | undefined;
  try {
    await mkdir(options.installation, { recursive: true, mode: 0o700 });
    const destination = await lstat(options.installation);
    if (!destination.isDirectory() || destination.isSymbolicLink()) throw new Error(FAILURE);
    // macOS's /var and Windows drive casing may differ from realpath. Persist
    // the canonical directory instead of comparing its textual spelling.
    const installation = await realpath(options.installation);
    await writeNew(descriptor, { version: 1, source_root: sourceRoot, installation,
      instance, endpoint: `http://127.0.0.1:${server.port}/upgrade`, token,
      ...(options.prepareConfiguration ? { preflight: true } : {}) });
    await writeNew(registration, { version: 1, descriptor });
    registrationIdentity = await lstat(registration);
    registered = true;
  } catch (error) { await server.stop(true); throw error; }
  return {
    descriptor,
    async close() {
      closed = true;
      await server.stop(true);
      // A caller disconnect cannot cancel the source's shutdown or truncate its
      // receipt. Keep the directory as recovery evidence, including failures.
      await flight;
      // Clean shutdown before any export may release discovery. Once offered,
      // retain it through failure, process exit and npm package replacement.
      if (registered && phase === 'idle') {
        const named = await lstat(registration).catch(() => null);
        if (named?.isFile() && !named.isSymbolicLink() && named.nlink === 1
          && named.dev === registrationIdentity?.dev && named.ino === registrationIdentity?.ino) {
          await unlink(registration);
        }
        registered = false;
      }
    },
  };
}
