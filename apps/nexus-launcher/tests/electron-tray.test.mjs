import test from 'node:test';
import assert from 'node:assert/strict';
import {trayEntries,cancelTrayStartup,TrayStartupFeedback} from '../electron/tray-menu.mjs';
const text=(en)=>en;
const menu=options=>trayEntries({text,act:()=>{},show:()=>{},...options});
const leaves=entries=>entries.flatMap(item=>item.submenu?leaves(item.submenu):[item]);
const by=(entries,id)=>leaves(entries).find(item=>item.id===id);

test('startup cancellation remains available during checks and binds to the displayed operation',async()=>{
 const entries=menu({busy:true,web:{state:'stopped',startup_id:'first'},canCancelStartup:true});
 assert.equal(by(entries,'cancel-startup').enabled,true);
 assert.match(entries[0].label,/Web checking startup/);
 assert.equal(by(entries,'start').enabled,false);
 assert.equal(by(menu({busy:true,desktop:{phase:'preparing'},canStopDesktop:true}),'desktop-stop').enabled,true);
 let current={operation_id:'second',cancellable:true}, writes=[];
 const bridge={request:async(_command,args)=>{if(args.method==='GET')return current;writes.push(args.body);return {};}};
 assert.equal(await cancelTrayStartup(bridge,'first'),false);
 assert.equal(writes.length,0,'an old menu must never cancel a newer operation');
 current={operation_id:'first',cancellable:false};
 assert.equal(await cancelTrayStartup(bridge,'first'),false);
 current={operation_id:'first',cancellable:true};
 assert.equal(await cancelTrayStartup(bridge,'first'),true);
 assert.deepEqual(writes,[{action:'cancel',operation_id:'first'}]);
});

test('Desktop tray reports client verification instead of treating a process as ready',()=>{
 for(const [state,expected] of [['checking',/checking/i],['ready',/ready/i],['failed',/failed/i],['unverified',/unverified/i]]) {
  const entries=menu({desktop:{phase:'launched',audit:{state}},supported:true});
  assert.match(entries[0].label,expected);
  assert.equal(by(entries,'desktop-stop').enabled,true);
  assert.equal(by(entries,'desktop').enabled,false);
 }
 assert.match(menu({desktop:{phase:'launched'}})[0].label,/checking/i);
 assert.match(menu({desktop:{phase:'failed'}})[0].label,/failed/i);
});
test('tray exposes only supported Desktop and separates Web actions',()=>{
 const idle={state:'stopped',start:true,stop:false,web:false,terminal:true};
 assert.equal(by(menu({web:idle}),'desktop'),undefined);
 let entries=menu({web:idle,supported:true});
 for(const id of ['start','desktop','terminal','profiles','maintenance','exit','stop-exit']) assert.equal(by(entries,id).enabled,true,id);
 assert.equal(by(entries,'desktop-stop').enabled,false);
 entries=menu({web:{state:'running',pid:20,stop:true,web:true,ready:true},supported:true});
 assert.equal(by(entries,'desktop').enabled,false);
 assert.equal(by(entries,'stop').enabled,true);
 assert.equal(by(entries,'web').enabled,true);
});
test('native preparation and stopping guard mutations; recovery remains reachable',()=>{
 for(const phase of ['preparing','launched','stopping']){
  const entries=menu({web:{state:'stopped',start:true,terminal:true},desktop:{phase},supported:true});
  for(const id of ['start','desktop','profiles','terminal'])assert.equal(by(entries,id).enabled,false,`${phase}:${id}`);
  assert.equal(by(entries,'desktop-stop').enabled,phase!=='stopping');
  assert.equal(by(entries,'maintenance').enabled,true);
 }
 const busy=menu({busy:true,supported:true});
 for(const entry of leaves(busy).filter(item=>item.id&&item.id!=='maintenance'))assert.equal(entry.enabled,false,entry.id);
 let called;
 by(trayEntries({text,web:{state:'running',stop:true},act:id=>called=id,show:()=>{}}),'stop').click();
 assert.equal(called,'stop');
});

