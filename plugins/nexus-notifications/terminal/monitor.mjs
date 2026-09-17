// Optional terminal delivery surface for this plugin.
import fs from 'node:fs';
const [file, preferences] = process.argv.slice(2);
if (!file || !preferences || !process.stdout.isTTY || !process.stdin.isTTY) {
  console.error('Run this monitor in an interactive terminal.'); process.exit(1);
}
let epoch, sequence = 0, focused;
const messages = {
  completed: 'Turn completed / 回合完成', failed: 'Turn failed / 回合失败',
  approval: 'Approval needed / 等待审批', question: 'Answer needed / 等待回答',
  blocked: 'Task blocked / 任务受阻', 'job-completed': 'Background job completed / 后台任务完成',
  'job-failed': 'Background job failed / 后台任务失败',
};
console.log('Nexus notifications / 通知终端 — Ctrl+C to stop. Focus detection requires terminal focus reporting (DEC mode 1004).');
process.stdin.setRawMode(true); process.stdin.resume();
process.stdout.write('\x1b[?1004h');
let input = '';
process.stdin.on('data', bytes => {
  input = (input + bytes.toString()).slice(-32);
  if (input.includes('\x03')) process.exit();
  for (const match of input.matchAll(/\x1b\[([IO])/g)) focused = match[1] === 'I';
  if (/\x1b\[[IO]/.test(input)) input = '';
});
process.on('exit', () => { process.stdout.write('\x1b[?1004l'); process.stdin.setRawMode(false); });
setInterval(() => {
  try {
    if (fs.statSync(file).size > 262144 || fs.statSync(preferences).size > 16384) return;
    const state = JSON.parse(fs.readFileSync(file, 'utf8'));
    const config = JSON.parse(fs.readFileSync(preferences, 'utf8'));
    if (epoch !== state.epoch) { epoch = state.epoch; sequence = state.sequence; return; }
    const fresh = state.events.filter(e => e.sequence > sequence); sequence = state.sequence;
    for (const event of fresh) {
      if (!messages[event.kind] || config.categories?.[event.kind] === false) continue;
      if (config.terminal !== 'always' && !(config.terminal === 'unfocused' && focused === false)) continue;
      // TERM_PROGRAM merely identifies a terminal, not OSC 9 support.
      // In particular Terminal.app needs the standard bell fallback.
      const method = config.method === 'auto' ? (process.env.WT_SESSION || process.env.TERM_PROGRAM === 'iTerm.app' ? 'osc9' : 'bel') : config.method;
      const clean = v => typeof v === 'string' ? v.replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, ' ').slice(0, 600) : '';
      const detail = clean(event.body) || (event.detailCode === 'max-tokens' ? 'Output token limit reached / 输出长度达到上限' : '');
      const message = [clean(event.title) || clean(event.session), messages[event.kind], detail].filter(Boolean).join(' — ');
      process.stdout.write(method === 'osc9' ? `\x1b]9;Nexus: ${message}\x07` : '\x07');
      console.log(`[Nexus] ${message}`);
    }
  } catch { /* Host is stopped or restarting; no history replay. */ }
}, 1000);
