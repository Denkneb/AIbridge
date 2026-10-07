import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import type {Project} from './types';
interface Removal {review_id:string;before:Project;after:null}
export function ProjectRemoval({project,disabled,onDeleted}:{project:string;disabled:boolean;onDeleted:()=>void}){
 const[review,setReview]=useState<Removal|null>(null),[busy,setBusy]=useState(false),[error,setError]=useState('');
 const active=useRef(true),pending=useRef<string|null>(null),working=useRef(false);
 const cancel=()=>{const id=pending.current;pending.current=null;setReview(null);setError('');if(id)void invoke('project_cancel',{reviewId:id}).catch(()=>{});};
 useEffect(()=>{active.current=true;return()=>{active.current=false;if(pending.current)void invoke('project_cancel',{reviewId:pending.current}).catch(()=>{});};},[project]);
 const preview=async()=>{if(working.current)return;working.current=true;setBusy(true);setError('');try{const next=await invoke<Removal>('project_remove_preview',{project});if(active.current){pending.current=next.review_id;setReview(next);}else void invoke('project_cancel',{reviewId:next.review_id});}catch(e){if(active.current)setError(String(e));}finally{working.current=false;if(active.current)setBusy(false);}};
 const remove=async()=>{if(!review||working.current)return;working.current=true;setBusy(true);setError('');try{await invoke('project_apply',{reviewId:review.review_id});pending.current=null;if(active.current){setReview(null);onDeleted();}}catch(e){if(active.current)setError(String(e));}finally{working.current=false;if(active.current)setBusy(false);}};
 return <section className="project-removal" aria-label="Удаление проекта"><button className="danger" disabled={disabled||busy||!!review} onClick={()=>void preview()}>Удалить проект</button>{error&&<p className="error" role="alert">{error}</p>}{review&&<div className="review" role="dialog" aria-label="Подтверждение удаления проекта"><h3>Удалить проект «{review.before.id}» из AIbridge?</h3><p>Каталог: {review.before.workspace}</p><p>Проект исчезнет из списка настроенных. Рабочие файлы, история задач и credentials сохранятся. Конфигурация получит резервную копию.</p><p>Перед удалением остановите сервисы и автоматический запуск, закройте контроллеры и завершите незавершённые задачи.</p><button className="danger" disabled={disabled||busy} onClick={()=>void remove()}>{busy?'Удаление…':'Подтвердить удаление'}</button><button disabled={busy} onClick={cancel}>Отмена</button></div>}</section>;
}
