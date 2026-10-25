/** Desktop remains the writer. Retain its original rollout, validating the
 * immutable session identity rather than requiring a running turn to stop. */
import { createHash } from 'node:crypto';
import { constants } from 'node:fs';
import { open, realpath } from 'node:fs/promises';
import { isAbsolute, relative, resolve, sep } from 'node:path';

const ERROR = 'The original Desktop chat identity could not be verified; no chat was moved.';
const LIMIT = 8 * 1024 * 1024;
export async function desktopTranscriptReference(profile: string, path: string, native: string) {
  if (!/^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(native)
    || !isAbsolute(path) || resolve(path) !== path) throw new Error(ERROR);
  const root = await realpath(profile), local = relative(resolve(profile), path);
  const parts = local.split(sep);
  if (!['sessions', 'archived_sessions'].includes(parts[0]!) || parts.length < 2
    || parts.some(p => !p || p === '.' || p === '..') || isAbsolute(local)) throw new Error(ERROR);
  let checked = root;
  for (const part of parts) {
    checked = resolve(checked, part);
    if (await realpath(checked) !== checked) throw new Error(ERROR);
  }
  const file = await open(checked, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  try {
    const meta = await file.stat();
    if (!meta.isFile() || meta.nlink !== 1) throw new Error(ERROR);
    const chunks: Buffer[] = [];
    let size = 0;
    for (;;) {
      const chunk = Buffer.alloc(Math.min(65536, LIMIT + 1 - size));
      const { bytesRead } = await file.read(chunk);
      if (!bytesRead) throw new Error(ERROR);
      const end = chunk.subarray(0, bytesRead).indexOf(10);
      const bytes = chunk.subarray(0, end < 0 ? bytesRead : end + 1);
      size += bytes.length; chunks.push(bytes);
      if (size > LIMIT) throw new Error(ERROR);
      if (end < 0) continue;
      const line = Buffer.concat(chunks, size), row = JSON.parse(line.toString('utf8'));
      if (row.type !== 'session_meta' || row.payload?.id !== native) throw new Error(ERROR);
      const named = await open(checked, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
      try {
        const now = await named.stat();
        if (now.ino !== meta.ino || now.dev !== meta.dev || now.size < size) throw new Error(ERROR);
      } finally { await named.close(); }
      return { relative: parts.join('/'), sha256: createHash('sha256').update(line).digest('hex'), bytes: size };
    }
  } finally { await file.close(); }
}