test('fresh runtime overrides stale renderer actions and distinguishes unavailable status',()=>{
 for(const state of ['running','starting','stopping']) {
  const entries=menu({web:{state,start:true,web:true,ready:true},supported:true});
  assert.equal(by(entries,'start').enabled,false);
  assert.equal(by(entries,'desktop').enabled,false);
  assert.equal(by(entries,'web').enabled,state==='running');
 }
 assert.match(menu({web:{state:'unknown'}})[0].label,/unavailable/);
 assert.match(menu({web:{state:'failed'}})[0].label,/failed/);
 assert.equal(by(menu({web:{state:'stopped',web:true}}),'web').enabled,false);
 assert.equal(by(menu({web:{state:'stopped',stop:true}}),'stop').enabled,false);
 assert.equal(by(menu({web:{state:'stopping',stop:true}}),'stop').enabled,false);
 assert.equal(by(menu({web:{state:'running',stop:false}}),'stop').enabled,false,'runtime facts must not bypass the UI gate');
});

test('browser and official desktop controls occupy distinct submenus',()=>{
 const entries=menu({supported:true});
 assert.deepEqual(entries.find(item=>item.id==='web-group').submenu.map(item=>item.id),['start','web','restart','stop']);
 assert.deepEqual(entries.find(item=>item.id==='desktop-group').submenu.map(item=>item.id),['desktop','desktop-restart','desktop-stop']);
 assert.ok(entries.find(item=>item.id==='profiles'));
 assert.ok(!entries.find(item=>item.id==='start'));
});

test('Web restart requires both current runtime and workbench permission',()=>{
 assert.equal(by(menu({web:{state:'running',restart:true}}),'restart').enabled,true);
 for(const web of [{state:'running',restart:false},{state:'stopped',restart:true},{state:'starting',restart:true}])
  assert.equal(by(menu({web}),'restart').enabled,false);
 assert.equal(by(menu({web:{state:'running',restart:true},desktop:{phase:'launched'}}),'restart').enabled,false);
 assert.equal(by(menu({web:{state:'running',restart:true},busy:true}),'restart').enabled,false);
});

test('tray browser requires successful current check',()=>{
 for(const health of [undefined,'checking','unverified','blocked','limited']) {
  const entries=menu({web:{state:'running',pid:1,web:true,stop:true,health}});
  assert.equal(by(entries,'web').enabled,false);assert.equal(by(entries,'stop').enabled,true);
 }
 assert.equal(by(menu({web:{state:'running',web:true,ready:true,startup_id:'checking'}}),'web').enabled,false);
});
test('tray feedback deduplicates results and rejects old runs',()=>{
 let now=0;const notices=[];const feedback=new TrayStartupFeedback((...args)=>notices.push(args),()=>now);
 feedback.begin('web',{run_id:'old',operation_id:'old-check'});
 feedback.observe({web:{run_id:'old',ready:true}});
 assert.deepEqual(notices,[['starting','web']]);
 feedback.observe({web:{run_id:'new',health:'checking'}});
 feedback.observe({web:{run_id:'new',ready:true}});feedback.observe({web:{run_id:'new',ready:true}});
 assert.deepEqual(notices,[['starting','web'],['ready','web']]);
 feedback.begin('web',{run_id:'new'});feedback.observe({startup:{operation_id:'failed-check',phase:'failed'}});
 assert.deepEqual(notices.at(-1),['failed','web']);
 feedback.begin('desktop',{operationId:'old'});
 feedback.observe({desktop:{operationId:'old',audit:{state:'ready'}}});
 feedback.observe({desktop:{operationId:'new',audit:{state:'failed'}}});
 assert.deepEqual(notices.at(-1),['failed','desktop']);
 feedback.begin('web');now=180001;feedback.observe({});assert.deepEqual(notices.at(-1),['unverified','web']);
});
