import {useEffect,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import type {Project} from './types';

interface Health {ready:boolean;servers:Record<string,{ready:boolean;managed:boolean;process_record:string;idle?:boolean;message?:string}>}
type Status='checking'|'running'|'idle'|'partial'|'stopped'|'unknown';
interface State {value:Status;message?:string}
const statusOrder:Record<Status,number>={running:0,idle:0,partial:1,stopped:2,checking:3,unknown:4};
const labels:Record<Status,string>={checking:'Проверка состояния',running:'Запущен: OpenCode и MCP доступны',idle:'Запущен: основной OpenCode запускается по требованию; MCP доступен',partial:'Частично запущен: часть сервисов недоступна',stopped:'Не запущен',unknown:'Состояние недоступно'};
const listLabels:Record<Status,string>={checking:'… проверка',running:'● активен',idle:'● активен',partial:'◐ частично запущен',stopped:'○ остановлен',unknown:'? статус недоступен'};

export function ProjectSelector({projects,selected,revision,onSelect}:{projects:Project[];selected:string;revision:number;onSelect:(project:string)=>void}){
 const [states,setStates]=useState<Record<string,State>>({});
 const projectIds=projects.map(p=>p.id).join(',');
 useEffect(()=>{
  const ids=projectIds?projectIds.split(','):[];
  if(!ids.length){setStates({});return;}
  let stopped=false,timer:ReturnType<typeof setTimeout>;
  setStates(previous=>Object.fromEntries(ids.map(id=>[id,previous[id]??{value:'checking'}])));
  const check=async(project:string)=>{
   let next:State;
   try{
    const health=await invoke<Health>('lifecycle',{project,action:'doctor'});
    const servers=Object.values(health.servers);
    const value:Status=health.ready?(health.servers.opencode?.idle?'idle':'running'):servers.some(s=>s.ready||s.managed)?'partial':servers.some(s=>s.process_record==='invalid')?'unknown':'stopped';
    next={value,message:health.servers.opencode?.message};
   }catch{next={value:'unknown'};}
   if(!stopped)setStates(previous=>({...previous,[project]:next}));
  };
  const refresh=async()=>{
   // Bound simultaneous diagnostics and wait for the sweep before polling again.
   for(let i=0;i<ids.length&&!stopped;i+=4)await Promise.all(ids.slice(i,i+4).map(check));
   if(!stopped)timer=setTimeout(refresh,5000);
  };
  void refresh();return()=>{stopped=true;clearTimeout(timer);};
 },[projectIds,revision]);
 const sortedProjects=[...projects].sort((a,b)=>statusOrder[states[a.id]?.value??'checking']-statusOrder[states[b.id]?.value??'checking']||a.id.localeCompare(b.id,'ru',{numeric:true}));
 const status=states[selected]??{value:'checking'};
 const description=`Проект ${selected}: ${labels[status.value]}${status.message?' — '+status.message:''}`;
 return <label className="project-select" title={projects.find(p=>p.id===selected)?.workspace}>Проект<select aria-label="Проект" value={selected} onChange={e=>onSelect(e.target.value)}><option value="" disabled>Выберите проект</option>{sortedProjects.map(p=>{const state=states[p.id]??{value:'checking'};return <option key={p.id} value={p.id} title={labels[state.value]}>{p.id} · {listLabels[state.value]}</option>;})}</select>{selected&&<span className={`project-indicator project-indicator-${status.value}`} role="status" aria-label={description} title={description}><span className="project-status-dot" aria-hidden="true"/></span>}</label>;
}
