import { createDecipheriv, createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';

const key = process.env.REMODEX_DESKTOP_QA_SNAPSHOT_KEY;
if (!/^[a-f0-9]{64}$/.test(key ?? '')) throw new Error('The CI snapshot key is unavailable');
const archive = process.argv[3] ?? 'native-snapshot';
const snapshot = readFileSync(archive + '.enc');
const decrypt = createDecipheriv('aes-256-gcm', Buffer.from(key, 'hex'), snapshot.subarray(0, 12));
decrypt.setAuthTag(snapshot.subarray(12, 28));
const source = Buffer.concat([decrypt.update(snapshot.subarray(28)), decrypt.final()]);
const expected = readFileSync(archive + '.sha256', 'utf8').trim();
const actual = createHash('sha256').update(source).digest('hex');
if (actual !== expected) throw new Error('The native source snapshot digest does not match');
writeFileSync(process.argv[2], source, { mode: 0o600 });
console.log('Built source snapshot SHA-256', actual);
