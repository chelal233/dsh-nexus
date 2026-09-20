import test from 'node:test';
import assert from 'node:assert/strict';
import {trayEntries} from '../electron/tray-menu.mjs';
const text=(en)=>en;
const menu=options=>trayEntries({text,act:()=>{},show:()=>{},...options});
const leaves=entries=>entries.flatMap(item=>item.submenu?leaves(item.submenu):[item]);
const by=(entries,id)=>leaves(entries).find(item=>item.id===id);
test('tray exposes only supported Desktop and separates Web actions',()=>{
 const idle={state:'stopped',start:true,stop:false,web:false,terminal:true};
 assert.equal(by(menu({web:idle}),'desktop'),undefined);
 let entries=menu({web:idle,supported:true});
 for(const id of ['start','desktop','terminal','profiles','maintenance','exit','stop-exit']) assert.equal(by(entries,id).enabled,true,id);
 assert.equal(by(entries,'desktop-stop').enabled,false);
 entries=menu({web:{state:'running',pid:20,stop:true,web:true},supported:true});
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
  const entries=menu({web:{state,start:true,web:true},supported:true});
  assert.equal(by(entries,'start').enabled,false);
  assert.equal(by(entries,'desktop').enabled,false);
  assert.equal(by(entries,'web').enabled,state==='running');
 }
 assert.match(menu({web:{state:'unknown'}})[0].label,/unavailable/);
 assert.match(menu({web:{state:'failed'}})[0].label,/failed/);
 assert.equal(by(menu({web:{state:'stopped',web:true}}),'web').enabled,false);
});

test('browser and official desktop controls occupy distinct submenus',()=>{
 const entries=menu({supported:true});
 assert.deepEqual(entries.find(item=>item.id==='web-group').submenu.map(item=>item.id),['start','web','stop']);
 assert.deepEqual(entries.find(item=>item.id==='desktop-group').submenu.map(item=>item.id),['desktop','desktop-stop']);
 assert.ok(entries.find(item=>item.id==='profiles'));
 assert.ok(!entries.find(item=>item.id==='start'));
});
