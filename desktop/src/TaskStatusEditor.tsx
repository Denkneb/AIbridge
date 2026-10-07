import {useEffect,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {statuses,type Task} from './types';

export function TaskStatusEditor({task,onUpdated}:{task:Task;onUpdated:()=>void}){
 const[open,setOpen]=useState(false),[target,setTarget]=useState(task.status),[reason,setReason]=useState(''),[busy,setBusy]=useState(false),[error,setError]=useState('');
 useEffect(()=>{if(!busy)setTarget(task.status);},[task.status,busy]);
 if(['accepted','closed'].includes(task.status))return null;
 const save=async()=>{setBusy(true);setError('');try{await invoke('task_set_status',{project:task.project_id,task:task.task_id,expected:task.status,target,reason});setOpen(false);setReason('');onUpdated();}catch(e){setError(String(e));onUpdated();}finally{setBusy(false);}};
 return <div className="task-status-editor"><button disabled={busy} onClick={()=>setOpen(v=>!v)}>Изменить статус</button>{open&&<form onSubmit={e=>{e.preventDefault();void save();}}><label>Новый статус<select disabled={busy} value={target} onChange={e=>setTarget(e.target.value)}>{Object.entries(statuses).filter(([id])=>!['accepted','closed'].includes(id)).map(([id,label])=><option key={id} value={id}>{label}</option>)}</select></label><label>Причина изменения<textarea required maxLength={2048} disabled={busy} value={reason} onChange={e=>setReason(e.target.value)}/></label><p>Смена статуса записывается в историю. Результат и проверки сохраняются без изменений.</p><button disabled={busy||target===task.status||!reason.trim()} type="submit">{busy?'Сохранение…':'Сохранить статус'}</button>{error&&<p role="alert">{error}</p>}</form>}</div>;
}
