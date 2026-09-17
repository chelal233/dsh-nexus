import { useEffect, useRef, useState } from 'react';
import { Storefront } from '@phosphor-icons/react';
import { useI18n } from '../i18n';
import { proxyRequest } from '../agent-bridge';
import { errorMessage } from '../display-format';
import { Panel, ActionButton } from '../ui-components';

type MarketState = { profile: string; scope: string; provider: string; status: string; installed: boolean };
export function MarketplaceSettings() {
  const { locale } = useI18n();
  const text = (en: string, zh: string) => locale === 'zh' ? zh : en;
  const [state, setState] = useState<MarketState>();
  const [choice, setChoice] = useState('none');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);
  const pending = useRef(false);
  useEffect(() => {
    let live = true;
    void proxyRequest<MarketState>('/v1/market').then(value => {
      if (live) { setState(value); setChoice(value.provider); }
    }).catch(e => { if (live) setError(errorMessage(e)); });
    return () => { live = false; };
  }, []);
  async function save() {
    if (!state || pending.current) return;
    pending.current = true;
    setBusy(true); setError(''); setSaved(false);
    try {
      setState(await proxyRequest<MarketState>('/v1/market', 'POST', { profile: state.profile, scope: state.scope, provider: choice }));
      setSaved(true);
    } catch (e) {
      setError(errorMessage(e));
      try { setState(await proxyRequest<MarketState>('/v1/market')); } catch { /* Keep diagnostic and last known selection. */ }
    } finally { pending.current = false; setBusy(false); }
  }
  return <Panel title={text('Plugin marketplace', '插件市场')} icon={<Storefront size={18} />}>
    <p>{text('Choose a marketplace for this profile, or manage plugins yourself. Built-in Nexus notifications do not require a marketplace.', '为当前配置选择插件市场，也可以自行管理插件。Nexus 内置通知不依赖市场。')}</p>
    {state && <>
      <p>{text('Profile: ', '当前配置：')}{state.profile}</p>
      <label className="field-label">{text('Marketplace', '市场选择')}
        <select disabled={busy} value={choice} onChange={e => { setChoice(e.target.value); setSaved(false); }}>
          <option value="none">{text('No installation — manage plugins myself', '无需安装，由我自行管理')}</option>
          <option value="dsh-market">{text('dsh-market (third-party marketplace)', 'dsh-market（第三方插件市场）')}</option>
        </select>
      </label>
      <p>{text('Stop Harness before applying. Choosing dsh-market installs version 1.38.1 from npm into the current Harness profile. Start Harness afterward and open Settings → Plugin Market there.', '应用前请停止 Harness。选择 dsh-market 会从 npm 将 1.38.1 版安装到当前 Harness 配置。安装后启动 Harness，在其“设置 → 插件市场”中使用。')}</p>
      <p>{text('Self-managed does not uninstall an existing market or any plugins. Use plugin management to disable or remove them.', '自行管理不会卸载已有市场或其他插件。如需禁用或移除，请使用插件管理。')}</p>
      {state.status !== 'ready' && <p role="status">{text('The previous installation did not finish successfully. Apply again to retry, or use plugin recovery. No automatic installation runs at startup.', '上次安装未成功完成。可以重新应用以重试，或使用插件恢复。启动时不会自动安装。')}</p>}
      {state.installed && <p>{text('dsh-market is registered in this profile.', '此配置已注册 dsh-market。')}</p>}
      <ActionButton disabled={busy} onClick={() => void save()}>{busy ? text('Applying…', '正在应用…') : text('Apply marketplace selection', '应用市场选择')}</ActionButton>
      {saved && <p role="status">{text('Selection saved.', '已保存选择。')}</p>}
    </>}
    {error && <p role="alert">{error}</p>}
  </Panel>;
}
