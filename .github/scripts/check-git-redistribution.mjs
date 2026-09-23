import fs from 'node:fs';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
import {gitDistribution} from '../../apps/nexus-launcher/desktop/scripts/bundled-git.mjs';
export function cleared(inventory) {
  if (!Array.isArray(inventory)) return false;
  return [['win32','x64'],['win32','arm64'],['darwin','x64'],['darwin','arm64'],['linux','arm64']].every(([platform,arch])=>{
    const expected=gitDistribution({platform,arch});
    const entries=inventory.filter(entry=>entry.archive===expected.archive);
    return entries.length===1 && entries[0].sha256===expected.sha256 && entries[0].publicRedistributionReady===true;
  });
}
if(process.argv[1] && path.resolve(process.argv[1])===fileURLToPath(import.meta.url)) {
  const inventory=JSON.parse(fs.readFileSync(new URL('../../docs/audits/git-redistribution-2026-09-23/archive-inventory.json',import.meta.url)));
  const ready=cleared(inventory);
  if(process.env.GITHUB_OUTPUT)fs.appendFileSync(process.env.GITHUB_OUTPUT,`cleared=${ready}\n`);
  console.log(ready?'Bundled Git redistribution clearance recorded.':'Bundled Git redistribution is not cleared; downloadable artifacts remain withheld.');
  if(!ready && process.argv.includes('--require'))process.exitCode=1;
}
