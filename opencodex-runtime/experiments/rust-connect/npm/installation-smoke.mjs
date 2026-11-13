// Offline fresh-install and version-upgrade rehearsal through the real npm bin.
// Synthetic dashboard/profiles; never publishes, installs globally or sends a turn.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { cp, mkdir, mkdtemp, readFile, rm, writeFile, access } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { createServer } from 'node:net';
import { createInterface } from 'node:readline';
import { buildPackages, buildPublishedPackage } from './build.mjs';

if (process.argv.length !== 3) throw new Error('Usage: node npm/installation-smoke.mjs NATIVE_BINARY');
const binary=resolve(process.argv[2]);
const source=resolve(dirname(fileURLToPath(import.meta.url)),'../../..');
const root=await mkdtemp(join(tmpdir(),'native install upgrade 测试 '));
const children=new Set();
const env={...process.env,OPENCODEX_HOME:join(root,'user state'),OPENCODEX_BUN_PATH:join(root,'no-bun'),
  npm_config_userconfig:join(root,'npmrc'),npm_config_globalconfig:join(root,'global-npmrc'),
  npm_config_cache:join(root,'cache'),npm_config_offline:'true',npm_config_audit:'false',npm_config_fund:'false'};
const exists=async p=>access(p).then(()=>true,()=>false);
let npmCli;
for (const p of [process.env.npm_execpath,join(dirname(process.execPath),'node_modules/npm/bin/npm-cli.js'),join(dirname(process.execPath),'../lib/node_modules/npm/bin/npm-cli.js')].filter(Boolean)) {
  if (await exists(p)) {npmCli=p;break;}
}
if (!npmCli) throw new Error('Run with a standard Node/npm distribution');
const run=(exe,args,cwd=root)=>{
  const p=spawnSync(exe,args,{env,cwd,windowsHide:true,shell:false,encoding:'utf8',timeout:120_000,maxBuffer:2*1024*1024});
  assert.equal(p.status,0,p.stderr||p.error?.message);return p.stdout;
};
const npm=(args,cwd)=>run(process.execPath,[npmCli,...args,'--ignore-scripts','--offline','--no-audit','--no-fund'],cwd);
const exited=(child)=>new Promise((resolve,reject)=>{
  if(child.exitCode!==null)return resolve(child.exitCode);
  const timer=setTimeout(()=>reject(new Error('Native supervisor did not stop')),10_000);
  child.once('exit',code=>{clearTimeout(timer);resolve(code);});
});
async function availablePort() {
  for (let i=0;i<50;i++) {
    const server=createServer(); await new Promise(r=>server.listen(0,'127.0.0.1',r));
    const port=server.address().port;
    if(port>65530) {await new Promise(r=>server.close(r));continue;}
    const phone=createServer();
    const available=await new Promise(r=>{phone.once('error',()=>r(false));phone.listen(port+5,'0.0.0.0',()=>r(true));});
    await new Promise(r=>server.close(r));
    if(available) {await new Promise(r=>phone.close(r));return port;}
  }
  throw new Error('No test port pair available');
}
try {
  await writeFile(env.npm_config_userconfig,'');await writeFile(env.npm_config_globalconfig,'');
  const fixture=join(root,'package-source'),prefix=join(root,'npm-install');
  await mkdir(fixture);await mkdir(prefix);await mkdir(join(fixture,'gui/dist'),{recursive:true});
  for(const dir of ['bin','src'])await cp(join(source,dir),join(fixture,dir),{recursive:true});
  const manifest=JSON.parse(await readFile(join(source,'package.json'),'utf8'));
  // The native first-run path needs no compatibility dependencies. Leaving
  // them absent makes an accidental fallback fail this offline rehearsal.
  await writeFile(join(fixture,'package.json'),JSON.stringify({...manifest,dependencies:{}}));
  await writeFile(join(fixture,'gui/dist/index.html'),'<html><head></head><body>Native setup fixture</body></html>');
  const port=await availablePort(),config=join(root,'setup.json');
  await writeFile(config,JSON.stringify({workspace:root,connection:{dashboard_bind:`127.0.0.1:${port}`}}));
  const entry=join(prefix,'node_modules/@remodex/rmx/bin/ocx.mjs');
  const command=(...args)=>JSON.parse(run(process.execPath,[entry,...args]));
  let identity,phoneToken,receipt;
  for(const version of ['0.0.0','0.0.1']) {
    const packages=await buildPackages(binary,join(root,`build-${version}`),version);
    const published=await buildPublishedPackage(fixture,packages);
    const tarballs=[packages.native,published].map(dir=>join(dir,JSON.parse(npm(['pack','--json'],dir))[0].filename));
    npm(['install','--prefix',prefix,'--package-lock=false',...tarballs]);
    assert.equal(JSON.parse(await readFile(join(prefix,'node_modules/@remodex/rmx/package.json'),'utf8')).version,version);
    const args=version==='0.0.0'?['onboard','--config',config]:['onboard'];
    const prepared=command(...args);
    assert.equal(prepared.runtime,'rust');assert.equal(prepared.installed,true);
    if(identity)assert.equal(prepared.environmentId,identity);identity=prepared.environmentId;
    const saved=await readFile(join(env.OPENCODEX_HOME,'native-installation/native-install.json'),'utf8');
    if(receipt)assert.equal(saved,receipt);receipt=saved;
    const child=spawn(process.execPath,[entry,'start'],{env,cwd:root,windowsHide:true,shell:false,stdio:['ignore','pipe','pipe']});
    children.add(child);child.stderr.resume();
    const ready=await new Promise((resolve,reject)=>{
      const lines=createInterface({input:child.stdout});
      const timer=setTimeout(()=>{lines.close();reject(new Error('Native setup did not start'));},15_000);
      lines.once('line',line=>{clearTimeout(timer);lines.close();resolve(JSON.parse(line));});
      child.once('error',error=>{clearTimeout(timer);lines.close();reject(error);});
      child.once('exit',()=>{clearTimeout(timer);lines.close();reject(new Error('Native setup exited before ready'));});
    });
    assert.equal(ready.ready,true);assert.equal(command('status').running,true);
    const base=`http://127.0.0.1:${port+5}`;
    if(!phoneToken) {
      const invitation=JSON.parse(await readFile(ready.invitationFile,'utf8'));
      const response=await fetch(`${base}/oauth/token`,{method:'POST',signal:AbortSignal.timeout(5_000),
        body:new URLSearchParams({grant_type:'urn:ietf:params:oauth:grant-type:token-exchange',subject_token_type:'urn:t3:params:oauth:token-type:environment-bootstrap',requested_token_type:'urn:ietf:params:oauth:token-type:access_token',subject_token:invitation.pairingToken})});
      assert(response.ok);phoneToken=(await response.json()).access_token;assert.equal(typeof phoneToken,'string');
    }
    const authenticated=await fetch(`${base}/api/auth/session`,{headers:{authorization:`Bearer ${phoneToken}`},signal:AbortSignal.timeout(5_000)});
    assert(authenticated.ok);assert.equal((await authenticated.json()).authenticated,true);
    assert.equal(command('restart').restarted,true);
    assert.equal(command('stop').stopped,true);
    assert.equal(await exited(child),0);children.delete(child);
    assert.equal(command('status').running,false);
    assert.equal(command('features').product,'native-connect');
    const viewed=command('config','show');
    const edit=join(root,`settings-${version}.json`);
    await writeFile(edit,JSON.stringify({...viewed.configuration,connection:{...viewed.configuration.connection,label:`Fixture ${version}`}}));
    assert.equal(command('config','check',edit).valid,true);
    assert.equal(command('config','apply',edit,'--revision',String(viewed.revision)).revision,viewed.revision+1);
    assert.equal(command('config','rollback',String(viewed.revision),'--revision',String(viewed.revision+1)).historyRetained,true);
    receipt=await readFile(join(env.OPENCODEX_HOME,'native-installation/native-install.json'),'utf8');
    assert.equal(command('service','prepare',join(root,`service-${version}`)).runtimePinned,true);
  }
  npm(['uninstall','--prefix',prefix,'--package-lock=false','@remodex/rmx',`@remodex/native-connect-experiment-${process.platform==='linux'?'linux-x64-gnu':process.platform==='darwin'?'darwin-arm64':'win32-x64'}`]);
  assert.equal(await readFile(join(env.OPENCODEX_HOME,'native-installation/native-install.json'),'utf8'),receipt);
  assert(await exists(join(env.OPENCODEX_HOME,'native-installation/state/native-connect.sqlite')));
  // Both retained generations must run after their npm packages disappear.
  // Roll back the executable without resetting configuration, identity or pairing.
  for(const version of ['0.0.1','0.0.0']) {
    const registration=join(root,`service-${version}`);
    const pinned=join(registration,process.platform==='win32'?'runtime.exe':'runtime');
    const child=spawn(pinned,['--native-service','launch',registration],{env,cwd:root,windowsHide:true,shell:false,stdio:['ignore','pipe','pipe']});
    children.add(child);child.stderr.resume();
    const ready=await new Promise((resolve,reject)=>{
      const lines=createInterface({input:child.stdout});
      const timer=setTimeout(()=>{lines.close();reject(new Error('Pinned service did not start'));},15_000);
      lines.once('line',line=>{clearTimeout(timer);lines.close();resolve(JSON.parse(line));});
      child.once('error',error=>{clearTimeout(timer);lines.close();reject(error);});
      child.once('exit',()=>{clearTimeout(timer);lines.close();reject(new Error('Pinned service exited before ready'));});
    });
    assert.equal(ready.ready,true);
    const authenticated=await fetch(`http://127.0.0.1:${port+5}/api/auth/session`,{headers:{authorization:`Bearer ${phoneToken}`},signal:AbortSignal.timeout(5_000)});
    assert(authenticated.ok);assert.equal((await authenticated.json()).authenticated,true);
    assert.equal(JSON.parse(run(pinned,['--npm-upgrade','stop',join(env.OPENCODEX_HOME,'native-installation')])).stopped,true);
    assert.equal(await exited(child),0);children.delete(child);
  }
  console.log(JSON.stringify({freshNativeSetup:true,legacyRuntimeRequired:false,versionUpgrade:'0.0.0 -> 0.0.1',samePairing:true,restart:true,uninstallPreservedState:true,configurationApplyRollback:true,serviceAssetsPinned:true,pinnedServiceAfterNpmRemoval:true,pinnedRollbackKeepsPairing:true}));
} finally {
  for(const child of children) {
    if(child.exitCode===null) {
      if(process.platform==='win32')spawnSync(join(process.env.SystemRoot,'System32/taskkill.exe'),['/PID',String(child.pid),'/T','/F'],{windowsHide:true,shell:false,stdio:'ignore',timeout:10_000});
      else child.kill();
    }
    await exited(child).catch(()=>{});
  }
  await rm(root,{recursive:true,force:true});
}
