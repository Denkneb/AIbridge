import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {IconButton} from './IconButton';
import type {Branches} from './ProjectBranches';

type Operation={action:'create';name:string;base:string;switch:boolean}|{action:'fetch';remote:string}|{action:'preview';remote:string;destination:string}|{action:'push';remote:string;destination:string;fingerprint:string};
interface Plan {current:string;head:string;remote:string;destination:string;ahead:number;behind:number;new_branch:boolean;set_upstream:boolean;fingerprint:string}
export function ProjectGitMenu({project,view,busy,onBusy,onResult}:{project:string;view:Branches;busy:boolean;onBusy:(value:boolean)=>void;onResult:(value:Branches)=>void}){
 const dialog=useRef<HTMLDialogElement>(null),mounted=useRef(true),running=useRef(false);
 const[open,setOpen]=useState(false),[name,setName]=useState(''),[base,setBase]=useState('HEAD'),[switchNow,setSwitchNow]=useState(true);
 const[remote,setRemote]=useState(''),[destination,setDestination]=useState(''),[plan,setPlan]=useState<Plan|null>(null),[approvedView,setApprovedView]=useState<Branches|null>(null),[error,setError]=useState(''),[message,setMessage]=useState('');
 useEffect(()=>{mounted.current=true;return()=>{mounted.current=false;};},[]);
 useEffect(()=>{if(open)dialog.current?.showModal();else dialog.current?.close();},[open]);
 const show=()=>{setRemote(view.upstream?.remote&&view.remotes.includes(view.upstream.remote)?view.upstream.remote:view.remotes.includes('origin')?'origin':view.remotes[0]??'');setDestination(view.upstream?.destination??view.current?.replace(/^refs\/heads\//,'')??'');setBase('HEAD');setName('');setPlan(null);setError('');setMessage('');setOpen(true);};
 const run=async(operation:Operation)=>{
  if(running.current||busy)return;
  running.current=true;onBusy(true);setError('');setMessage('');
  const expected=operation.action==='push'?approvedView: view;
  if(!expected){running.current=false;onBusy(false);return;}
  try{
   const result=await invoke<Branches|Plan>('project_git',{project,request:{workspace:expected.workspace,current:expected.current,head:expected.head,operation}});
   if(!mounted.current)return;
   if(operation.action==='preview'){setPlan(result as Plan);setApprovedView(expected);}
   else {const next=result as Branches;onResult(next);if(operation.action==='create'){setName('');setBase('HEAD');if(operation.switch)setDestination(next.current?.replace(/^refs\/heads\//,'')??'');}setPlan(null);setMessage(operation.action==='fetch'?'Удалённые ветки обновлены':operation.action==='create'?'Ветка создана':(result as Branches).upstream_saved===false?'Коммиты отправлены, но upstream сохранить не удалось. Настройте его через Git.':'Коммиты отправлены');}
  }catch(e){if(mounted.current){setError(String(e));if(operation.action==='push')setPlan(null);}}
  finally{running.current=false;if(mounted.current)onBusy(false);}
 };
 return <><IconButton icon="git" label="Git" title="Создание ветки, Fetch и Push" disabled={busy} onClick={show}/>
 <dialog ref={dialog} className="git-dialog" aria-labelledby={`git-title-${project}`} onCancel={e=>{if(running.current)e.preventDefault();else setOpen(false);}} onClose={()=>setOpen(false)}>
  <div className="git-dialog-header"><strong id={`git-title-${project}`}>Git · {view.current?.replace(/^refs\/heads\//,'')??'Detached HEAD'}</strong><button type="button" disabled={busy} onClick={()=>setOpen(false)}>Закрыть</button></div>
  <p>Изменённых файлов: {view.dirty}. Push отправляет только коммиты.</p>
  <fieldset disabled={busy||!!plan}><legend>Создать ветку</legend>
   <label>Имя ветки<input aria-label="Имя новой ветки" value={name} onChange={e=>setName(e.target.value)} placeholder="feature/new-task"/></label>
   <label>Создать от<select aria-label="Исходная ветка" value={base} onChange={e=>setBase(e.target.value)}><option value="HEAD">Текущий HEAD</option>{view.branches.map(b=><option key={b.reference} value={b.reference}>{b.name}</option>)}</select></label>
   <label className="git-checkbox"><input type="checkbox" checked={switchNow} onChange={e=>setSwitchNow(e.target.checked)}/>Сразу переключиться</label>
   {switchNow&&view.dirty>0&&<p>Для переключения сначала сохраните изменения в коммите или stash.</p>}
   <button type="button" disabled={!name.trim()||!view.head||(switchNow&&view.dirty>0)} onClick={()=>void run({action:'create',name:name.trim(),base,switch:switchNow})}>Создать ветку</button>
  </fieldset>
  <fieldset disabled={busy||!!plan}><legend>Удалённый репозиторий</legend>
   <label>Remote<select aria-label="Git remote" value={remote} onChange={e=>setRemote(e.target.value)}>{!view.remotes.length&&<option value="">Нет remote</option>}{view.remotes.map(r=><option key={r} value={r}>{r}</option>)}</select></label>
   {!view.remotes.length&&<p>Добавьте remote через Git в консоли проекта.</p>}
   <button type="button" disabled={!remote} onClick={()=>void run({action:'fetch',remote})}>Fetch</button>
   <label>Ветка назначения<input aria-label="Ветка назначения push" value={destination} onChange={e=>setDestination(e.target.value)}/></label>
   <button type="button" disabled={!remote||!destination.trim()||!view.current||!view.head} onClick={()=>void run({action:'preview',remote,destination:destination.trim()})}>Просмотреть push</button>
  </fieldset>
  {plan&&<section className="git-push-preview" aria-label="Параметры push"><strong>Push: {plan.remote} → {plan.destination}</strong><p>{plan.current.replace(/^refs\/heads\//,'')} · {plan.head.slice(0,8)}</p><p>Коммитов к отправке: {plan.ahead}. Отставание: {plan.behind}.</p>{plan.new_branch&&<p>На remote будет создана новая ветка.</p>}{plan.set_upstream&&<p>Для текущей ветки будет настроен upstream.</p>}{plan.behind>0&&<p role="alert">Выполните Fetch и объедините удалённые изменения перед отправкой.</p>}
   <button type="button" disabled={busy||plan.behind>0||(!plan.new_branch&&!plan.ahead&&!plan.set_upstream)} onClick={()=>void run({action:'push',remote:plan.remote,destination:plan.destination,fingerprint:plan.fingerprint})}>Отправить</button> <button type="button" disabled={busy} onClick={()=>setPlan(null)}>Отмена</button>
  </section>}
  {busy&&<p role="status">Выполняется операция Git…</p>}{message&&<p role="status">{message}</p>}{error&&<p role="alert">{error}</p>}
 </dialog></>;
}
