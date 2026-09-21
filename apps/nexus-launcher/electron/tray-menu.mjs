export async function cancelTrayStartup(bridge, operationId) {
  if (!operationId) return false;
  const current = await bridge.request('proxy_request', {method:'GET',path:'/v1/harness/startup'});
  if (current.operation_id !== operationId || !current.cancellable || current.cancel_requested) return false;
  await bridge.request('proxy_request', {method:'POST',path:'/v1/harness/startup',body:{action:'cancel',operation_id:operationId}});
  return true;
}

export function trayEntries({ text, web = {}, desktop = {}, supported = false, busy = false, canCancelStartup = false, canStopDesktop = false, act, show }) {
  const nativeActive = ['preparing', 'launched', 'stopping'].includes(desktop.phase);
  const webActive = ['running', 'starting', 'stopping'].includes(web.state) || !!web.pid;
  const desktopStatus = desktop.audit?.state === 'failed'
    ? text('Harness · Desktop startup failed', 'Harness · 桌面端启动失败')
    : desktop.audit?.state === 'ready'
    ? text('Harness · Desktop ready', 'Harness · 桌面端已就绪')
    : desktop.audit?.state === 'unverified'
    ? text('Harness · Desktop startup unverified', 'Harness · 桌面端启动未验证')
    : text('Harness · Desktop checking client', 'Harness · 桌面端检查客户端中');
  const status = nativeActive
    ? desktop.phase === 'launched' ? desktopStatus : text(desktop.phase === 'stopping' ? 'Harness · Desktop stopping' : 'Harness · Desktop preparing', desktop.phase === 'stopping' ? 'Harness · 桌面端正在停止' : 'Harness · 桌面端准备中')
    : web.startup_id ? text('Harness · Web checking startup', 'Harness · Web 启动检查中')
    : web.state === 'starting' ? text('Harness · Web starting', 'Harness · Web 启动中')
    : web.state === 'stopping' ? text('Harness · Web stopping', 'Harness · Web 正在停止')
    : web.state === 'failed' ? text('Harness · Web failed', 'Harness · Web 启动失败')
    : webActive ? web.ready ? text('Harness · Web ready', 'Harness · Web 已就绪') : text('Harness · Web not ready — open workbench', 'Harness · Web 未就绪，请查看工作台')
    : desktop.phase === 'failed' ? text('Harness · Desktop failed', 'Harness · 桌面端异常退出')
    : ['stopped', 'detached'].includes(web.state) ? text('Harness · Stopped', 'Harness · 已停止')
    : text('Harness · Status unavailable', 'Harness · 状态不可用');
  const item = (id,en,zh,enabled) => ({id,label:text(en,zh),enabled:!!enabled,click:()=>act(id)});
  return [
    {label: web.state || nativeActive || desktop.phase === 'failed' ? status : text('Harness · Status unavailable','Harness · 状态不可用'),enabled:false},
    {label:text('Show launcher','显示启动器'),click:show},
    {type:'separator'},
    {id:'web-group',label:text('Browser mode','浏览器版'),submenu:[
      item('start','Start Harness','启动 Harness',!busy&&!nativeActive&&!webActive&&web.start),
      item('web','Open page','打开网页',!busy&&!nativeActive&&web.state==='running'&&web.ready===true&&!web.startup_id&&web.web),
      item('restart','Restart Harness','重启 Harness',!busy&&!nativeActive&&web.state==='running'&&web.restart),
      item('stop','Stop Harness','中止 Harness',!busy&&!nativeActive&&['running','starting','failed'].includes(web.state)&&web.stop),
      ...(web.startup_id ? [item('cancel-startup','Cancel startup','取消启动',canCancelStartup&&!nativeActive)] : []),
    ]},
    ...(supported||nativeActive ? [{id:'desktop-group',label:text('Official Desktop','官方桌面版'),submenu:[
      item('desktop','Open Desktop','打开桌面端',!busy&&!nativeActive&&!webActive&&supported),
      item('desktop-restart','Restart Desktop','重启桌面端',!busy&&desktop.phase==='launched'&&!webActive&&supported),
      item('desktop-stop',desktop.phase==='preparing'?'Cancel startup':'Stop Desktop',desktop.phase==='preparing'?'取消启动':'中止桌面端',(!busy||canStopDesktop)&&nativeActive&&desktop.phase!=='stopping'),
    ]}] : []),
    {type:'separator'},
    item('profiles','Switch profile…','切换配置…',!busy&&!nativeActive),
    item('maintenance','Maintenance and diagnostics…','维护与诊断…',true),
    item('terminal','Open DSH terminal','打开 DSH 终端',!busy&&!nativeActive&&web.terminal),
    {type:'separator'},
    item('exit','Exit launcher (keep Harness running)','退出启动器（Harness 继续运行）',!busy),
    item('stop-exit','Stop all services and exit','中止所有服务并退出',!busy),
  ];
}

// Tracks only launches requested from the tray; old runs cannot report success.
export class TrayStartupFeedback {
  constructor(notify, now = Date.now) { this.notify = notify; this.now = now; }
  begin(mode, baseline = {}) {
    this.pending = {mode, baseline, started:this.now()};
    this.notify('starting', mode);
  }
  finish(state) {
    if (!this.pending) return;
    const {mode} = this.pending; this.pending = undefined;
    this.notify(state, mode);
  }
  observe({web = {}, startup = {}, desktop = {}}) {
    const p = this.pending; if (!p) return;
    if (p.mode === 'desktop') {
      if (desktop.operationId && desktop.operationId !== p.baseline.operationId) {
        if (desktop.audit?.state === 'ready') return this.finish('ready');
        if (desktop.phase === 'failed' || desktop.audit?.state === 'failed') return this.finish('failed');
        if (desktop.audit?.state === 'unverified') return this.finish('unverified');
        if (desktop.phase === 'stopped') return this.finish('cancelled');
      }
    } else {
      if (startup.operation_id && startup.operation_id !== p.baseline.operation_id) {
        if (startup.phase === 'failed') return this.finish('failed');
        if (startup.phase === 'cancelled') return this.finish('cancelled');
      }
      if (web.run_id && web.run_id !== p.baseline.run_id) {
        if (web.ready) return this.finish('ready');
        if (['blocked','limited'].includes(web.health) || web.state === 'failed') return this.finish('failed');
        if (web.reason?.startsWith('client_audit_')) return this.finish('unverified');
      }
    }
    if (this.now() - p.started >= 180000) this.finish('unverified');
  }
}
