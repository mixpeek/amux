import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const src=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
function fn(name){
 const i=src.indexOf('function '+name+'(');let d=0,j=src.indexOf('{',i);
 for(;j<src.length;j++){if(src[j]==='{')d++;if(src[j]==='}'){d--;if(!d)break;}}
 return src.slice(i,j+1);
}
function load(){
 const g={peekSessionDir:'/Users/ethan/Dev/amux',esc:s=>String(s),escJs:s=>String(s),
  _resolveOutputPath:p=>p.startsWith('/')?p:'/Users/ethan/Dev/amux/'+p,
  _encodeHashPath:p=>p,_outputHrefCache:new Map(),location:{origin:'https://h',pathname:'/'}};
 const body=['_outputFileHref','_outputFileLink','_linkifyPaths'].map(fn).join('\n');
 return new Function(...Object.keys(g),body+';return _linkifyPaths;')(...Object.values(g));
}
// A linked path is an <a> whose data-path is what _openLinkedPath opens.
const target=h=>(h.match(/<a class="(?:file|md)-link"[^>]* data-path="([^"]*)"/)||[])[1];
test('an @-mention marker is not part of the linked path',()=>{
 const f=load();
 assert.equal(target(f('see @/Users/ethan/.amux/uploads/1a04feff407b-image.png')),'/Users/ethan/.amux/uploads/1a04feff407b-image.png');
 assert.equal(target(f('edit @src/app.js')),'src/app.js');
 assert.match(f('see @/tmp/a/b.png'),/see @<a /);
 assert.equal(target(f('plain /tmp/a/b.txt')),'/tmp/a/b.txt');
});
