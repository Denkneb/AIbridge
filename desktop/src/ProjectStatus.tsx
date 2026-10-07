import {useEffect,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';

interface Health {ready:boolean;servers:Record<string,{ready:boolean;managed:boolean;process_record:string;idle?:boolean}>}
type Status='checking'|'running'|'idle'|'partial'|'stopped'|'unknown';
const labels:Record<Status,string>={checking:'Проверка состояния',running:'Запущен: OpenCode и MCP доступны',idle:'Запущен: основной OpenCode запускается по требованию; MCP доступен',partial:'Частично запущен: часть сервисов недоступна',stopped:'Не запущен',unknown:'Состояние недоступно'};

export function ProjectStatus({project,revision}:{project:string;revision:number}){
 const [status,setStatus]=useState<{project:string;value:Status}>({project:'',value:'checking'});
 useEffect(()=>{
  if(!project)return;
  let stopped=false,timer:ReturnType<typeof setTimeout>;
  setStatus({project,value:'checking'});
  const refresh=async()=>{
   try{
    const health=await invoke<Health>('lifecycle',{project,action:'doctor'});
    const servers=Object.values(health.servers);
    const value:Status=health.ready?(health.servers.opencode?.idle?'idle':'running'):servers.some(s=>s.ready||s.managed)?'partial':servers.some(s=>s.process_record==='invalid')?'unknown':'stopped';
    if(!stopped)setStatus({project,value});
   }catch{if(!stopped)setStatus({project,value:'unknown'});}
   finally{if(!stopped)timer=setTimeout(refresh,5000);}
  };
  void refresh();return()=>{stopped=true;clearTimeout(timer);};
 },[project,revision]);
 if(!project)return null;
 const value=status.project===project?status.value:'checking';
 const description=`Проект ${project}: ${labels[value]}`;
 return <span className={`project-indicator project-indicator-${value}`} role="status" aria-label={description} title={description}><span className="project-status-dot" aria-hidden="true"/></span>;
}
