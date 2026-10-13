import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {IconButton} from './IconButton';
import {ProjectDiff} from './ProjectDiff';
import {ProjectGitMenu} from './ProjectGitMenu';
import type {ConsoleCheckout} from './ConsoleCheckoutPicker';

interface Branch {reference:string;name:string;remote:boolean}
export interface Branches {workspace:string;current:string|null;head:string|null;branches:Branch[];remotes:string[];upstream:{remote:string;destination:string}|null;dirty:number;upstream_saved?:boolean}
export function ProjectBranches({project,checkout,checkoutRunning=false}:{project:string;checkout?:ConsoleCheckout;checkoutRunning?:boolean}){
 const[view,setView]=useState<Branches|null>(null),[target,setTarget]=useState(''),[busy,setBusy]=useState(false),[error,setError]=useState(''),[readError,setReadError]=useState('');
 const active=useRef(true),switching=useRef(false),latest=useRef<Branches|null>(null),generation=useRef(0);
 const[pulling,setPulling]=useState(false),[message,setMessage]=useState('');
 const apply=(next:Branches)=>{const previous=latest.current;latest.current=next;setView(next);setTarget(t=>!previous||previous.current!==next.current||!next.branches.some(b=>b.reference===t)?next.current??'':t);};
 const refresh=async()=>{if(!project||switching.current)return;const request=++generation.current;try{const next=await invoke<Branches>('project_branches',{project,checkout});if(active.current&&request===generation.current){apply(next);setReadError('');}}catch(e){if(active.current&&request===generation.current)setReadError(String(e));}};
 useEffect(()=>{active.current=true;let stopped=false,timer:ReturnType<typeof setTimeout>;const poll=async()=>{await refresh();if(!stopped)timer=setTimeout(poll,5000);};void poll();return()=>{stopped=true;active.current=false;generation.current++;clearTimeout(timer);};},[project]);
 const change=async()=>{if(!view||!target)return;switching.current=true;generation.current++;setBusy(true);setError('');setMessage('');try{const next=await invoke<Branches>('project_branch_switch',{project,reference:target,expectedCurrent:view.current,expectedHead:view.head,expectedWorkspace:view.workspace});if(active.current)apply(next);}catch(e){if(active.current)setError(String(e));}finally{switching.current=false;if(active.current)setBusy(false);}};
 const pull=async()=>{
  if(!view?.upstream||switching.current||busy)return;
  switching.current=true;generation.current++;setBusy(true);setPulling(true);setError('');setMessage('');
  try{
   const next=await invoke<Branches>('project_git',{project,request:{workspace:view.workspace,current:view.current,head:view.head,operation:{action:'pull',remote:view.upstream.remote,destination:view.upstream.destination}},checkout});
   if(active.current){apply(next);setReadError('');setMessage(next.head===view.head?'Ветка уже актуальна':'Текущая ветка обновлена');}
  }catch(e){if(active.current)setError(String(e));}
  finally{switching.current=false;if(active.current){setBusy(false);setPulling(false);}void refresh();}
 };
 if(!project)return null;
 const current=view?.current?.replace(/^refs\/heads\//,'')??(view?.head?`HEAD ${view.head.slice(0,8)}`:'Нет ветки');
 return <div className="project-branches">{checkout&&<span title={checkout.workspace}>Codex</span>}<label title={`Текущая ветка: ${current}. Удалённые ветки показаны из локального Git; при переключении создаётся локальная tracking-ветка.`}>Ветка<select aria-label="Ветка Git проекта" disabled={!view||busy||!!checkout} value={target} onChange={e=>setTarget(e.target.value)}>
  {!view&&<option value="">{readError?'Git недоступен':'Загрузка…'}</option>}
  {view&&(!view.current||!view.branches.some(b=>b.reference===view.current))&&<option value={view.current??''} disabled>{current}</option>}
  {view&&<><optgroup label="Локальные">{view.branches.filter(b=>!b.remote).map(b=><option key={b.reference} value={b.reference}>{b.name}{b.reference===view.current?' · текущая':''}</option>)}</optgroup><optgroup label="Удалённые">{view.branches.filter(b=>b.remote).map(b=><option key={b.reference} value={b.reference}>{b.name}</option>)}</optgroup></>}
 </select></label>{!checkout&&view&&target&&target!==view.current&&<IconButton icon="switch" label={busy?'Переключение…':'Переключить'} disabled={busy} aria-busy={busy} title={busy?'Переключение…':`Переключить с ${current} на выбранную ветку`} onClick={()=>void change()}/>}
 <IconButton icon="pull" label={pulling?'Pull выполняется…':'Pull текущей ветки'} aria-busy={pulling} disabled={!view?.current||!view.head||!view.upstream||view.dirty>0||busy||checkoutRunning} title={checkoutRunning?'Завершите консоль Codex перед Pull':pulling?'Pull выполняется…':!view?.current?'Pull недоступен без текущей ветки':!view.upstream?'У текущей ветки нет upstream':view.dirty>0?'Сохраните изменения в коммите или stash перед Pull':`Pull текущей ветки ${current} из ${view.upstream.remote}/${view.upstream.destination}`} onClick={()=>void pull()}/>
 {pulling&&<span role="status">Pull…</span>}
 {message&&!error&&<span className="branch-message" role="status">{message}</span>}
 {view&&<ProjectGitMenu project={project} checkout={checkout} view={view} busy={busy} onResult={next=>{setMessage('');apply(next);}} onBusy={value=>{switching.current=value;generation.current++;setBusy(value);}}/>}
 <ProjectDiff project={project} checkout={checkout}/>
 {(error||readError)&&<details className="toolbar-menu branch-error" open><summary aria-label="Ошибка Git" title="Ошибка Git">!</summary><div className="toolbar-menu-content"><p role="alert">{error||readError}</p><button disabled={busy} onClick={()=>{setError('');void refresh();}}>Обновить список веток</button></div></details>}</div>;
}
