import {readFile} from 'node:fs/promises';
import {join} from 'node:path';
const [binary,connector,directory]=process.argv.slice(2);
const server=Bun.serve({hostname:'127.0.0.1',port:0,fetch(){return new Response('fixture')}});
const child=Bun.spawn([binary!,'--legacy-quick-launch',connector!,`127.0.0.1:${server.port}`,'http2',directory!],{windowsHide:true,detached:true,stdin:'ignore',stdout:'pipe',stderr:'pipe'});
const [code,raw,error]=await Promise.all([child.exited,new Response(child.stdout).text(),new Response(child.stderr).text()]);
if(code!==0)throw Error(error);
const record=join(directory!,'process.json');
if(JSON.stringify(JSON.parse(await readFile(record,'utf8')))!==JSON.stringify(JSON.parse(raw)))throw Error('Launcher did not persist its process record');
// The cheap native watcher belongs to the old source, unlike the connector.
Bun.spawn([binary!,'--legacy-quick-control','wait',record],{windowsHide:true,stdin:'ignore',stdout:'ignore',stderr:'ignore'});
await server.stop(true);
console.log(raw.trim());process.exit(0);
