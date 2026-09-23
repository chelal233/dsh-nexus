// Use the selected Harness implementation without loading the user profile's plugins.
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { pathToFileURL, fileURLToPath } from 'node:url';
export async function manage({root, home, profile, action, package: name, packageManager, work}) {
  const anchor = path.join(root, 'apps/cli/package.json');
  const require = createRequire(anchor);
  const source = !fs.existsSync(path.join(root, 'packages/boot/plugin-manager/lib/index.js'));
  let unregister;
  if (source) {
    const api = await import(pathToFileURL(require.resolve('tsx/esm/api')));
    unregister = api.register({tsconfig:path.join(root, 'tsconfig.json')});
  }
  let ctx;
  try {
    const suffix = source ? 'src/index.ts' : 'lib/index.js';
    const {boot, resolveBundleDir} = await import(pathToFileURL(path.join(root,'packages/boot/app-boot',suffix)));
    const {default:Manager} = await import(pathToFileURL(path.join(root,'packages/boot/plugin-manager',suffix)));
    fs.mkdirSync(work,{recursive:true});
    const empty = path.join(work,'empty.yml');
    fs.writeFileSync(empty,'[]\n');
    const dir = path.join(home,'profiles',profile);
    ctx = await boot('dsh',empty,[],async context => {
      context.provide('profileContext',{name:profile,dir,patchPath:path.join(dir,'cordis.patch.yml'),installAnchor:anchor,cwd:dir,home,startedBundles:[],overlays:[],packageManager,telemetryDisabledEnv:process.env.DSH_TELEMETRY_DISABLED});
      await context.plugin(Manager,{});
    });
    const manager = ctx.pluginManager;
    let result = null;
    switch(action) {
      case 'list': break;
      case 'enable': case 'disable': result=await manager.setBundleEnabled(name,action==='enable'); break;
      case 'remove': result=await manager.removeBundle(name); break;
      case 'inspect': result=await manager.inspect(name); break;
      case 'install': result=await manager.installBundle(name,{enabled:false}); break;
      default: throw new Error('Unsupported plugin operation');
    }
    // Nexus renders its own icon; embedded upstream icon data can exceed the
    // desktop bridge response limit even for a small inventory. Keep all text,
    // identities and permission decisions, and omit only unused image bytes.
    const withoutIcon = value => value?.meta ? {...value,meta:{...value.meta,icon:undefined}} : value;
    const bundles=(await manager.listBundles()).map(bundle=>{
      let repository;
      try {
        const directory=resolveBundleDir('dsh',bundle.name,anchor,dir);
        const file=path.join(directory,'package.json');
        if(fs.statSync(file).size<=1024*1024) {
          const manifest=JSON.parse(fs.readFileSync(file,'utf8'));
          repository=typeof manifest.repository==='string'?manifest.repository:manifest.repository?.url;
        }
      } catch { /* Metadata must not prevent upstream management or recovery. */ }
      return {...withoutIcon(bundle),repository:typeof repository==='string'?repository:undefined,rows:bundle.rows.map(withoutIcon)};
    });
    return {profile,bundles,result};
  } finally { await ctx?.fiber.dispose(); await unregister?.(); }
}
if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  try { const input=JSON.parse(process.argv[2]); const result=await manage(input); fs.writeFileSync(input.output,JSON.stringify(result),{mode:0o600,flag:'wx'}); }
  catch(error) { console.error(error.stack || String(error)); process.exitCode=1; }
}
