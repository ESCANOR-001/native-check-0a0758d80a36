import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, readFile, rm, appendFile, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { desktopTranscriptReference } from '../src/android-remote/upgrade-desktop-reference';
import { exportOwnedUpgrade } from '../src/android-remote/upgrade-export';
const native = '11111111-1111-4111-8111-111111111111';
for (const race of ['append', 'identity', 'truncate'] as const) {
  test(`Desktop reference handoff retains native history: ${race}`, async () => {
    const root = await mkdtemp(join(tmpdir(), 'desktop-reference-'));
    try {
      const codex = join(root,'codex'), claude=join(root,'claude'), path=join(codex,'sessions','test.jsonl');
      await mkdir(join(codex,'sessions'),{recursive:true}); await mkdir(claude);
      const header=JSON.stringify({type:'session_meta',payload:{id:native}})+'\n';
      const original=header+JSON.stringify({type:'event_msg',payload:{type:'task_started',turn_id:'still-running'}})+'\n';
      await writeFile(path,original);
      const promise=exportOwnedUpgrade(join(root,'export'),{codex,claude},{
        environment:'fixture',connection:{clients:[]},projects:[],claudeSettings:{},assertSettled(){},release(){},
        chats:(async function*(){yield {agent:'codex' as const,id:'public',transcript:path,desktopReference:true,history:{nativeId:native}}})(),
        async stop(){
          if(race==='append')await appendFile(path,JSON.stringify({type:'event_msg',payload:{type:'task_complete',turn_id:'still-running'}})+'\n');
          else await writeFile(path,race==='truncate'?'':header.replace(native,'22222222-2222-4222-8222-222222222222'));
        },
      });
      if(race==='append'){
        const result=await promise;
        const receipt=JSON.parse(await readFile(result.receipt,'utf8'));
        expect(receipt.chats[0].desktopReference).toBe(true);
        const ref=JSON.parse(await readFile(join(root,'export','codex-0.json'),'utf8'));
        expect(ref).toMatchObject({threadId:'public',nativeId:native,profile:codex,transcript:'sessions/test.jsonl',firstLineBytes:Buffer.byteLength(header)});
        expect(ref.messages).toBeUndefined();
        expect((await readFile(path,'utf8')).startsWith(original)).toBe(true);
      }else await expect(promise).rejects.toThrow();
    } finally {await rm(root,{recursive:true,force:true});}
  });
}
test('Desktop references reject escaped paths and linked rollout files',async()=>{
  const root=await mkdtemp(join(tmpdir(),'desktop-reference-path-'));
  try{
    await mkdir(join(root,'sessions'));
    const path=join(root,'outside.jsonl');await writeFile(path,JSON.stringify({type:'session_meta',payload:{id:native}})+'\n');
    await expect(desktopTranscriptReference(root,path,native)).rejects.toThrow();
    if(process.platform!=='win32'){
      const link=join(root,'sessions','linked.jsonl');await symlink(path,link);
      await expect(desktopTranscriptReference(root,link,native)).rejects.toThrow();
    }
  }finally{await rm(root,{recursive:true,force:true});}
});
