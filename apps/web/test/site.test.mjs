import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile,readdir,stat,writeFile} from 'node:fs/promises';
import {resolve,dirname,join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawn} from 'node:child_process';
import {bridgeCommand,replayCommand} from '../onboarding.mjs';
const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
async function files(dir){const result=[];for(const entry of await readdir(dir,{withFileTypes:true})){const p=join(dir,entry.name);if(entry.isDirectory())result.push(...await files(p));else result.push(p)}return result}
test('every local navigation and asset target exists',async()=>{const all=await files(join(root,'dist'));const pages=all.filter(p=>p.endsWith('.html'));assert.equal(pages.length,20);for(const p of pages){const html=await readFile(p,'utf8');for(const m of html.matchAll(/(?:href|src)="(\/[^"#?]*)[^"\s]*"/g)){const target=join(root,'dist',m[1]);let exists=false;try{await stat(target);exists=true}catch{}assert.ok(exists,`${p}: broken target ${m[1]}`)}assert.ok(html.includes('name="viewport"'));assert.ok(html.includes('Skip to content'))}});
test('quickstart preserves keys and distinguishes server and demo',async()=>{const html=await readFile(join(root,'dist/docs/quickstart/index.html'),'utf8');for(const required of ['os.O_EXCL','0o600','--multi','SHOGUN_DB_KEY_DEMO','127.0.0.1:7464','does not prove automatic extraction','not a graphical dashboard'])assert.ok(html.includes(required),required)});
test('onboarding links the runnable source path and labels replay limitations',async()=>{
  for(const command of [bridgeCommand,replayCommand]){
    assert.ok(command.includes('\\\n  --'),'multi-line commands retain their shell continuation');
    assert.ok(!command.includes('\n+'),'copied commands must not contain patch markers');
  }
  const mcp=await readFile(join(root,'dist/docs/mcp/index.html'),'utf8');
  for(const client of ['claude-code','cursor','codex','claude-desktop'])assert.ok(mcp.includes('--print-config '+client));
  for(const header of ['Mcp-Method','Mcp-Name'])assert.ok(mcp.includes(header));
  const state=await readFile(join(root,'dist/docs/state-walkthrough/index.html'),'utf8');
  for(const text of ['--example maya_handoff','Synthetic model answers','test credential was unavailable'])assert.ok(state.includes(text),text);
  assert.ok(!mcp.includes('>undefined</a>'),'every document needs a sidebar label');
  const home=await readFile(join(root,'dist/index.html'),'utf8');
  assert.ok(home.includes('href="/docs/state-walkthrough/"'));
});
test('preview serves pages, agent negotiation, and real 404s',async()=>{const child=spawn(process.execPath,['scripts/serve.mjs'],{cwd:root,env:{...process.env,PORT:'3198'},stdio:['ignore','pipe','pipe']});try{await new Promise((res,rej)=>{child.once('error',rej);child.once('exit',code=>rej(new Error('server exited '+code)));child.stdout.once('data',res)});for(const path of ['/','/product/','/docs/quickstart/','/playground/','/assets/manrope-latin.woff2'])assert.equal((await fetch('http://127.0.0.1:3198'+path)).status,200,path);const prompt=await fetch('http://127.0.0.1:3198/setup-prompt.txt');
assert.equal(prompt.status,200);
const text=await prompt.text();
assert.ok(text.includes('Do not assume a particular agent product.'));
assert.ok(!text.includes('https://shogunagent.com/'),'preview setup must not send users to undeployed production docs');
const references=[...text.matchAll(/http:\/\/127\.0\.0\.1:3198\/[^\s]+/g)].map(match=>match[0]);
assert.equal(references.length,4);
for(const url of references)assert.equal((await fetch(url)).status,200,url);
const module=await fetch('http://127.0.0.1:3198/setup-prompt.mjs');
assert.equal(module.status,200);
assert.match(module.headers.get('content-type'),/javascript/,'browser modules require a JavaScript MIME type');
const canonical=await readFile(join(root,'dist/setup-prompt.txt'),'utf8');
assert.ok(canonical.includes('https://shogunagent.com/docs/mcp/'),'production build retains canonical setup references');
try {
  await writeFile(join(root,'dist/setup-prompt.txt'),canonical+'\nPreview rebuild regression marker\n');
  assert.ok((await (await fetch('http://127.0.0.1:3198/setup-prompt.txt')).text()).includes('Preview rebuild regression marker'),'a running preview must serve the latest built prompt');
} finally {await writeFile(join(root,'dist/setup-prompt.txt'),canonical)}
const agent=await fetch('http://127.0.0.1:3198/',{headers:{Accept:'text/markdown'}});assert.match(await agent.text(),/^# ShogunAgent/);assert.equal((await fetch('http://127.0.0.1:3198/missing-page')).status,404);assert.equal((await fetch('http://127.0.0.1:3198/',{method:'POST'})).status,405)}finally{child.kill();}});
