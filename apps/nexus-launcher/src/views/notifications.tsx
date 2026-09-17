import { useEffect, useState } from 'react';
import { useI18n } from '../i18n';
import { proxyRequest } from '../agent-bridge';
import { invoke } from '../desktop';
import { Panel, ActionButton } from '../ui-components';
import { Bell } from '@phosphor-icons/react';
import { errorMessage } from '../display-format';

type Mode = 'off' | 'unfocused' | 'always';
type Preferences = { observer: boolean; desktop: Mode; terminal: Mode; method: string; categories: Record<string, boolean> };
const categories = [
  ['completed', 'Turn completed', '回合完成'], ['failed', 'Turn failed', '回合失败'],
  ['approval', 'Approval needed', '等待审批'], ['question', 'Answer needed', '等待回答'],
  ['blocked', 'Task blocked', '任务受阻'], ['job-completed', 'Background job completed', '后台任务完成'],
  ['job-failed', 'Background job failed', '后台任务失败'], ['harness-failed', 'Harness failed', 'Harness 启动失败或崩溃'],
  ['update-ready', 'Update ready', 'Nexus 更新已下载'],
];
export function NotificationSettings() {
  const { locale } = useI18n();
  const text = (en: string, zh: string) => locale === 'zh' ? zh : en;
  const [value, setValue] = useState<Preferences | null>(null);
  const [capabilities, setCapabilities] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    let live = true;
    void proxyRequest('/v1/notifications').then(raw => {
      if (!live) return;
      const data = raw as { settings?: Partial<Preferences>; capabilities?: string[] };
      setCapabilities(data.capabilities ?? []);
      setValue({ observer: true, desktop: 'unfocused', terminal: 'off', method: 'auto', ...data.settings,
        categories: Object.fromEntries(categories.map(([key]) => [key, data.settings?.categories?.[key] !== false])) });
    }).catch(e => { if (live) setError(errorMessage(e)); });
    return () => { live = false; };
  }, []);
  async function action(fn: () => Promise<unknown>) {
    setBusy(true); setError(''); setSaved(false);
    try { await fn(); } catch (e) { setError(errorMessage(e)); } finally { setBusy(false); }
  }
  return <Panel title={text('Notifications', '通知设置')} icon={<Bell size={18} />}>
    <p>{text('Task notifications show the conversation and a content preview. Only when unfocused checks the relevant conversation page in your browser or independent window; Launcher focus does not affect task notifications. Terminal reminders use terminal focus.', '任务通知显示对话名称和内容摘要。“仅失焦”按浏览器或独立窗口中的对应对话页面判断，Launcher 是否聚焦不影响任务通知。终端提醒按终端焦点判断。')}</p>
    <p>{capabilities.includes('sessions') ? text('Harness event observer connected.', 'Harness 事件监听已连接。') : text('Task observer unavailable. Restart a supported Harness to connect; management notifications remain available.', '任务事件监听未连接。请重启支持的 Harness；管理通知仍可使用。')}</p>
    {value && <>
      <label className="form-check"><input type="checkbox" disabled={busy} checked={value.observer} onChange={e => { setSaved(false); setValue({ ...value, observer: e.target.checked }); }} />{text('Built-in task notification plugin (restart Harness to apply)', '内置任务通知插件（重启 Harness 生效）')}</label>
      {(['desktop', 'terminal'] as const).map(channel => <label className="field-label" key={channel}>
        {channel === 'desktop' ? text('System notifications', '系统通知') : text('Terminal reminders', '终端提醒')}
        <select disabled={busy} value={value[channel]} onChange={e => { setSaved(false); setValue({ ...value, [channel]: e.target.value as Mode }); }}>
          <option value="off">{text('Off', '关闭')}</option><option value="unfocused">{text('Only when unfocused', '仅失焦')}</option><option value="always">{text('Always', '始终')}</option>
        </select>
      </label>)}
      <label className="field-label">{text('Terminal method', '终端提醒方式')}<select disabled={busy} value={value.method} onChange={e => { setSaved(false); setValue({ ...value, method: e.target.value }); }}>
        <option value="auto">{text('Automatic', '自动')}</option><option value="osc9">{text('OSC 9 notification', 'OSC 9 通知')}</option><option value="bel">{text('BEL bell', 'BEL 响铃')}</option>
      </select></label>
      {categories.map(([key, en, zh]) => <label className="form-check" key={key}>
        <input type="checkbox" disabled={busy} checked={value.categories[key]} onChange={e => { setSaved(false); setValue({ ...value, categories: { ...value.categories, [key]: e.target.checked } }); }} />{text(en, zh)}
      </label>)}
      <ActionButton disabled={busy} onClick={() => void action(async () => { await proxyRequest('/v1/notifications', 'POST', value); setSaved(true); })}>{text('Save notifications', '保存通知设置')}</ActionButton>
      <ActionButton disabled={busy || !window.nexusDesktop} onClick={() => void action(() => invoke('notification_test'))}>{text('Send test notification', '发送测试通知')}</ActionButton>
      <ActionButton disabled={busy} onClick={() => void action(() => proxyRequest('/v1/profiles', 'POST', { action: 'open_terminal', target: 'notifications' }))}>{text('Open notification terminal', '打开通知终端')}</ActionButton>
      <p>{text('Terminal reminders require the notification terminal to stay open. Focus reporting depends on the terminal; unknown focus suppresses unfocused-only alerts. Ctrl+C stops monitoring. OS permissions and Do Not Disturb can suppress system banners.', '终端提醒需要保持通知终端打开。失焦检测依赖终端支持；焦点未知时不发送“仅失焦”提醒。Ctrl+C 停止监听。系统权限和勿扰设置可能阻止系统横幅。')}</p>
      {saved && <p role="status">{text('Saved', '已保存')}</p>}
    </>}
    {error && <p role="alert">{error}</p>}
  </Panel>;
}
