export const kinds = ['completed', 'failed', 'approval', 'question', 'blocked', 'job-completed', 'job-failed', 'harness-failed', 'update-ready'];
export const defaults = { desktop: 'unfocused', terminal: 'off', method: 'auto', categories: Object.fromEntries(kinds.map(k => [k, true])) };
export function settings(value = {}) {
  return {
    desktop: ['off', 'unfocused', 'always'].includes(value.desktop) ? value.desktop : defaults.desktop,
    terminal: ['off', 'unfocused', 'always'].includes(value.terminal) ? value.terminal : defaults.terminal,
    method: ['auto', 'osc9', 'bel'].includes(value.method) ? value.method : 'auto',
    categories: Object.fromEntries(kinds.map(k => [k, typeof value.categories?.[k] === 'boolean' ? value.categories[k] : true])),
  };
}
export function shouldNotify(config, kind, focused, channel = 'desktop') {
  return kinds.includes(kind) && config.categories[kind] && (config[channel] === 'always' || (config[channel] === 'unfocused' && focused === false));
}
const copy = {
  completed: ['Turn completed', '回合已完成'], failed: ['Turn failed', '回合执行失败'],
  approval: ['Approval needed', '需要你批准操作'], question: ['Answer needed', '需要你回答问题'],
  blocked: ['Task blocked', '任务受阻，需要处理'], 'job-completed': ['Background job completed', '后台任务已完成'],
  'job-failed': ['Background job failed', '后台任务失败'], 'harness-failed': ['Harness failed', 'Harness 启动失败或崩溃'],
  'update-ready': ['Update ready to install', '更新已下载，可重启应用'],
};
export function notificationText(kind, locale) { return copy[kind]?.[locale.startsWith('zh') ? 1 : 0] ?? 'Nexus'; }
export function notificationContent(kind, locale, task = {}) {
  const clean = (v, n) => typeof v === 'string' ? [...v.replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, ' ').trim()].slice(0, n).join('') : '';
  const status = notificationText(kind, locale);
  const title = clean(task.title, 120) || (task.session ? (locale.startsWith('zh') ? '对话 ' : 'Conversation ') + clean(task.session, 60) : 'Nexus Launcher');
  const detail = clean(task.body, 500) || (task.detailCode === 'max-tokens' ? (locale.startsWith('zh') ? '已达到本回合输出长度上限' : 'The turn reached its output token limit') : '');
  return { title: `${title} · ${status}`, body: detail || status };
}
export function taskFocused(task, views, now = Date.now()) {
  // Launcher focus is unrelated to reading a Harness conversation. A different
  // session in either a browser or the independent window must not suppress it.
  return !!task?.session && (views ?? []).some(v => v.focused === true && v.session === task.session && now - v.time >= 0 && now - v.time < 10000);
}
export class EventCursor {
  startedAt = Date.now();
  epoch; sequence = 0;
  consume(snapshot) {
    if (!snapshot?.epoch) return [];
    if (snapshot.epoch !== this.epoch) {
      this.epoch = snapshot.epoch; this.sequence = snapshot.sequence;
      return (snapshot.events ?? []).filter(e => e.time >= this.startedAt && kinds.includes(e.kind));
    }
    const fresh = (snapshot.events ?? []).filter(e => e.sequence > this.sequence && kinds.includes(e.kind));
    this.sequence = Math.max(this.sequence, snapshot.sequence ?? 0);
    return fresh;
  }
}
