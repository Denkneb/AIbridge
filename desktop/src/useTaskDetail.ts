import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import type {RoundPage,Task,TaskSummary} from './types';

export function useTaskDetail(summary:TaskSummary|undefined,refreshRevision:number){
 const[detail,setDetail]=useState<Task|null>(null),[loading,setLoading]=useState(false),[paging,setPaging]=useState(false),[error,setError]=useState(''),[retry,setRetry]=useState(0);
 const generation=useRef(0);
 const project=summary?.project_id,task=summary?.task_id,revision=summary?.revision;
 useEffect(()=>{
  const request=++generation.current;
  setDetail(null);setError('');setPaging(false);setLoading(Boolean(task));
  if(project&&task){
   invoke<Task>('task_detail',{project,task}).then(next=>{
    if(request===generation.current)setDetail(next);
   }).catch(e=>{if(request===generation.current)setError(String(e));})
    .finally(()=>{if(request===generation.current)setLoading(false);});
  }
  return()=>{++generation.current;};
 },[project,task,revision,refreshRevision,retry]);
 const current=detail?.task_id===task&&detail?.project_id===project?detail:null;
 const loadMore=async()=>{
  if(!current||current.next_before===null||paging)return;
  const request=generation.current,cursor=current.next_before;
  setPaging(true);setError('');
  try{
   const page=await invoke<RoundPage>('task_rounds',{project,task,before:cursor,expectedRevision:current.revision});
   if(request===generation.current)setDetail(d=>d&&d.revision===current.revision&&d.next_before===cursor?{...d,rounds:[...d.rounds,...page.rounds],next_before:page.next_before}:d);
  }catch(e){if(request===generation.current)setError(String(e));}
  finally{if(request===generation.current)setPaging(false);}
 };
 return {detail:current,loading,paging,error,loadMore,reload:()=>setRetry(r=>r+1)};
}
