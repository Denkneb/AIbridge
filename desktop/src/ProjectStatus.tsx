import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import type {Project} from './types';

interface Health {ready:boolean;servers:Record<string,{ready:boolean;managed:boolean;process_record:string;idle?:boolean;message?:string}>}
type Status='checking'|'running'|'idle'|'partial'|'stopped'|'unknown';
export interface ProjectHealthState {value:Status;message?:string}
const statusOrder:Record<Status,number>={running:0,idle:0,partial:1,stopped:2,checking:3,unknown:4};
const labels:Record<Status,string>={checking:'Проверка состояния',running:'Запущен: OpenCode и MCP доступны',idle:'Запущен: основной OpenCode запускается по требованию; MCP доступен',partial:'Частично запущен: часть сервисов недоступна',stopped:'Не запущен',unknown:'Состояние недоступно'};
const listLabels:Record<Status,string>={checking:'… проверка',running:'● активен',idle:'● активен',partial:'◐ частично запущен',stopped:'○ остановлен',unknown:'? статус недоступен'};

export function useProjectHealth(projects:Project[],revision:number){
 const [states,setStates]=useState<Record<string,ProjectHealthState>>({});
 const projectIds=projects.map(p=>p.id).join(',');
 useEffect(()=>{
  const ids=projectIds?projectIds.split(','):[];
  if(!ids.length){setStates({});return;}
  let stopped=false,timer:ReturnType<typeof setTimeout>;
  setStates(previous=>Object.fromEntries(ids.map(id=>[id,previous[id]??{value:'checking'}])));
  const check=async(project:string)=>{
   let next:ProjectHealthState;
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
 return states;
}

export function projectHealthLabel(state:ProjectHealthState|undefined){
 const current=state??{value:'checking'};
 return `${labels[current.value]}${current.message?' — '+current.message:''}`;
}

export function ProjectSelector({projects,selected,states,onSelect}:{projects:Project[];selected:string;states:Record<string,ProjectHealthState>;onSelect:(project:string)=>void}){
 const current=useRef(selected);current.current=selected;
 const sortedProjects=[...projects].sort((a,b)=>statusOrder[states[a.id]?.value??'checking']-statusOrder[states[b.id]?.value??'checking']||a.id.localeCompare(b.id,'ru',{numeric:true}));
 useEffect(()=>{
  const switchProject=(event:KeyboardEvent)=>{
   if(!event.shiftKey||event.ctrlKey||event.altKey||event.metaKey||event.isComposing||sortedProjects.length<2)return;
   const direction=event.key==='ArrowLeft'||event.key==='ArrowUp'?-1:event.key==='ArrowRight'||event.key==='ArrowDown'?1:0;
   if(!direction||document.querySelector('dialog[open],[role="dialog"]'))return;
   const target=event.target;
   if(target instanceof Element&&target.closest('input,textarea,[contenteditable]:not([contenteditable="false"])')&&!target.classList.contains('xterm-helper-textarea'))return;
   event.preventDefault();event.stopImmediatePropagation();
   const index=sortedProjects.findIndex(p=>p.id===current.current);
   const next=sortedProjects[index<0?(direction>0?0:sortedProjects.length-1):(index+direction+sortedProjects.length)%sortedProjects.length];
   current.current=next.id;onSelect(next.id);
  };
  // Capture before xterm so the shortcut never becomes terminal input.
  window.addEventListener('keydown',switchProject,true);
  return()=>window.removeEventListener('keydown',switchProject,true);
 },[projects,states,onSelect]);
 const status=states[selected]??{value:'checking'};
 const description=`Проект ${selected}: ${labels[status.value]}${status.message?' — '+status.message:''}`;
 return <label className="project-select" title={`${projects.find(p=>p.id===selected)?.workspace??''} · Shift+←/→: переключить проект`}>Проект<select aria-label="Проект" value={selected} onChange={e=>onSelect(e.target.value)}><option value="" disabled>Выберите проект</option>{sortedProjects.map(p=>{const state=states[p.id]??{value:'checking'};return <option key={p.id} value={p.id} title={labels[state.value]}>{p.id} · {listLabels[state.value]}</option>;})}</select>{selected&&<span className={`project-indicator project-indicator-${status.value}`} role="status" aria-label={description} title={description}><span className="project-status-dot" aria-hidden="true"/></span>}</label>;
}
