/** Bounded, explicit file inventory for a closed-owner export. No URL fetching
 * or recursive directory scanning is allowed here. */
import { claudeNativeUploads } from './upgrade-native-assets';
import { createHash } from 'node:crypto';
import { mkdir, open } from 'node:fs/promises';
import { isAbsolute, join, relative, resolve, sep } from 'node:path';
type Row = Record<string, any>;
export interface AssetProof { root: string; path: string; sha256: string; bytes: number }
type Copy = (root: string, path: string, destination: string | undefined, limit: number) => Promise<{ relative: string; sha256: string; bytes: number }>;
const INVALID = 'An attachment is missing or cannot be verified. Original history was retained; export was not activated.';
function local(root: string, path: unknown): string {
  if (typeof path !== 'string' || path.length > 4096 || !isAbsolute(path) || resolve(path) !== path || /[\x00-\x1f]/.test(path)) throw new Error(INVALID);
  const name = relative(root, path);
  if (!name || isAbsolute(name) || name === '..' || name.startsWith('..' + sep)) throw new Error(INVALID);
  return name.split(sep).join('/');
}
async function manifest(path: string, value: unknown) {
  const bytes = Buffer.from(JSON.stringify(value));
  if (bytes.length > 2 * 1024 * 1024) throw new Error(INVALID);
  const file = await open(path, 'wx', 0o600);
  try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
}
export async function exportChatAssets(
  target: string, index: number, chat: { id: string; agent: string; history: Row; transcript?: string },
  roots: { uploads: string; images: string; claudeUploads?: string } | undefined, copy: Copy,
  charge: (bytes: number) => void,
): Promise<{ uploads?: Row; images?: Row; proofs: AssetProof[] }> {
  const thread = chat.agent === 'claude' ? chat.history.thread : chat.history;
  const uploadRoot = roots && (chat.agent === 'claude' ? roots.claudeUploads ?? roots.uploads : roots.uploads);
  const uploads = new Map<string, string>(), images = new Set<string>();
  for (const message of thread.messages ?? []) {
    if (message.attachments != null && !Array.isArray(message.attachments)) throw new Error(INVALID);
    for (const attachment of message.attachments ?? []) {
      if (!roots) throw new Error(INVALID);
      local(uploadRoot!, attachment.path);
      if (!uploads.has(attachment.path)) uploads.set(attachment.path, message.id);
    }
    const text: string = typeof message.text === 'string' ? message.text : '';
    const structured = new Set<string>();
    if (chat.agent === 'codex' && message.role === 'user') {
      for (const line of text.split('\n')) {
        const image = /^\[Image: ([^\]\r\n]+)\]$/.exec(line);
        if (image) structured.add(image[1]!);
      }
      const marker = '\n\nFiles uploaded from Android and staged on this PC:\n';
      const offset = text.lastIndexOf(marker);
      if (offset >= 0) {
        const lines = text.slice(offset + marker.length).split('\n');
        if (lines.every(line => line.startsWith('- ') || /^\[Image: [^\]\r\n]+\]$/.test(line))) {
          for (const line of lines) if (line.startsWith('- ')) structured.add(line.slice(2));
        }
      }
      for (const path of structured) {
        if (!path.includes('android-remote-files' + sep)) continue;
        if (!roots) throw new Error(INVALID);
        local(uploadRoot!, path);
        if (!uploads.has(path)) uploads.set(path, message.id);
      }
    }
    for (const match of text.matchAll(/\]\((?:<([^>\r\n]+)>|([^()\r\n]+))\)/g)) {
      const path = match[1] ?? match[2]!;
      if (!path.includes('android-remote-files' + sep) && !(roots && isAbsolute(path) && !relative(uploadRoot!, path).startsWith('..'))) continue;
      if (!roots) throw new Error(INVALID);
      local(uploadRoot!, path);
      if (!uploads.has(path)) uploads.set(path, message.id);
    }
    let remainder = text;
    for (const path of uploads.keys()) remainder = remainder.replaceAll(`](<${path}>)`, '](retained)').replaceAll(`](${path})`, '](retained)');
    remainder = remainder.split('\n').map(line => {
      for (const path of structured) if (uploads.has(path) && (line === `- ${path}` || line === `[Image: ${path}]`)) return '(retained)';
      return line;
    }).join('\n');
    if (remainder.includes('android-remote-files/') || remainder.includes('android-remote-files\\')) throw new Error(INVALID);
  }
  for (const activity of thread.activities ?? []) {
    const paths = activity.payload?.data?.toolImages;
    if (paths == null) continue;
    if (!Array.isArray(paths) || paths.length > 8) throw new Error(INVALID);
    for (const path of paths) {
      if (!roots || chat.agent !== 'claude') throw new Error(INVALID);
      local(roots.images, path); images.add(path);
    }
  }
  if (roots && chat.agent === 'claude' && chat.transcript && chat.history.nativeId) {
    for (const file of await claudeNativeUploads(chat.transcript, chat.history, uploadRoot!, (root, path) => copy(root, path, undefined, 10 * 1024 * 1024))) {
      if (!uploads.has(file.path)) uploads.set(file.path, file.message);
    }
  }
  if (uploads.size > 2048 || images.size > 2048) throw new Error(INVALID);
  const result: { uploads?: Row; images?: Row; proofs: AssetProof[] } = { proofs: [] };
  if (uploads.size) {
    const directory = `uploads-${index}`, files: Row[] = [];
    await mkdir(join(target, directory), { mode: 0o700 });
    for (const [path, message] of uploads) {
      const name = local(uploadRoot!, path), parts = name.split('/');
      if (parts.length !== 3 || !/^[a-zA-Z0-9._-]{1,256}$/.test(parts[0]!) || !/^[0-9a-f-]{36}$/i.test(parts[1]!) || !/^[a-zA-Z0-9._ -]{1,256}$/.test(parts[2]!) || typeof message !== 'string' || !message) throw new Error(INVALID);
      const destination = join(target, directory, ...parts);
      await mkdir(resolve(destination, '..'), { mode: 0o700, recursive: true });
      const proof = await copy(uploadRoot!, path, destination, 10 * 1024 * 1024);
      charge(proof.bytes);
      files.push({ message, original: path, relative: name, sha256: proof.sha256, bytes: proof.bytes });
      result.proofs.push({ root: uploadRoot!, path, ...proof });
    }
    const name = `uploads-${index}.json`;
    await manifest(join(target, name), { version: 1, files });
    result.uploads = { original_root: uploadRoot!, export: directory, manifest: name };
  }
  if (images.size) {
    const directory = `images-${index}`, bucket = createHash('sha256').update(chat.id).digest('hex');
    await mkdir(join(target, directory, bucket), { mode: 0o700, recursive: true });
    for (const path of images) {
      const name = local(roots!.images, path);
      if (!new RegExp(`^${bucket}/[a-f0-9]{64}\\.(png|jpg|gif|webp)$`).test(name)) throw new Error(INVALID);
      const proof = await copy(roots!.images, path, join(target, directory, name), 16 * 1024 * 1024);
      if (proof.sha256 !== name.slice(bucket.length + 1, bucket.length + 65)) throw new Error(INVALID);
      charge(proof.bytes);
      result.proofs.push({ root: roots!.images, path, ...proof });
    }
    result.images = { original_root: roots!.images, export: directory };
  }
  return result;
}
