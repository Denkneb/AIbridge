import {useCallback,useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Terminal} from '@xterm/xterm';
import {FitAddon} from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import type {TerminalEvent} from './types';
declare const __DESKTOP_SMOKE__:boolean;

type TabStatus='starting'|'running'|'exited'|'error';
interface TerminalTab {id:string;project:string;profile:string;task:string|null;launchEnv:string|null;status:TabStatus}
const label=(tab:TerminalTab)=>`${tab.project} · ${tab.profile}${tab.task?' · '+tab.task.slice(0,8):''}`;

// Each mounted tab owns its PTY, output buffer and serialized input queue.
function TerminalSession({tab,visible,fontSize,clearCount,onStatus}:{tab:TerminalTab;visible:boolean;fontSize:number;clearCount:number;onStatus:(id:string,status:TabStatus)=>void}){
 const host=useRef<HTMLDivElement>(null),term=useRef<Terminal|null>(null),fit=useRef<FitAddon|null>(null),session=useRef<string|null>(null),openGeneration=useRef(0);
 const [error,setError]=useState('');
 const isVisible=useRef(visible);isVisible.current=visible;
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
    e.preventDefault();if(t.hasSelection())invoke('clipboard_write',{text:t.getSelection()}).catch(()=>setError('Буфер обмена недоступен'));return false;
   }
   if(e.type==='keydown'&&e.ctrlKey&&e.shiftKey&&e.code==='KeyV'){
    e.preventDefault();const id=session.current;invoke<string>('clipboard_read').then(text=>{if(id&&session.current===id)t.paste(text);}).catch(()=>setError('Буфер обмена недоступен'));return false;
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
     else if(e.type==='exit'){t.writeln(`\r\n[процесс завершён: ${e.code}]`);session.current=null;onStatus(tab.id,'exited');invoke('terminal_close',{session:id}).catch(()=>{});}
     else setError(e.message);
    }}
   }catch(e){if(!stopped)setError(String(e));}
   if(!stopped)setTimeout(poll,32);
  };void poll();
  return()=>{stopped=true;openGeneration.current++;const id=session.current;session.current=null;if(id)invoke('terminal_close',{session:id}).catch(()=>{});resize.disconnect();data.dispose();t.dispose();term.current=null;};
 },[]);
 useEffect(()=>{if(term.current){term.current.options.fontSize=fontSize;if(visible){fit.current?.fit();term.current.focus();}}},[fontSize,visible]);
 useEffect(()=>{if(clearCount&&visible){term.current?.clear();term.current?.focus();}},[clearCount]);
 useEffect(()=>{
  const generation=++openGeneration.current;
  const connect=async()=>{
   try{
    const t=term.current;if(!t)return;
    if(host.current?.clientWidth&&host.current?.clientHeight)fit.current?.fit();
    const id=await invoke<string>('terminal_open',{project:tab.project,profile:tab.profile,task:tab.task,rows:t.rows,cols:t.cols,launchEnv:tab.launchEnv});
    if(generation!==openGeneration.current){await invoke('terminal_close',{session:id});return;}
    session.current=id;onStatus(tab.id,'running');if(isVisible.current)t.focus();
   }catch(e){if(generation===openGeneration.current){setError(String(e));onStatus(tab.id,'error');}}
  };void connect();
 },[]);
 return <div className="terminal-session" hidden={!visible} role="tabpanel" id={`terminal-panel-${tab.id}`} aria-labelledby={`terminal-tab-${tab.id}`}>
  {error&&<p role="alert" className="error">{error}</p>}<div className="terminal-host" ref={host}/>
 </div>;
}

