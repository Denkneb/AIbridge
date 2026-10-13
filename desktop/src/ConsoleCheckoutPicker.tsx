import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import type {Branches} from './ProjectBranches';

export interface ConsoleCheckout {workspace:string;current:string|null}
interface CheckoutView extends ConsoleCheckout {kind:'main'|'codex'|'existing'|'task';available:boolean}
export interface ConsoleContext {checkout:ConsoleCheckout;running:boolean}
export function ConsoleCheckoutPicker({project,external,onSelect,onCancel}:{project:string;external:boolean;onSelect:(checkout:ConsoleCheckout)=>Promise<void>;onCancel:()=>void}){
 const dialog=useRef<HTMLDialogElement>(null),mounted=useRef(true),pending=useRef(false);
 const[checkouts,setCheckouts]=useState<CheckoutView[]>([]),[branches,setBranches]=useState<Branches|null>(null),[selected,setSelected]=useState(''),[mode,setMode]=useState('existing'),[createBranch,setCreateBranch]=useState(true),[branch,setBranch]=useState(()=>`codex/task-${crypto.randomUUID().slice(0,6)}`),[base,setBase]=useState('HEAD'),[busy,setBusy]=useState(false),[error,setError]=useState('');
 const load=async()=>{
  const [folders,view]=await Promise.all([invoke<CheckoutView[]>('console_checkouts',{project}),invoke<Branches>('project_branches',{project})]);
  if(!mounted.current)return;
  setCheckouts(folders);setBranches(view);setSelected(previous=>folders.some(folder=>folder.workspace===previous&&folder.available)?previous:folders.find(folder=>folder.kind==='main'&&folder.available)?.workspace??folders.find(folder=>folder.available)?.workspace??'');
 };
 useEffect(()=>{mounted.current=true;dialog.current?.showModal();void load().catch(e=>{if(mounted.current)setError(String(e));});return()=>{mounted.current=false;};},[]);
 const run=async()=>{
  if(pending.current||!branches)return;pending.current=true;setBusy(true);setError('');
  try{
   const checkout=mode==='new'?await invoke<ConsoleCheckout>('console_checkout_create',{project,branch:branch.trim(),base:createBranch?base:null,expectedWorkspace:branches.workspace}):checkouts.find(folder=>folder.workspace===selected&&folder.available);
   if(!checkout)throw Error('Выберите доступную рабочую папку');
   if(mounted.current){if(mode==='new'){await load();setSelected(checkout.workspace);setMode('existing');}await onSelect({workspace:checkout.workspace,current:checkout.current});}
  }catch(e){if(mounted.current)setError(String(e));}
  finally{pending.current=false;if(mounted.current)setBusy(false);}
 };
 const occupied=new Set(checkouts.map(folder=>folder.current));
 const free=branches?.branches.filter(item=>!item.remote&&!occupied.has(item.reference))??[];
 return <dialog ref={dialog} className="git-dialog console-checkout-dialog" aria-label="Рабочая папка Codex" onCancel={e=>{e.preventDefault();if(!busy)onCancel();}}>
  <div className="git-dialog-header"><strong>Codex без моста · {project}</strong><button disabled={busy} onClick={onCancel}>Закрыть</button></div>
  <p>Каждая рабочая папка имеет свои файлы и ветку. Изменения в основном каталоге не мешают созданию нового worktree.</p>
  <fieldset disabled={busy||!branches}><legend>Рабочая папка</legend>
   <label>Выбор<select aria-label="Режим рабочей папки Codex" value={mode} onChange={e=>setMode(e.target.value)}><option value="existing">Открыть существующую</option><option value="new">Создать worktree</option></select></label>
   {mode==='existing'?<><label>Каталог<select aria-label="Рабочая папка Codex" value={selected} onChange={e=>setSelected(e.target.value)}>
    {!checkouts.some(folder=>folder.available)&&<option value="">Нет доступных папок</option>}
    <optgroup label="Основной каталог и самостоятельные worktree">{checkouts.filter(folder=>folder.kind!=='task').map(folder=><option key={folder.workspace} value={folder.workspace} disabled={!folder.available}>{folder.kind==='main'?'Основной каталог':folder.kind==='codex'?'Codex':'Worktree'} · {folder.current?.replace(/^refs\/heads\//,'')??'detached HEAD'}{!folder.available?' · недоступен':''}</option>)}</optgroup>
    {!!checkouts.filter(folder=>folder.kind==='task').length&&<optgroup label="Служебные worktree задач · недоступны для Codex">{checkouts.filter(folder=>folder.kind==='task').map(folder=><option key={folder.workspace} value={folder.workspace} disabled>{folder.workspace}</option>)}</optgroup>}
   </select></label><p className="muted checkout-path">{selected}</p></>:<>
    <label>Ветка<select aria-label="Тип ветки Codex" value={createBranch?'new':'existing'} onChange={e=>{const next=e.target.value==='new';setCreateBranch(next);setBranch(next?`codex/task-${crypto.randomUUID().slice(0,6)}`:free[0]?.name??'');}}><option value="new">Создать новую ветку</option><option value="existing">Использовать существующую свободную ветку</option></select></label>
    {createBranch?<><label>Название ветки<input aria-label="Название ветки Codex" value={branch} onChange={e=>setBranch(e.target.value)} placeholder="codex/my-task"/></label><label>Создать от<select aria-label="Исходная ветка Codex" value={base} onChange={e=>setBase(e.target.value)}><option value="HEAD">Текущий HEAD основного каталога</option>{branches?.branches.map(item=><option key={item.reference} value={item.reference}>{item.name}</option>)}</select></label></>:<label>Свободная ветка<select aria-label="Существующая ветка Codex" value={branch} onChange={e=>setBranch(e.target.value)}>{!free.length&&<option value="">Нет свободных веток</option>}{free.map(item=><option key={item.reference} value={item.name}>{item.name}</option>)}</select></label>}
    <p className="muted">Worktree будет создан отдельно от основного каталога. После закрытия консоли папка и изменения сохранятся.</p>
   </>}
  </fieldset>
  {error&&<p role="alert" className="error">{error}</p>}{!branches&&!error&&<p role="status">Загрузка worktree…</p>}
  <div className="pane-bar"><button className="primary" disabled={busy||!branches||(mode==='new'?!branch.trim():!selected)} onClick={()=>void run()}>{busy?'Подготовка…':mode==='new'?'Создать и открыть Codex':external?'Открыть во внешнем терминале':'Открыть Codex'}</button><button disabled={busy} onClick={()=>void load().catch(e=>setError(String(e)))}>Обновить список</button><button disabled={busy} onClick={onCancel}>Отмена</button></div>
 </dialog>;
}
