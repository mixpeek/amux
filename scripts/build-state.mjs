import fs from 'node:fs';
import * as espree from 'espree';
import { build } from 'esbuild';

const dir='crates/amux-dashboard/static/state';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const ast=espree.parse(source,{ecmaVersion:'latest',sourceType:'script'});
const functions=new Map();
function visit(node,fn) {
  if(!node || typeof node!=='object') return;
  if(node.type) fn(node);
  for(const value of Object.values(node)) {
    if(Array.isArray(value)) value.forEach(child=>visit(child,fn));
    else if(value?.type) visit(value,fn);
  }
}
visit(ast,node=>{if(node.type==='FunctionDeclaration' && node.id) functions.set(node.id.name,node);});
const graph=new Map(); const mutations=new Set();
for(const [name,node] of functions) {
  const calls=new Set();
  visit(node.body,child=>{
    if(child.type!=='CallExpression') return;
    const callee=child.callee.name;
    if(callee) calls.add(callee);
    const options=child.arguments[1];
    const method=options?.type==='ObjectExpression' && options.properties.find(p=>p.key?.name==='method')?.value?.value;
    if(['fetch','apiCall','_directInteractionFetch','_origFetch','_uploadRequest'].includes(callee) && /^(POST|PUT|PATCH|DELETE)$/i.test(method || '')) mutations.add(name);
    if(callee==='_interactionAccept' || callee==='_queueOp') mutations.add(name);
    // _mdaiJson(path, method, body): the method is the second argument.
    if(callee==='_mdaiJson' && /^(POST|PUT|PATCH|DELETE)$/i.test(child.arguments[1]?.value || '')) mutations.add(name);
  });
  graph.set(name,calls);
}
let changed=true;
while(changed) {
  changed=false;
  for(const [name,calls] of graph) if(!mutations.has(name) && [...calls].some(call=>mutations.has(call))) { mutations.add(name);changed=true; }
}
const controls=Object.fromEntries([...mutations].sort().map(name=>[name,{kind:'command.'+name,feedback_required:true,target:'control-or-request',queue_policy:'transport-declared'}]));
// The committed file holds only what changes when a command handler does: one
// sorted block per handler. Population counts (functions considered, handlers
// declared) move on almost any app.js edit, so they are printed below instead
// of committed; committing them made every pair of dashboard PRs conflict on
// line 3. state/feedback.mjs fetches this file at runtime, so the kernel bundle
// does not embed it either.
const registry={command_handlers:controls,
  coverage_scope:'Static call graph of named functions. Dynamic event listeners and computed dispatch require browser observation.'};
const json=JSON.stringify(registry,null,2)+'\n';
if(process.argv.includes('--check')) {
  if(fs.readFileSync(dir+'/control-registry.json','utf8')!==json) throw new Error('Interaction registry is stale: npm run build:state');
  const built=await build({entryPoints:[dir+'/kernel.mjs'],bundle:true,minify:true,format:'iife',write:false,outfile:dir+'/kernel.js'});
  if(!fs.readFileSync(dir+'/kernel.js').equals(Buffer.from(built.outputFiles[0].contents))) throw new Error('State bundle is stale: npm run build:state');
} else {
  fs.writeFileSync(dir+'/control-registry.json',json);
  await build({entryPoints:[dir+'/kernel.mjs'],bundle:true,minify:true,format:'iife',outfile:dir+'/kernel.js'});
}
console.log(`interaction registry: ${functions.size} functions considered, ${mutations.size} command-capable handlers declared`);
// The button-feedback audit doc is generated from the same app.js. It used to
// need its own command, which nobody ran: spa-lint's --check then failed every
// e2e shard on main after ordinary app.js edits (2026-10-01). Regenerate it
// here, where every app.js edit already passes. Skipped when the script is not
// in this tree (the pre-commit hook checks a partial export; CI checks the doc).
if(fs.existsSync('scripts/button-audit.mjs')) {
  const {execFileSync}=await import('node:child_process');
  execFileSync(process.execPath,['scripts/button-audit.mjs',...(process.argv.includes('--check')?['--check']:[])],{stdio:'inherit'});
}