export function TerminalPane({project,attach}:{project:string;attach:{task:string;project:string;nonce:number}|null}){
 const [tabs,setTabs]=useState<TerminalTab[]>([]),[activeByProject,setActiveByProject]=useState<Record<string,string>>({}),[profiles,setProfiles]=useState<Record<string,string>>({}),[errors,setErrors]=useState<Record<string,string>>({}),[clearCount,setClearCount]=useState(0);
 const active=activeByProject[project]??'',profile=profiles[project]??'shell',error=errors[project]??'';
 const setError=(message:string,target=project)=>setErrors(items=>({...items,[target]:message}));
 const setActive=(id:string,target=project)=>setActiveByProject(items=>({...items,[target]:id}));
 const [fontSize,setFontSize]=useState(()=>Number(localStorage.getItem('aibridge-terminal-font'))||14);
 const [envDrafts,setEnvDrafts]=useState<Record<string,{text:string;saved:string}>>({}),[savingProjects,setSavingProjects]=useState<Record<string,boolean>>({});
 const envSaving=!!savingProjects[project];
 const launchEnv=envDrafts[project]?.text??'',envReady=!!envDrafts[project],envDirty=envReady&&launchEnv!==envDrafts[project].saved;
 const envDraftsRef=useRef(envDrafts);envDraftsRef.current=envDrafts;
 useEffect(()=>{
  if(!project||envDraftsRef.current[project])return;
  let cancelled=false;
  invoke<string>('codex_env_read',{project}).then(text=>{if(!cancelled)setEnvDrafts(items=>({...items,[project]:{text,saved:text}}));}).catch(e=>{if(!cancelled)setError(String(e),project);});
  return()=>{cancelled=true;};
 },[project]);
 const saveEnv=async(target=project)=>{
  const draft=envDraftsRef.current[target];if(!draft)throw Error('Переменные Codex ещё не загружены');
  setSavingProjects(items=>({...items,[target]:true}));
  try{await invoke('codex_env_save',{project:target,text:draft.text});setEnvDrafts(items=>({...items,[target]:{...items[target],saved:draft.text}}));return draft.text;}
  finally{setSavingProjects(items=>({...items,[target]:false}));}
 };
 const tabsRef=useRef(tabs);tabsRef.current=tabs;
 const onStatus=useCallback((id:string,status:TabStatus)=>setTabs(items=>items.map(tab=>tab.id===id?{...tab,status}:tab)),[]);
 useEffect(()=>{localStorage.setItem('aibridge-terminal-font',String(fontSize));},[fontSize]);
 const connect=async(target=project,selected=profile,task:string|null=null)=>{
  setError('',target);
  const existing=selected==='shell'?undefined:tabsRef.current.find(tab=>tab.project===target&&tab.profile===selected&&tab.task===task&&(tab.status==='starting'||tab.status==='running'));
  if(existing){setActive(existing.id,target);return;}
  if(tabsRef.current.length>=8){setError('Открыто 8 сессий во всех проектах: закройте ненужную перед запуском новой',target);return;}
  let variables:string|null=null;
  if(selected==='codex'){try{variables=await saveEnv(target);}catch(e){setError(String(e),target);return;}}
  // Saving crosses an async boundary: another click may have opened a tab.
  const opened=selected==='shell'?undefined:tabsRef.current.find(tab=>tab.project===target&&tab.profile===selected&&tab.task===task&&(tab.status==='starting'||tab.status==='running'));
  if(opened){setActive(opened.id,target);return;}
  if(tabsRef.current.length>=8){setError('Открыто 8 сессий во всех проектах: закройте ненужную перед запуском новой',target);return;}
  const tab:TerminalTab={id:crypto.randomUUID(),project:target,profile:selected,task,launchEnv:variables,status:'starting'};
  tabsRef.current=[...tabsRef.current,tab];setTabs(tabsRef.current);setActive(tab.id,target);
 };
 const close=(id=active)=>{
  const closing=tabsRef.current.find(tab=>tab.id===id);if(!closing)return;
  const siblings=tabsRef.current.filter(tab=>tab.project===closing.project);
  const index=siblings.findIndex(tab=>tab.id===id);
  const remaining=tabsRef.current.filter(tab=>tab.id!==id);
  const next=siblings.filter(tab=>tab.id!==id);
  tabsRef.current=remaining;setTabs(remaining);
  if(activeByProject[closing.project]===id)setActive(next[Math.min(index,next.length-1)]?.id??'',closing.project);
 };
 useEffect(()=>{if(attach)connect(attach.project,'attach',attach.task);},[attach]);
 const external=async()=>{
  setError('');try{const variables=profile==='codex'?await saveEnv():null;await invoke('terminal_external',{project,profile,task:null,launchEnv:variables});}catch(e){setError(String(e));}
 };
 const codexBlocked=profile==='codex'&&(!envReady||envSaving);
 const visibleTabs=tabs.filter(tab=>tab.project===project);
 const current=visibleTabs.find(tab=>tab.id===active);
 return <section className="terminal-pane"><div className="pane-bar"><strong>Терминал</strong><select aria-label="Программа терминала" value={profile} onChange={e=>setProfiles(items=>({...items,[project]:e.target.value}))}><option value="shell">Shell</option><option value="opencode">OpenCode</option><option value="codex">Codex</option></select><button disabled={!project||codexBlocked} onClick={()=>void connect()}>Открыть</button><details className="toolbar-menu"><summary aria-label="Дополнительные действия терминала" title="Дополнительные действия">⋯</summary><div className="toolbar-menu-content"><label>Размер шрифта<select aria-label="Размер шрифта терминала" value={fontSize} onChange={e=>setFontSize(Number(e.target.value))}>{[12,14,16,18,20].map(size=><option key={size} value={size}>{size}px</option>)}</select></label><button disabled={!project||codexBlocked} onClick={e=>{e.currentTarget.closest('details')?.removeAttribute('open');void external();}}>Во внешнем терминале</button><button disabled={!current} onClick={e=>{setClearCount(n=>n+1);e.currentTarget.closest('details')?.removeAttribute('open');}}>Очистить</button></div></details></div>
 {profile==='codex'&&<details className="terminal-launch-env"><summary>Переменные окружения Codex</summary><p>По одной переменной на строку: NAME=value или export NAME=value. Сохраняются отдельно для каждого проекта в приватном файле с правами 0600. Нажмите «Сохранить»; перед запуском Codex сохранение выполняется автоматически. Значения используются буквально: команды и подстановки $VAR не выполняются.</p><textarea aria-label="Переменные окружения Codex" spellCheck={false} placeholder={'export MY_VARIABLE=value\nOTHER_VARIABLE="значение с пробелами"'} value={launchEnv} disabled={!envReady||envSaving} onChange={e=>setEnvDrafts(items=>({...items,[project]:{...items[project],text:e.target.value}}))}/><button disabled={!envReady||envSaving||!envDirty} onClick={()=>{setError('');void saveEnv().catch(e=>setError(String(e)));}}>{envSaving?'Сохранение…':'Сохранить переменные'}</button><span className="muted">{!envReady?'Загрузка…':envDirty?'Есть несохранённые изменения':'Сохранено для проекта'}</span></details>}
 {error&&<p role="alert" className="error">{error}</p>}
 <div className="terminal-tabs" role="tablist" aria-label="Терминальные сессии">{visibleTabs.map(tab=><div className="terminal-tab" key={tab.id}>
  <button role="tab" id={`terminal-tab-${tab.id}`} aria-controls={`terminal-panel-${tab.id}`} aria-selected={active===tab.id} onClick={()=>setActive(tab.id)}>{label(tab)}{tab.status==='starting'?' · запуск':tab.status==='exited'?' · завершена':tab.status==='error'?' · ошибка':''}</button>
  <button aria-label={`Закрыть ${label(tab)}`} onClick={()=>close(tab.id)}>×</button>
 </div>)}</div>
 {tabs.map(tab=><TerminalSession key={tab.id} tab={tab} visible={tab.project===project&&active===tab.id} fontSize={fontSize} clearCount={clearCount} onStatus={onStatus}/>)}
 {!visibleTabs.length&&<p className="empty">Выберите программу и нажмите «Открыть»</p>}
 </section>;
}
