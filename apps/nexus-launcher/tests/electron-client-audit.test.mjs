import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { ClientAudit, openVerifiedBrowser } from '../electron/client-audit.mjs';

function fixture() {
  let now = 0;
  const windows = [];
  const audit = new ClientAudit({ now: () => now, createWindow: options => {
    const window = new EventEmitter();
    window.options = options;
    window.webContents = new EventEmitter();
    window.webContents.session = { setPermissionRequestHandler: fn => { window.permission = fn; }, setPermissionCheckHandler: fn => { window.check = fn; } };
    window.webContents.setWindowOpenHandler = fn => { window.open = fn; };
    window.isDestroyed = () => !!window.destroyed;
    window.destroy = () => { window.destroyed = true; };
    window.loadURL = url => { window.url = url; return new Promise((_resolve, reject) => { window.reject = reject; }); };
    windows.push(window); return window;
  } });
  const info = { available: true, generation: 1, run_id: 'one', url: 'http://127.0.0.1:1234/?token=a', browser_health: { state: 'unverified' } };
  return { audit, windows, info, advance: ms => { now += ms; } };
}

test('loads only one real page per run and never treats HTTP load as activation', () => {
  const f = fixture();
  assert.equal(f.audit.observe(f.info).browser_health.state, 'checking');
  f.audit.observe(f.info);
  assert.equal(f.windows.length, 1);
  assert.equal(f.windows[0].url, f.info.url);
  assert.equal(f.windows[0].options.show, false);
  assert.equal(f.windows[0].options.webPreferences.preload, undefined);
  for (const state of ['active', 'limited', 'blocked']) {
    const info = { ...f.info, browser_health: { state, entries: [{ name: 'actual-plugin' }] } };
    assert.equal(f.audit.observe(info), info);
  }
  f.advance(45000);
  const result = f.audit.observe(f.info).browser_health;
  assert.equal(result.state, 'unverified');
  assert.equal(result.reason, 'client_audit_timeout');
  assert.equal(f.windows.length, 1);
  f.audit.stop(); assert.equal(f.windows[0].destroyed, true);
});

test('same-port same-token restarts replace the page; late failures cannot affect the new run', async () => {
  const f = fixture(); f.audit.observe(f.info);
  const next = { ...f.info, run_id: 'two' };
  f.audit.observe(next);
  assert.equal(f.windows[0].destroyed, true);
  assert.equal(f.windows.length, 2);
  f.windows[0].reject(Error('old load failed'));
  await Promise.resolve();
  assert.equal(f.audit.result(next).browser_health.state, 'checking');
  // A delayed UI response is read-only and must not roll back the observer.
  assert.equal(f.audit.result(f.info), f.info);
  f.audit.observe({ available: false });
  assert.equal(f.windows[1].destroyed, true);
  f.audit.stop(); f.audit.observe(next); assert.equal(f.windows.length, 2);
});

test('crashed pages fail visibly without a reload loop and untrusted navigation is rejected', () => {
  const f = fixture(); f.audit.observe(f.info);
  let prevented = false;
  f.windows[0].webContents.emit('will-redirect', { preventDefault() { prevented = true; } }, 'https://example.com/');
  assert.equal(prevented, true);
  assert.equal(f.windows[0].destroyed, true);
  assert.equal(f.audit.observe(f.info).browser_health.reason, 'client_audit_load_failed');
  assert.equal(f.windows.length, 1);
  assert.equal(f.windows[0].check(), false);
  let permission;
  f.windows[0].permission(null, 'notifications', allowed => { permission = allowed; });
  assert.equal(permission, false);
  assert.equal(f.windows[0].open().action, 'deny');
  f.audit.observe({ ...f.info, url: 'https://example.com/' });
  assert.equal(f.windows.length, 1);
});

test('deferred browser open waits for verified client, honors opt-out and happens once per run',async()=>{
 const f=fixture(), opened=[];f.audit.openReady=async(url,run)=>opened.push(run);
 for(const state of ['unverified','checking','blocked','limited']) f.audit.observe({...f.info,open_browser_after_ready:true,browser_health:{state}});
 await Promise.resolve();assert.deepEqual(opened,[]);
 f.audit.observe({...f.info,open_browser_after_ready:false,browser_health:{state:'active'}});await Promise.resolve();assert.deepEqual(opened,[]);
 const ready={...f.info,open_browser_after_ready:true,browser_health:{state:'active'}};
 f.audit.observe(ready);f.audit.observe(ready);await Promise.resolve();assert.deepEqual(opened,['one']);
 f.audit.clear();f.audit.observe(ready);await Promise.resolve();assert.deepEqual(opened,['one']);
 const restarted=fixture();restarted.audit.openReady=async()=>opened.push('duplicate');restarted.audit.openedRun='one';restarted.audit.observe(ready);await Promise.resolve();assert.deepEqual(opened,['one']);
 f.audit.observe({...ready,run_id:'two',generation:2,browser_health:{state:'limited'}});await Promise.resolve();assert.deepEqual(opened,['one']);
 f.audit.observe({...ready,run_id:'two',generation:2});await Promise.resolve();assert.deepEqual(opened,['one','two']);
});

test('manual browser open rechecks health', async()=>{
 const f=fixture(),opened=[];let current=f.info;
 const bridge={request:async()=>current};
 for(const state of [undefined,'unverified','checking','limited','blocked']) {
  current={...f.info,browser_health:{state}};
  await assert.rejects(openVerifiedBrowser(bridge,f.audit,url=>opened.push(url)));
 }
 assert.equal(opened.length,0);
 current={...f.info,browser_health:{state:'active'}};
 await openVerifiedBrowser(bridge,f.audit,url=>opened.push(url));
 assert.equal(opened.length,1);
 current={...current,available:false};
 await assert.rejects(openVerifiedBrowser(bridge,f.audit,url=>opened.push(url)));
 assert.equal(opened.length,1);
});
