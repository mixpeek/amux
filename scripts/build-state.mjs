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
  });
  graph.set(name,calls);
}
let changed=true;
while(changed) {
  changed=false;
  for(const [name,calls] of graph) if(!mutations.has(name) && [...calls].some(call=>mutations.has(call))) { mutations.add(name);changed=true; }
}
const controls=Object.fromEntries([...mutations].sort().map(name=>[name,{kind:'command.'+name,feedback_required:true,target:'control-or-request',queue_policy:'transport-declared'}]));
const registry={measured:true,n_considered:functions.size,command_handlers:controls,
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
