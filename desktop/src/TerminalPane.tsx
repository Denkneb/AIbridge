import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Terminal} from '@xterm/xterm';
import {FitAddon} from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import type {TerminalEvent} from './types';
declare const __DESKTOP_SMOKE__:boolean;

// Serialize IPC writes, including multi-byte paste, across the bounded Rust queue.
// A replacement session never receives input queued for its predecessor.
export function TerminalPane({project,attach}:{project:string;attach:{task:string;project:string;nonce:number}|null}){
 const host=useRef<HTMLDivElement>(null),term=useRef<Terminal|null>(null),fit=useRef<FitAddon|null>(null),session=useRef<string|null>(null),openGeneration=useRef(0);
 const[profile,setProfile]=useState('shell'),[binding,setBinding]=useState(''),[busy,setBusy]=useState(false),[error,setError]=useState(''),[fontSize,setFontSize]=useState(()=>Number(localStorage.getItem('aibridge-terminal-font'))||14);
 useEffect(()=>{
  const t=new Terminal({fontSize:14,fontFamily:'"DejaVu Sans Mono",monospace',scrollback:5000,theme:{background:'#10151c',foreground:'#d5dfed',cursor:'#70d9ae'},cursorBlink:true});
  const f=new FitAddon();t.loadAddon(f);t.open(host.current!);term.current=t;fit.current=f;f.fit();
  if(__DESKTOP_SMOKE__)Object.assign(host.current!,{smokeTerminal:t});
  let stopped=false,pending=0,chain=Promise.resolve();
  const delay=(ms:number)=>new Promise<void>(resolve=>setTimeout(resolve,ms));
  const data=t.onData(data=>{
   const id=session.current;if(!id)return;
   const bytes=new TextEncoder().encode(data);
   if(pending+bytes.length>1024*1024){setError('Слишком большой объём ввода: дождитесь завершения вставки');return;}
   pending+=bytes.length;
   chain=chain.then(async()=>{
    for(let offset=0;offset<bytes.length;offset+=4096){
     const deadline=Date.now()+2000;
     while(!stopped&&session.current===id){
      try{await invoke('terminal_write',{session:id,bytes:Array.from(bytes.subarray(offset,offset+4096))});break;}
      catch(e){if(String(e)!=='terminal input queue full'||Date.now()>=deadline)throw e;await delay(10);}
     }
     if(stopped||session.current!==id)return;
    }
   }).catch(e=>{if(!stopped&&session.current===id)setError(String(e));}).finally(()=>{pending-=bytes.length;});
  });
  // Preserve Ctrl+C for SIGINT. Copy uses the conventional terminal shortcut.
  t.attachCustomKeyEventHandler(e=>{
   if(e.type==='keydown'&&e.ctrlKey&&e.shiftKey&&e.code==='KeyC'){
    e.preventDefault();if(t.hasSelection())navigator.clipboard.writeText(t.getSelection()).catch(()=>setError('Буфер обмена недоступен'));return false;
   }
   if(e.type==='keydown'&&e.ctrlKey&&e.shiftKey&&e.code==='KeyV'){
    e.preventDefault();const id=session.current;navigator.clipboard.readText().then(text=>{if(id&&session.current===id)t.paste(text);}).catch(()=>setError('Буфер обмена недоступен'));return false;
   }
   return true;
  });
  const resize=new ResizeObserver(()=>{if(host.current?.clientWidth&&host.current?.clientHeight){f.fit();const id=session.current;if(id)invoke('terminal_resize',{session:id,rows:t.rows,cols:t.cols}).catch(e=>{if(session.current===id)setError(String(e));});}});resize.observe(host.current!);
  const poll=async()=>{
   if(stopped)return;
   try{
    const id=session.current;
    if(id){const events=await invoke<TerminalEvent[]>('terminal_read',{session:id});for(const e of events){
     if(stopped||session.current!==id)break;
     if(e.type==='data')await new Promise<void>(resolve=>t.write(new Uint8Array(e.bytes),resolve));
     else if(e.type==='exit'){t.writeln(`\r\n[процесс завершён: ${e.code}]`);session.current=null;setBinding(b=>b+' · завершена');invoke('terminal_close',{session:id}).catch(()=>{});}
     else setError(e.message);
    }}
   }catch(e){if(!stopped)setError(String(e));}
   if(!stopped)setTimeout(poll,32);
  };void poll();
  return()=>{stopped=true;openGeneration.current++;const id=session.current;session.current=null;if(id)invoke('terminal_close',{session:id}).catch(()=>{});resize.disconnect();data.dispose();t.dispose();term.current=null;};
 },[]);
 useEffect(()=>{if(term.current){term.current.options.fontSize=fontSize;fit.current?.fit();}localStorage.setItem('aibridge-terminal-font',String(fontSize));},[fontSize]);
 const connect=async(target=project,selected=profile,task:string|null=null)=>{
  const generation=++openGeneration.current;setBusy(true);setError('');
  try{
   if(session.current){const id=session.current;session.current=null;await invoke('terminal_close',{session:id});}
   if(generation!==openGeneration.current)return;
   const t=term.current;if(!t)return;t.reset();fit.current?.fit();
   const id=await invoke<string>('terminal_open',{project:target,profile:selected,task,rows:t.rows,cols:t.cols});
   if(generation!==openGeneration.current){await invoke('terminal_close',{session:id});return;}
   session.current=id;setBinding(`${target} · ${selected}${task?' · '+task.slice(0,8):''}`);t.focus();
  }catch(e){if(generation===openGeneration.current)setError(String(e));}finally{if(generation===openGeneration.current)setBusy(false);}
 };
 const close=()=>{openGeneration.current++;const id=session.current;session.current=null;setBusy(false);setBinding('');if(id)invoke('terminal_close',{session:id}).catch(e=>setError(String(e)));};
 useEffect(()=>{if(attach)void connect(attach.project,'attach',attach.task);},[attach]);
 return <section className="terminal-pane"><div className="pane-bar"><strong>Терминал</strong><span className="muted">{binding||'Нет сессии'}</span><select aria-label="Программа терминала" value={profile} onChange={e=>setProfile(e.target.value)}><option value="shell">Shell</option><option value="opencode">OpenCode</option><option value="codex">Codex</option></select><select aria-label="Размер шрифта терминала" value={fontSize} onChange={e=>setFontSize(Number(e.target.value))}>{[12,14,16,18,20].map(size=><option key={size} value={size}>{size}px</option>)}</select><button disabled={!project||busy} onClick={()=>void connect()}>{busy?'Запуск…':'Открыть'}</button><button disabled={!binding&&!busy} onClick={close}>Завершить</button><button disabled={!term.current} onClick={()=>{term.current?.clear();term.current?.focus();}}>Очистить</button></div>{error&&<p role="alert" className="error">{error}</p>}<div className="terminal-host" ref={host}/></section>;
}
