const zh = navigator.language.startsWith('zh');
if (zh) {
  document.querySelector('#title').textContent = '等待 Harness 就绪';
  document.querySelector('#message').textContent = '服务就绪后将自动恢复连接。你的数据没有被更改。若持续无法连接，请打开启动器检查或恢复 Harness。';
  document.querySelector('#retry').textContent = '重试连接';
  document.querySelector('#launcher').textContent = '打开启动器';
}
for (const action of ['retry', 'launcher']) document.querySelector(`#${action}`).onclick = async () => {
  const button = document.querySelector(`#${action}`); button.disabled = true;
  try { await window.nexusShell[action](); }
  catch { document.querySelector('#status').textContent = zh ? '操作未完成，请稍后重试。' : 'Could not complete the action. Please retry.'; }
  finally { button.disabled = false; }
};
let busy = false;
async function status() {
  if (busy) return;
  busy = true;
  try {
    const { phase } = await window.nexusShell.switchStatus();
    if (['failed', 'interrupted'].includes(phase)) document.querySelector('#status').textContent = zh
      ? '配置切换未完成。请打开启动器检查当前配置并重新启动；不会自动重复切换。'
      : 'Profile switching did not complete. Open Launcher to inspect the selected profile and start it; the switch will not be repeated automatically.';
  } catch {} finally { busy = false; }
}
void status(); setInterval(status, 2000);
