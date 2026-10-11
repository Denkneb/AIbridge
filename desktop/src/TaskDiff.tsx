import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import type {TaskDiffList,TaskFileDiff} from './types';

interface Line {kind:'context'|'added'|'deleted'|'hunk'|'note';text:string;old:number|null;next:number|null}
function parsePatch(patch:string):Line[]{
 const rows:Line[]=[];let old=0,next=0,started=false;
 for(const raw of patch.split('\n')){
  const hunk=/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
  if(hunk){old=Number(hunk[1]);next=Number(hunk[2]);started=true;rows.push({kind:'hunk',text:raw,old:null,next:null});continue;}
  if(!started)continue;
  const prefix=raw[0];
  if(prefix==='+')rows.push({kind:'added',text:raw.slice(1),old:null,next:next++});
  else if(prefix==='-')rows.push({kind:'deleted',text:raw.slice(1),old:old++,next:null});
  else if(prefix===' ')rows.push({kind:'context',text:raw.slice(1),old:old++,next:next++});
  else if(prefix==='\\')rows.push({kind:'note',text:'Нет перевода строки в конце файла',old:null,next:null});
 }
 return rows;
}
const changes={added:'Добавлен',modified:'Изменён',deleted:'Удалён'};
export function TaskDiff({project,task,revision=''}:{project:string;task?:string;revision?:string}){
 const [scope,setScope]=useState('all');
 const [expanded,setExpanded]=useState(true),[repository,setRepository]=useState(0),[list,setList]=useState<TaskDiffList|null>(null),[repositories,setRepositories]=useState<TaskDiffList['repositories']>([]),[selected,setSelected]=useState(''),[file,setFile]=useState<TaskFileDiff|null>(null),[search,setSearch]=useState(''),[error,setError]=useState(''),[loading,setLoading]=useState(false),[fileLoading,setFileLoading]=useState(false),[refresh,setRefresh]=useState(0),[context,setContext]=useState(true),[scroll,setScroll]=useState(0);
 const listGeneration=useRef(0),fileGeneration=useRef(0),viewer=useRef<HTMLDivElement>(null);
 useEffect(()=>{
  const request=++listGeneration.current;
  setList(null);setFile(null);setError('');
  if(!expanded){setLoading(false);return;}
  setLoading(true);
  invoke<TaskDiffList>(task?'task_diff':'project_diff',task?{project,task,repository}:{project,scope}).then(next=>{
   if(request!==listGeneration.current)return;
   setList(next);setRepositories(next.repositories);setSelected(previous=>next.files.some(f=>f.path===previous)?previous:(next.files[0]?.path??''));
  }).catch(e=>{if(request===listGeneration.current)setError(String(e));}).finally(()=>{if(request===listGeneration.current)setLoading(false);});
  return()=>{++listGeneration.current;};
 },[project,task,revision,repository,refresh,expanded,scope]);
 useEffect(()=>{
  const request=++fileGeneration.current;setFile(null);setFileLoading(false);setError('');
  if(!list||!selected||!expanded)return;
  setFileLoading(true);
  invoke<TaskFileDiff>(task?'task_diff_file':'project_diff_file',task?{project,task,repository,path:selected,expectedRevision:list.revision}:{project,scope,path:selected,expectedRevision:list.revision}).then(next=>{
   if(request===fileGeneration.current){setFile(next);setError('');}
  }).catch(e=>{if(request===fileGeneration.current)setError(String(e));}).finally(()=>{if(request===fileGeneration.current)setFileLoading(false);});
  return()=>{++fileGeneration.current;};
 },[project,task,repository,list,selected,expanded,scope]);
 useEffect(()=>{setScroll(0);if(viewer.current)viewer.current.scrollTop=0;},[selected,file,context]);
 const currentFile=file?.path===selected?file:null;
 const parsed=useMemo(()=>parsePatch(currentFile?.patch??''),[currentFile]);
 const lines=context?parsed:parsed.filter(line=>line.kind!=='context');
 const filtered=(list?.files??[]).filter(f=>f.path.toLocaleLowerCase().includes(search.toLocaleLowerCase()));
 const index=filtered.findIndex(f=>f.path===selected),start=Math.max(0,Math.floor(scroll/24)-5);
 const longest=lines.reduce((max,line)=>Math.max(max,line.text.length),0);
 return <section className="task-diff" aria-label={task?"Изменения задачи":"Изменения проекта"}><div className="pane-bar"><h3>Изменения</h3>{!task&&<select aria-label="Режим diff проекта" value={scope} onChange={e=>{setScope(e.target.value);setSearch('');}}><option value="all">Все · HEAD → файлы</option><option value="staged">Staged · HEAD → индекс</option><option value="unstaged">Unstaged · индекс → файлы</option></select>}<button aria-expanded={expanded} onClick={()=>setExpanded(v=>!v)}>{expanded?'Свернуть diff':'Показать diff'}</button>{expanded&&<button disabled={loading} onClick={()=>setRefresh(n=>n+1)}>Обновить diff</button>}</div>{expanded&&<>
  <p className="muted diff-description">{task?'Текущие файлы относительно исходного снимка задачи.':'Незакоммиченные изменения проекта, включая правки Codex и ручные изменения. Новые файлы включены в «Все» и «Unstaged». Коммиты не показаны.'} Просмотр не меняет файлы. {list?.mode==='direct'?'В общем рабочем каталоге могут присутствовать и ручные изменения.':''}</p>
  {repositories.length>1&&<label className="diff-repository">Репозиторий<select aria-label="Репозиторий diff" value={repository} onChange={e=>{setRepository(Number(e.target.value));setSelected('');setSearch('');}}>{repositories.map(repo=><option key={repo.id} value={repo.id}>{repo.label}</option>)}</select></label>}
  {error&&<p role="alert" className="error">{error}</p>}{loading&&<p role="status" className="muted">Загрузка изменений…</p>}
  {list&&!list.total&&<p className="empty">{task?'Изменений относительно начала задачи нет':'В выбранном режиме изменений нет'}</p>}
  {!!list?.total&&<><p className="muted">Файлов: {list.total}{list.total>list.files.length?` · Показаны первые ${list.files.length}`:''}</p><div className="diff-layout"><aside className="diff-sidebar"><input aria-label="Поиск файлов diff" placeholder="Поиск файлов…" value={search} onChange={e=>setSearch(e.target.value)}/><div className="diff-files" role="list" aria-label="Изменённые файлы">{filtered.map(item=><button role="listitem" key={item.path} aria-current={selected===item.path?'true':undefined} title={`${item.path} · ${changes[item.change]}`} onClick={()=>setSelected(item.path)}><span className={`diff-change diff-change-${item.change}`} aria-label={changes[item.change]}>{item.change==='added'?'+':item.change==='deleted'?'−':'~'}</span><span>{item.path}</span></button>)}{!filtered.length&&<p className="muted">Файлы не найдены</p>}</div></aside><div className="diff-file"><div className="diff-toolbar"><strong title={selected}>{selected}</strong><button disabled={index<=0} onClick={()=>setSelected(filtered[index-1].path)} aria-label="Предыдущий файл diff">↑</button><button disabled={index<0||index>=filtered.length-1} onClick={()=>setSelected(filtered[index+1].path)} aria-label="Следующий файл diff">↓</button></div>
   {fileLoading&&<p role="status" className="muted">Загрузка файла…</p>}{currentFile&&currentFile.kind!=='text'&&<p role="status" className="diff-message">{currentFile.message}</p>}
   {currentFile?.kind==='text'&&<><div className="diff-toolbar"><span className="diff-count-added">+{parsed.filter(l=>l.kind==='added').length}</span><span className="diff-count-deleted">−{parsed.filter(l=>l.kind==='deleted').length}</span><label className="check"><input type="checkbox" checked={context} onChange={e=>setContext(e.target.checked)}/>Контекст</label>{currentFile.before_mode!==currentFile.after_mode&&<span className="muted">Режим: {currentFile.before_mode??'—'} → {currentFile.after_mode??'—'}</span>}</div>{currentFile.truncated&&<p className="diff-message" role="status">Diff сокращён до 512 КиБ. Показана часть изменений.</p>}{!parsed.length?<p className="diff-message">{currentFile.before_mode===null?'Добавлен пустой файл':currentFile.after_mode===null?'Удалён пустой файл':'Содержимое совпадает. Изменился тип файла или права доступа.'}</p>:<div className="diff-lines" ref={viewer} onScroll={e=>setScroll(e.currentTarget.scrollTop)} tabIndex={0} role="region" aria-label={`Diff файла ${selected}`}><div style={{height:lines.length*24,minWidth:`${Math.min(100000,Math.max(40,longest)+18)}ch`,position:'relative'}}>{lines.slice(start,start+35).map((line,i)=><div className={`diff-line diff-line-${line.kind}`} style={{position:'absolute',top:(start+i)*24,left:0,right:0,height:24}} key={start+i}><span className="diff-number">{line.old??''}</span><span className="diff-number">{line.next??''}</span><span className="diff-prefix">{line.kind==='added'?'+':line.kind==='deleted'?'−':''}</span><code>{line.text}</code></div>)}</div></div>}</>}
  </div></div></>}
 </>}</section>;
}
