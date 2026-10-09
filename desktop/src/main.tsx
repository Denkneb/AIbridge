import {useEffect,useRef,useState} from 'react';
import {createRoot} from 'react-dom/client';
import {invoke} from '@tauri-apps/api/core';
import {Dashboard} from './Dashboard';
import {Automation} from './Automation';
import {Settings} from './Settings';
import {TerminalPane} from './TerminalPane';
import {ProjectSelector} from './ProjectStatus';
import {ProjectBranches} from './ProjectBranches';
import {IconButton} from './IconButton';
import type {Project,Task} from './types';
import './style.css';
function App(){
 const[projects,setProjects]=useState<Project[]>([]),[selected,setSelected]=useState(''),[tab,setTab]=useState('dashboard'),[error,setError]=useState(''),[busy,setBusy]=useState(false),[report,setReport]=useState(''),[serviceRevision,setServiceRevision]=useState(0),[attach,setAttach]=useState<{task:string;project:string;nonce:number}|null>(null),[split,setSplit]=useState(()=>Math.max(30,Math.min(75,Number(localStorage.getItem('aibridge-split'))||52))),[theme,setTheme]=useState(()=>localStorage.getItem('aibridge-theme')||'dark');const layout=useRef<HTMLDivElement>(null);
 const selectedRef=useRef(selected);selectedRef.current=selected;
 useEffect(()=>{setError('');setReport('');},[selected]);
 const load=async()=>{try{const p=await invoke<Project[]>('projects');setProjects(p);setSelected(s=>p.some(p=>p.id===s)?s:(p[0]?.id??''));setError('');}catch(e){setError(String(e));}};
 useEffect(()=>{void load();},[]);useEffect(()=>{localStorage.setItem('aibridge-split',String(split));},[split]);useEffect(()=>{document.documentElement.dataset.theme=theme;localStorage.setItem('aibridge-theme',theme);},[theme]);
 const action=async(action:string)=>{const target=selected;setBusy(true);setError('');try{if(action==='start')await invoke('lifecycle',{project:target,action:'setup'});const result=await invoke('lifecycle',{project:target,action});if(selectedRef.current===target)setReport(JSON.stringify(result,null,2));}catch(e){if(selectedRef.current===target)setError(String(e));}finally{setBusy(false);setServiceRevision(n=>n+1);}};
 const drag=(e:React.PointerEvent<HTMLDivElement>)=>{e.currentTarget.setPointerCapture(e.pointerId);};const resize=(e:React.PointerEvent<HTMLDivElement>)=>{if(!e.currentTarget.hasPointerCapture(e.pointerId)||!layout.current)return;const box=layout.current.getBoundingClientRect();setSplit(Math.max(30,Math.min(75,(e.clientX-box.left)/box.width*100)));};
 const attachTask=(task:Task)=>{setSelected(task.project_id);setAttach({task:task.task_id,project:task.project_id,nonce:Date.now()});};
 return <main><header><div className="brand"><b>AI<span>bridge</span></b></div><ProjectSelector projects={projects} selected={selected} revision={serviceRevision} onSelect={setSelected}/><ProjectBranches key={selected} project={selected}/><nav aria-label="Разделы проекта"><IconButton icon="dashboard" label="Dashboard" aria-current={tab==='dashboard'?'page':undefined} onClick={()=>setTab('dashboard')}/><IconButton icon="automation" label="Автоматизация" aria-current={tab==='automation'?'page':undefined} onClick={()=>setTab('automation')}/><IconButton icon="settings" label="Настройки" aria-current={tab==='settings'?'page':undefined} onClick={()=>setTab('settings')}/></nav><div className="project-actions" role="group" aria-label="Сервисы проекта">{(['doctor','start','stop'] as const).map(a=><IconButton icon={{doctor:'check' as const,start:'play' as const,stop:'stop' as const}[a]} label={{doctor:'Проверить',start:'Запустить',stop:'Остановить'}[a]} disabled={!selected||busy} key={a} onClick={()=>void action(a)}/>)}</div><IconButton className="theme-toggle" icon={theme==='dark'?'sun':'moon'} label={theme==='dark'?'Светлая тема':'Тёмная тема'} onClick={()=>setTheme(t=>t==='dark'?'light':'dark')}/></header>
 {error&&<p role="alert" className="error">{error}</p>}{report&&<details className="service-report"><summary>Результат действия</summary><pre>{report}</pre></details>}
 <div className="workspace" ref={layout} style={{gridTemplateColumns:`${split}% 8px minmax(0,1fr)`}}><TerminalPane project={selected} attach={attach}/><div className="divider" role="separator" aria-label="Ширина панелей" aria-orientation="vertical" aria-valuenow={split} aria-valuemin={30} aria-valuemax={75} tabIndex={0} onPointerDown={drag} onPointerMove={resize} onKeyDown={e=>{if(e.key==='ArrowLeft'||e.key==='ArrowRight'){e.preventDefault();setSplit(s=>Math.max(30,Math.min(75,s+(e.key==='ArrowRight'?2:-2))));}}}/><div className="content">{tab==='dashboard'?<Dashboard key={selected} project={selected} onAttach={attachTask}/>:tab==='automation'?<Automation key={selected} project={selected}/>:<Settings key={selected} project={projects.find(p=>p.id===selected)} projects={projects} onSaved={()=>void load()}/>}</div></div>
 <footer>AIbridge · Rust state изолирован · Закрытие окна завершает терминальные процессы</footer></main>;
}
createRoot(document.getElementById('root')!).render(<App/>);
