// Keep an existing isolated two-version fixture available for human acceptance.
import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { readFile, writeFile, appendFile, stat } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import path from 'node:path';

const fixture = path.resolve(process.argv[2]);
const config = JSON.parse(await readFile(path.join(fixture, 'baseline-builder.json'), 'utf8'));
const origin = new URL(config.publish[0].url);
if (origin.hostname !== '127.0.0.1') throw new Error('Manual testing requires a loopback feed');
const executable = path.join(fixture, 'installed', `${config.executableName}.exe`);
await stat(executable);
const telemetry = path.join(fixture, 'running.json');
const stateFile = path.join(fixture, 'manual-state.json');
let published = 'baseline';
try { published = JSON.parse(await readFile(stateFile, 'utf8')).published === 'published' ? 'published' : 'baseline'; } catch {}
const token = randomBytes(24).toString('hex');
let busy = false;
const assets = new Map([
  ['/nexus-update-test-0.1.3.exe', path.join(fixture, 'baseline/nexus-update-test-0.1.3.exe')],
  ['/nexus-update-test-0.1.4-local.1.exe', path.join(fixture, 'published/nexus-update-test-0.1.4-local.1.exe')],
]);
async function running() {
  try {
    const value = JSON.parse(await readFile(telemetry, 'utf8'));
    if (value.executable.toLowerCase() !== executable.toLowerCase()) return null;
    process.kill(value.pid, 0); return value;
  } catch { return null; }
}
function launch() {
  const child = spawn(executable, [], { cwd: path.dirname(executable), detached: true, stdio: 'ignore', windowsHide: false });
  child.on('error', error => console.error(error)); child.unref();
}
async function publish() {
  const current = await running();
  if (current && current.version !== '0.1.3') throw new Error('当前已经升级到新版本，无需再次发布。');
  published = 'published';
  await writeFile(stateFile, JSON.stringify({ published }));
  if (current) {
    await writeFile(path.join(fixture, 'quit-request'), 'quit');
    const deadline = Date.now() + 20000;
    while (await running()) {
      if (Date.now() > deadline) throw new Error('测试版尚未退出，请在托盘退出后点击“打开测试版”。');
      await new Promise(resolve => setTimeout(resolve, 150));
    }
  }
  launch();
}
const html = `<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Nexus 本地更新测试</title>
<style>body{font:16px/1.8 system-ui;background:#f2f7f6;color:#163832;max-width:820px;margin:50px auto;padding:24px}h1{font-size:30px}button{font:inherit;background:#087c69;color:white;border:0;border-radius:8px;padding:12px 20px;margin:8px 10px 8px 0;cursor:pointer}button:disabled{opacity:.5}section{background:white;padding:24px;border-radius:12px;margin:20px 0}pre{white-space:pre-wrap}small{color:#55706b}</style>
<h1>Nexus 本地更新测试</h1><p>这是真实安装的隔离测试版。你亲自点击应用中的“更新”，完成安装和重启。</p>
<section><strong>操作步骤</strong><ol><li>查看已打开的旧版应用：此时没有可用更新。</li><li>点击下方“发布新版本并重启测试版”，模拟新版本发布后的下一次启动。</li><li>返回 Nexus，等待下载完成，在左下角侧栏底部点击“更新”。</li><li>观察窗口退出和重新打开，再回到这里确认运行版本已变为 0.1.4-local.1。</li></ol>
<button onclick="action('publish')">发布新版本并重启测试版</button><button onclick="action('launch')">打开测试版</button><p id="message"></p></section>
<section><strong>实时状态</strong><pre id="status">读取中…</pre><small>旧版 0.1.3 → 新版 0.1.4-local.1。更新源仅在本机运行，无 GitHub 发布。自动更新开关位于应用设置中；检测周期仍为启动时及每两小时。</small></section>
<script>async function refresh(){try{const s=await(await fetch('/api/status')).json();document.querySelector('#status').textContent='发布源：'+s.release+'\\n测试版：'+(s.running?s.running.version+'（进程 '+s.running.pid+'）':'未运行')+'\\n自动更新：'+(s.enabled?'开启':'关闭');}catch{document.querySelector('#status').textContent='本地更新源已停止';}}
async function action(name){document.querySelectorAll('button').forEach(b=>b.disabled=true);try{const r=await fetch('/api/'+name,{method:'POST',headers:{'X-Test-Token':'${token}'}});const s=await r.json();if(!r.ok)throw new Error(s.error);document.querySelector('#message').textContent=name==='publish'?'新版本已发布。请返回 Nexus，等待左下角侧栏底部出现“更新”后亲自点击。':'测试版已打开。';}catch(e){document.querySelector('#message').textContent=e.message;}finally{document.querySelectorAll('button').forEach(b=>b.disabled=false);refresh();}}refresh();setInterval(refresh,2000);</script></html>`;
const server = createServer(async (request, response) => {
  try {
    if (request.headers.host !== origin.host) { response.writeHead(403).end(); return; }
    const url = new URL(request.url, origin);
    if (request.method === 'POST') {
      if (request.headers.origin !== origin.origin || request.headers['x-test-token'] !== token) { response.writeHead(403).end(); return; }
      if (busy) throw new Error('上一个操作尚未完成');
      busy = true;
      try {
        if (url.pathname === '/api/publish') await publish();
        else if (url.pathname === '/api/launch') launch();
        else { response.writeHead(404).end(); return; }
      } finally { busy = false; }
      response.writeHead(200, { 'Content-Type': 'application/json' }).end('{}'); return;
    }
    if (request.method !== 'GET' && request.method !== 'HEAD') { response.writeHead(405).end(); return; }
    if (url.pathname === '/') { response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' }).end(html); return; }
    if (url.pathname === '/api/status') {
      const settings = JSON.parse(await readFile(path.join(fixture, 'desktop/desktop-update.json'), 'utf8'));
      response.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }).end(JSON.stringify({ release: published === 'baseline' ? '0.1.3' : '0.1.4-local.1', running: await running(), enabled: settings.enabled !== false })); return;
    }
    const file = url.pathname === '/latest-x64.yml' ? path.join(fixture, published, 'latest-x64.yml') : assets.get(url.pathname);
    if (!file) { response.writeHead(404).end(); return; }
    const info = await stat(file);
    await appendFile(path.join(fixture, 'manual-feed.jsonl'), JSON.stringify({ at: new Date().toISOString(), path: url.pathname, published }) + '\n');
    response.writeHead(200, { 'Content-Length': info.size, 'Cache-Control': 'no-store', 'Content-Type': file.endsWith('.yml') ? 'text/yaml' : 'application/octet-stream' });
    if (request.method === 'HEAD') response.end(); else createReadStream(file).pipe(response);
  } catch (error) { if (!response.headersSent) response.writeHead(500, { 'Content-Type': 'application/json' }).end(JSON.stringify({ error: error.message })); else response.destroy(); }
});
await new Promise((resolve, reject) => { server.once('error', reject); server.listen(Number(origin.port), '127.0.0.1', resolve); });
await writeFile(path.join(fixture, 'manual-server.json'), JSON.stringify({ pid: process.pid, url: origin.origin }));
console.log(`Manual update source: ${origin.origin}`);
