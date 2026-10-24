import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, readFile, rm, appendFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { readDesktopExportThread } from '../src/android-remote/upgrade-desktop-history';

for (const mode of ['settled','live','changed','foreign','cycle','missing-owner','partial','oversize'] as const) {
  test(`Desktop export retains its original owner: ${mode}`, async () => {
    const root=await mkdtemp(join(tmpdir(),'rmx-desktop-export-'));
    try {
      await mkdir(join(root,'sessions'));
      const path=join(root,'sessions/chat.jsonl');
      const source=[{type:'session_meta',payload:{id:mode==='foreign'?'wrong':'native'}},
        {type:'event_msg',payload:{type:mode==='live'?'task_started':'task_complete',turn_id:'turn'}}].map(v=>JSON.stringify(v)).join('\n')+'\n';
      await writeFile(path,source);
      const calls:string[]=[];
      let page=0;
      const client={async request(method:string,params:Record<string,any>){
        calls.push(method);
        if(method==='thread/read'){expect(params.includeTurns).toBe(false);return {thread:{id:'native',path,turns:[],status:{type:'notLoaded'}}};}
        if(method==='thread/turns/list'){
          expect(params.itemsView).toBe('notLoaded');expect(params.limit).toBe(1);
          // The saved reader lies about a running owner's status.
          return {data:[{id:'turn',status:mode==='live'?'interrupted':'completed'}],nextCursor:mode==='partial'?'again':null};
        }
        if(method==='thread/items/list'){
          expect(params.limit).toBe(8);
          if(mode==='changed')await appendFile(path,JSON.stringify({type:'event_msg',payload:{type:'task_started',turn_id:'next'}})+'\n');
          return {data:[{item:{id:'item-'+page++,type:'userMessage',content:[{type:'text',text:mode==='oversize'?'x'.repeat(13*1024*1024):'Keep original prompt'}]}}],nextCursor:mode==='cycle'?'same':null};
        }
        throw new Error('Mutating RPC is forbidden');
      }};
      const result=readDesktopExportThread(client,'native',root,async()=>mode!=='missing-owner');
      if(mode==='settled'){
        const thread=await result;expect(thread.turns[0].items[0].content[0].text).toBe('Keep original prompt');
        expect(await readFile(path,'utf8')).toBe(source);
      }else await expect(result).rejects.toThrow();
      expect(calls.every(m=>['thread/read','thread/turns/list','thread/items/list'].includes(m))).toBe(true);
      if(mode==='live')expect(calls).toEqual(['thread/read']);
    }finally{await rm(root,{recursive:true,force:true});}
  });
}

import { upgradeProjects } from '../src/android-remote/upgrade-projects';
test('upgrade retains Desktop-only projects, Android mirrors and their stable IDs', () => {
  const time='2026-01-01T00:00:00Z';
  const root=join(tmpdir(),'existing');
  const projects=upgradeProjects([[{id:'mirror',cwd:root,title:'Android',createdAt:time}],
    [{id:'desktop',workspaceRoot:join(root,'desktop'),title:'Desktop'}]],time);
  expect(projects).toHaveLength(2);
  expect(projects.map(p=>[p.id,p.title,p.workspaceRoot,p.updatedAt])).toEqual([
    ['mirror','Android',root,time],['desktop','Desktop',join(root,'desktop'),time]]);
  expect(()=>upgradeProjects([[projects[0]!],[{id:'mirror',workspaceRoot:join(root,'different')}]],time)).toThrow();
});
