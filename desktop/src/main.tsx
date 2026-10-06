import {useEffect,useRef,useState} from 'react';
import {createRoot} from 'react-dom/client';
import {invoke} from '@tauri-apps/api/core';
import {Dashboard} from './Dashboard';
import {Settings} from './Settings';
import {TerminalPane} from './TerminalPane';
import type {Project,Task} from './types';
import './style.css';
function App(){
 const[projects,setProjects]=useState<Project[]>([]),[selected,setSelected]=useState(''),[tab,setTab]=useState('dashboard'),[error,setError]=useState(''),[busy,setBusy]=useState(false),[report,setReport]=useState(''),[attach,setAttach]=useState<{task:string;project:string;nonce:number}|null>(null),[split,setSplit]=useState(()=>Math.max(30,Math.min(75,Number(localStorage.getItem('aibridge-split'))||52))),[theme,setTheme]=useState(()=>localStorage.getItem('aibridge-theme')||'dark');const layout=useRef<HTMLDivElement>(null);
 const load=async()=>{try{const p=await invoke<Project[]>('projects');setProjects(p);setSelected(s=>p.some(p=>p.id===s)?s:(p[0]?.id??''));setError('');}catch(e){setError(String(e));}};
 useEffect(()=>{void load();},[]);useEffect(()=>{localStorage.setItem('aibridge-split',String(split));},[split]);useEffect(()=>{document.documentElement.dataset.theme=theme;localStorage.setItem('aibridge-theme',theme);},[theme]);
 const action=async(action:string)=>{setBusy(true);setError('');try{const result=await invoke('lifecycle',{project:selected,action});setReport(JSON.stringify(result,null,2));}catch(e){setError(String(e));}finally{setBusy(false);}};
 const drag=(e:React.PointerEvent<HTMLDivElement>)=>{e.currentTarget.setPointerCapture(e.pointerId);};const resize=(e:React.PointerEvent<HTMLDivElement>)=>{if(!e.currentTarget.hasPointerCapture(e.pointerId)||!layout.current)return;const box=layout.current.getBoundingClientRect();setSplit(Math.max(30,Math.min(75,(e.clientX-box.left)/box.width*100)));};
 const attachTask=(task:Task)=>setAttach({task:task.task_id,project:task.project_id,nonce:Date.now()});
 return <main><header><div className="brand"><b>AI<span>bridge</span></b><small>Рабочее пространство агента</small></div><label className="project-select">Проект<select value={selected} onChange={e=>setSelected(e.target.value)}><option value="" disabled>Выберите проект</option>{projects.map(p=><option key={p.id} value={p.id}>{p.id}</option>)}</select></label><nav aria-label="Разделы"><button aria-current={tab==='dashboard'?'page':undefined} onClick={()=>setTab('dashboard')}>Dashboard</button><button aria-current={tab==='settings'?'page':undefined} onClick={()=>setTab('settings')}>Настройки</button></nav><button aria-label="Переключить тему" onClick={()=>setTheme(t=>t==='dark'?'light':'dark')}>{theme==='dark'?'Светлая тема':'Тёмная тема'}</button></header>
 <div className="service-bar"><span className="muted">{projects.find(p=>p.id===selected)?.workspace??'Нет выбранного проекта'}</span>{['setup','doctor','start','stop'].map(a=><button disabled={!selected||busy} key={a} onClick={()=>void action(a)}>{{setup:'Настроить',doctor:'Проверить',start:'Запустить',stop:'Остановить'}[a]}</button>)}</div>{error&&<p role="alert" className="error">{error}</p>}{report&&<details className="service-report"><summary>Результат действия</summary><pre>{report}</pre></details>}
 <div className="workspace" ref={layout} style={{gridTemplateColumns:`${split}% 8px minmax(0,1fr)`}}><TerminalPane project={selected} attach={attach}/><div className="divider" role="separator" aria-label="Ширина панелей" aria-orientation="vertical" aria-valuenow={split} aria-valuemin={30} aria-valuemax={75} tabIndex={0} onPointerDown={drag} onPointerMove={resize} onKeyDown={e=>{if(e.key==='ArrowLeft'||e.key==='ArrowRight'){e.preventDefault();setSplit(s=>Math.max(30,Math.min(75,s+(e.key==='ArrowRight'?2:-2))));}}}/><div className="content">{tab==='dashboard'?<Dashboard project={selected} onAttach={attachTask}/>:<Settings project={projects.find(p=>p.id===selected)} projects={projects} onSaved={()=>void load()}/>}</div></div>
 <footer>AIbridge · Rust state изолирован · Закрытие окна завершает терминальные процессы</footer></main>;
}
createRoot(document.getElementById('root')!).render(<App/>);
