import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {IconButton} from './IconButton';

interface Branch {reference:string;name:string;remote:boolean}
interface Branches {workspace:string;current:string|null;head:string|null;branches:Branch[]}
export function ProjectBranches({project}:{project:string}){
 const[view,setView]=useState<Branches|null>(null),[target,setTarget]=useState(''),[busy,setBusy]=useState(false),[error,setError]=useState(''),[readError,setReadError]=useState('');
 const active=useRef(true),switching=useRef(false),latest=useRef<Branches|null>(null),generation=useRef(0);
 const apply=(next:Branches)=>{const previous=latest.current;latest.current=next;setView(next);setTarget(t=>!previous||previous.current!==next.current||!next.branches.some(b=>b.reference===t)?next.current??'':t);};
 const refresh=async()=>{if(!project||switching.current)return;const request=++generation.current;try{const next=await invoke<Branches>('project_branches',{project});if(active.current&&request===generation.current){apply(next);setReadError('');}}catch(e){if(active.current&&request===generation.current)setReadError(String(e));}};
 useEffect(()=>{active.current=true;let stopped=false,timer:ReturnType<typeof setTimeout>;const poll=async()=>{await refresh();if(!stopped)timer=setTimeout(poll,5000);};void poll();return()=>{stopped=true;active.current=false;generation.current++;clearTimeout(timer);};},[project]);
 const change=async()=>{if(!view||!target)return;switching.current=true;generation.current++;setBusy(true);setError('');try{const next=await invoke<Branches>('project_branch_switch',{project,reference:target,expectedCurrent:view.current,expectedHead:view.head,expectedWorkspace:view.workspace});if(active.current)apply(next);}catch(e){if(active.current)setError(String(e));}finally{switching.current=false;if(active.current)setBusy(false);}};
 if(!project)return null;
 const current=view?.current?.replace(/^refs\/heads\//,'')??(view?.head?`HEAD ${view.head.slice(0,8)}`:'Нет ветки');
 return <div className="project-branches"><label title={`Текущая ветка: ${current}. Удалённые ветки показаны из локального Git; при переключении создаётся локальная tracking-ветка.`}>Ветка<select aria-label="Ветка Git проекта" disabled={!view||busy} value={target} onChange={e=>setTarget(e.target.value)}>
  {!view&&<option value="">{readError?'Git недоступен':'Загрузка…'}</option>}
  {view&&(!view.current||!view.branches.some(b=>b.reference===view.current))&&<option value={view.current??''} disabled>{current}</option>}
  {view&&<><optgroup label="Локальные">{view.branches.filter(b=>!b.remote).map(b=><option key={b.reference} value={b.reference}>{b.name}{b.reference===view.current?' · текущая':''}</option>)}</optgroup><optgroup label="Удалённые">{view.branches.filter(b=>b.remote).map(b=><option key={b.reference} value={b.reference}>{b.name}</option>)}</optgroup></>}
 </select></label>{view&&target&&target!==view.current&&<IconButton icon="switch" label={busy?'Переключение…':'Переключить'} disabled={busy} aria-busy={busy} title={busy?'Переключение…':`Переключить с ${current} на выбранную ветку`} onClick={()=>void change()}/>}
 {(error||readError)&&<details className="toolbar-menu branch-error" open><summary aria-label="Ошибка Git" title="Ошибка Git">!</summary><div className="toolbar-menu-content"><p role="alert">{error||readError}</p><button disabled={busy} onClick={()=>{setError('');void refresh();}}>Обновить список веток</button></div></details>}</div>;
}
