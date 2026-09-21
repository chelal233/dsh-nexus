import test from 'node:test';
import assert from 'node:assert/strict';
import { renameDesktopState } from '../electron/desktop-startup-audit.mjs';

test('Windows state replacement retries transient sharing errors without deleting state',()=>{
  let attempts=0, waits=0;
  renameDesktopState('next','state',{platform:'win32',wait:()=>waits++,rename:(from,to)=>{
    assert.equal(from,'next');assert.equal(to,'state');
    if(attempts++<2) throw Object.assign(Error('busy'),{code:'EPERM'});
  }});
  assert.equal(attempts,3);assert.equal(waits,2);
});
test('state replacement has a fixed retry bound and preserves permanent errors',()=>{
  for(const [platform,code,limit] of [['win32','EACCES',11],['win32','ENOSPC',1],['linux','EPERM',1]]) {
    let attempts=0,waits=0;const error=Object.assign(Error(code),{code});
    assert.throws(()=>renameDesktopState('next','state',{platform,wait:()=>waits++,rename:()=>{attempts++;throw error;}}),e=>e===error);
    assert.equal(attempts,limit);assert.equal(waits,limit-1);
  }
});
