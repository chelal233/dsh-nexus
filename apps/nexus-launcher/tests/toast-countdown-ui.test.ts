import assert from 'node:assert/strict';
import test from 'node:test';
import { JSDOM } from 'jsdom';
import { createUiTestLoader } from './ui-test-loader.ts';

test('toast countdown pauses for hover or focus and resumes only its remaining lifetime', async () => {
  const dom = new JSDOM('<div id="root"></div><button id="outside">outside</button>', {url:'http://localhost/'});
  let now = 0, nextId = 0;
  const intervals = new Map<number, Function>();
  dom.window.setInterval = ((fn: Function) => { const id = ++nextId; intervals.set(id, fn); return id; }) as any;
  dom.window.clearInterval = ((id: number) => intervals.delete(id)) as any;
  dom.window.setTimeout = (() => ++nextId) as any;
  dom.window.clearTimeout = () => {};
  const bindings = {window:dom.window,document:dom.window.document,navigator:dom.window.navigator,HTMLElement:dom.window.HTMLElement,performance:{now:()=>now},IS_REACT_ACT_ENVIRONMENT:true};
  const previous = new Map(Object.keys(bindings).map(key=>[key,Object.getOwnPropertyDescriptor(globalThis,key)]));
  for(const [key,value] of Object.entries(bindings)) Object.defineProperty(globalThis,key,{value,configurable:true,writable:true});
  let root, loader;
  try {
    const React=await import('react'); const {createRoot}=await import('react-dom/client');
    loader=await createUiTestLoader(); const {ToastNotice}=await loader.loadModule('/src/App.tsx');
    root=createRoot(document.getElementById('root')!);
    await React.act(async()=>root.render(React.createElement(ToastNotice,{message:'Done',kind:'success'})));
    const bar=()=>document.querySelector('.toast-countdown > div') as HTMLElement;
    const tick=async(time:number)=>{now=time;await React.act(async()=>{for(const fn of [...intervals.values()])fn();});};
    assert.equal(bar().style.transform,'scaleX(1)');
    await tick(2000); assert.equal(bar().style.transform,'scaleX(0.6)');
    const toast=document.querySelector('.toast')!;
    await React.act(async()=>toast.dispatchEvent(new dom.window.MouseEvent('mouseover',{bubbles:true})));
    await tick(3500); assert.equal(bar().style.transform,'scaleX(0.6)');
    await React.act(async()=>(toast.querySelector('button') as HTMLElement).focus());
    await React.act(async()=>toast.dispatchEvent(new dom.window.MouseEvent('mouseout',{bubbles:true,relatedTarget:document.body})));
    await tick(4500); assert.equal(bar().style.transform,'scaleX(0.6)');
    await React.act(async()=>document.getElementById('outside')!.focus());
    await tick(6500); assert.equal(bar().style.transform,'scaleX(0.2)');
    await tick(7500); assert.equal(document.querySelector('.toast'),null);
  } finally {
    if(root){const {act}=await import('react');await act(async()=>root.unmount());}
    await loader?.close();dom.window.close();
    for(const [key,value] of previous) value?Object.defineProperty(globalThis,key,value):delete globalThis[key];
  }
});
