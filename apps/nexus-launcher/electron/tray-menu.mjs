export function trayEntries({ text, web = {}, desktop = {}, supported = false, busy = false, act, show }) {
  const nativeActive = ['preparing', 'launched', 'stopping'].includes(desktop.phase);
  const webActive = ['running', 'starting', 'stopping'].includes(web.state) || !!web.pid;
  const status = nativeActive
    ? text(desktop.phase === 'launched' ? 'Harness · Desktop running' : desktop.phase === 'stopping' ? 'Harness · Desktop stopping' : 'Harness · Desktop preparing', desktop.phase === 'launched' ? 'Harness · 桌面端运行中' : desktop.phase === 'stopping' ? 'Harness · 桌面端正在停止' : 'Harness · 桌面端准备中')
    : web.state === 'starting' ? text('Harness · Web starting', 'Harness · Web 启动中')
    : web.state === 'stopping' ? text('Harness · Web stopping', 'Harness · Web 正在停止')
    : web.state === 'failed' ? text('Harness · Web failed', 'Harness · Web 启动失败')
    : webActive ? text('Harness · Web active', 'Harness · Web 运行中')
    : desktop.phase === 'failed' ? text('Harness · Desktop failed', 'Harness · 桌面端异常退出')
    : ['stopped', 'detached'].includes(web.state) ? text('Harness · Stopped', 'Harness · 已停止')
    : text('Harness · Status unavailable', 'Harness · 状态不可用');
  const item = (id,en,zh,enabled) => ({id,label:text(en,zh),enabled:!!enabled,click:()=>act(id)});
  return [
    {label: web.state || nativeActive ? status : text('Harness · Status unavailable','Harness · 状态不可用'),enabled:false},
    {label:text('Show launcher','显示启动器'),click:show},
    {type:'separator'},
    {id:'web-group',label:text('Browser mode','浏览器版'),submenu:[
      item('start','Start Harness','启动 Harness',!busy&&!nativeActive&&!webActive&&web.start),
      item('web','Open page','打开网页',!busy&&!nativeActive&&web.state==='running'&&web.web),
      item('stop','Stop Harness','中止 Harness',!busy&&!nativeActive&&['running','starting','failed'].includes(web.state)&&web.stop),
    ]},
    ...(supported||nativeActive ? [{id:'desktop-group',label:text('Official Desktop','官方桌面版'),submenu:[
      item('desktop','Open Desktop','打开桌面端',!busy&&!nativeActive&&!webActive&&supported),
      item('desktop-stop','Stop Desktop','中止桌面端',!busy&&nativeActive&&desktop.phase!=='stopping'),
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
