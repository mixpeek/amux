import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const src=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
function load(){
 const i=src.indexOf('function _linkifyPaths(safeHtml)');let d=0,j=src.indexOf('{',i);
 for(;j<src.length;j++){if(src[j]==='{')d++;if(src[j]==='}'){d--;if(!d)break;}}
 const g={peekSessionDir:'/Users/ethan/Dev/amux',esc:s=>String(s),escJs:s=>String(s),
  _resolveOutputPath:p=>p.startsWith('/')?p:'/Users/ethan/Dev/amux/'+p};
 return new Function(...Object.keys(g),src.slice(i,j+1)+';return _linkifyPaths;')(...Object.values(g));
}
const target=h=>(h.match(/_openPathFromOutput\('([^']*)'\)/)||[])[1];
test('an @-mention marker is not part of the linked path',()=>{
 const f=load();
 assert.equal(target(f('see @/Users/ethan/.amux/uploads/1a04feff407b-image.png')),'/Users/ethan/.amux/uploads/1a04feff407b-image.png');
 assert.equal(target(f('edit @src/app.js')),'src/app.js');
 assert.match(f('see @/tmp/a/b.png'),/see @<span/);
 assert.equal(target(f('plain /tmp/a/b.txt')),'/tmp/a/b.txt');
});
