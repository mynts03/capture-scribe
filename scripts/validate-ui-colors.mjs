import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {resolve} from 'node:path';

export function validateUIColors(root=fileURLToPath(new URL('..',import.meta.url))) {
  const colors=JSON.parse(readFileSync(resolve(root,'src/ui-colors.json'),'utf8'));
  if(!colors||Array.isArray(colors)||typeof colors!=='object'||!Object.keys(colors).length)throw Error('Invalid UI color catalog');
  const variables=new Set();
  for(const [role,value] of Object.entries(colors)) {
    if(!/^[a-z][A-Za-z0-9]*$/.test(role)||typeof value!=='string'||!/^#[0-9a-f]{6}([0-9a-f]{2})?$/.test(value))throw Error('Invalid UI color: '+role);
    variables.add('--'+role.replace(/[A-Z]/g,c=>'-'+c.toLowerCase()));
  }
  const css=readFileSync(resolve(root,'src/App.css'),'utf8');
  const nonColors=new Set(['--font-body','--font-note','--font-heading','--font-button','--app-header-height']);
  for(const [,name] of css.matchAll(/var\((--[a-z-]+)/g))if(!nonColors.has(name)&&!variables.has(name))throw Error('Missing UI color: '+name);
  return {roles:Object.keys(colors).length};
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url))console.log(JSON.stringify(validateUIColors()));
