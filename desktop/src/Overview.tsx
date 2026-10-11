import {useEffect,useState} from 'react';
import {projectHealthLabel,type ProjectHealthState} from './ProjectStatus';
import {invoke} from '@tauri-apps/api/core';
import {statuses,type Project,type ProjectOverview,type TaskSummary} from './types';

interface State {snapshot?:ProjectOverview;updated?:number;error?:string}
const phaseLabels:Record<string,string>={prepare:'Подготовка',submit:'Отправка',active:'Выполнение и проверка',revise:'Доработка',accept:'Приёмка',accepted:'Принята',deliver:'Доставка'};
const runProgress=(run:Record<string,unknown>)=>{const steps=Array.isArray(run.steps)?run.steps as {phase:string}[]:[];const accepted=steps.filter(s=>s.phase==='accepted').length;const current=steps.find(s=>s.phase!=='accepted');return steps.length?` · Выполнено ${accepted}/${steps.length}${current?' · '+(phaseLabels[current.phase]??current.phase):''}`:'';};
const runLabels:Record<string,string>={running:'Выполняется',ready:'Готов к доставке',completed:'Завершён',blocked:'Заблокирован',stopped:'Остановлен',failed:'Ошибка',paused:'Приостановлен'};
export function Overview({projects,health,revision,onTask,onProject,onAutomation,onAttach}:{projects:Project[];health:Record<string,ProjectHealthState>;revision:number;onTask:(task:TaskSummary)=>void;onProject:(project:string)=>void;onAutomation:(project:string)=>void;onAttach:(task:TaskSummary)=>void}){
 const [states,setStates]=useState<Record<string,State>>({}),[refresh,setRefresh]=useState(0);
 const ids=JSON.stringify(projects.map(p=>p.id));
 useEffect(()=>{
  let stopped=false;let timer:ReturnType<typeof setTimeout>;
  const projectIds=JSON.parse(ids) as string[];
  const stamps=new Map<string,string>(),lastFull=new Map<string,number>();
  setStates({});
  const sweep=async()=>{
   let index=0;
   const worker=async()=>{
    while(!stopped&&index<projectIds.length){
     const project=projectIds[index++];
     try{
      const stamp=await invoke<string>('dashboard_revision',{project,linked:false});
      if(stopped)return;
      if(stamps.get(project)===stamp&&Date.now()-(lastFull.get(project)??0)<30000)continue;
      const snapshot=await invoke<ProjectOverview>('project_overview',{project});
      if(stopped)return;
      stamps.set(project,stamp);lastFull.set(project,Date.now());
      setStates(previous=>({...previous,[project]:{snapshot,updated:Date.now()}}));
     }catch{
      if(stopped)return;
      stamps.delete(project);
      setStates(previous=>({...previous,[project]:{...previous[project],error:'Не удалось обновить проект. Проверьте состояние и соединение.'}}));
     }
    }
   };
   await Promise.all(Array.from({length:Math.min(4,projectIds.length)},worker));
   if(!stopped)timer=setTimeout(sweep,5000);
  };
  void sweep();return()=>{stopped=true;clearTimeout(timer);};
 },[ids,revision,refresh]);
 const rows=projects.map(project=>({project,state:states[project.id]}));
 const loaded=rows.filter(({state})=>state?.snapshot&&!state.error);
 const active=loaded.reduce((sum,{state})=>sum+(state?.snapshot?.active_count??0),0);
 const attention=loaded.reduce((sum,{state})=>sum+(state?.snapshot?.attention_count??0),0);
 const runNeedsAttention=(run:Record<string,unknown>)=>['ready','blocked','failed'].includes(String(run.status));
 const taskRow=(task:TaskSummary)=><div className="overview-task" key={`${task.project_id}:${task.task_id}`}><button className="overview-task-link" onClick={()=>onTask(task)}><strong>{task.title}</strong><small>{task.project_id} · {task.task_id.slice(0,8)}</small></button><span className={`status status-${task.status}`}>{statuses[task.status]??task.status}</span>{!['failed','accepted','closed'].includes(task.status)&&<button onClick={()=>onAttach(task)} aria-label={`Открыть сессию ${task.project_id} ${task.task_id}`}>Сессия</button>}</div>;
 return <section className="overview" aria-label="Обзор всех проектов"><div className="pane-bar"><h2>Обзор проектов</h2><button onClick={()=>setRefresh(n=>n+1)}>Обновить</button></div>{!projects.length?<p className="empty">Добавьте проект в настройках</p>:<>
  <p className="overview-summary" role="status">Получены данные: {loaded.length} из {projects.length} проектов · Активных задач: {active} · Задач требуют внимания: {attention}</p>
  <section className="overview-attention" aria-label="Требует внимания"><h3>Требует внимания</h3>{rows.flatMap(({project,state})=>state?.snapshot&&!state.error?[...state.snapshot.attention.map(taskRow),...state.snapshot.runs.filter(runNeedsAttention).map(run=><button className="overview-run-link" key={`${project.id}:${String(run.run_id)}`} onClick={()=>onAutomation(project.id)}>{project.id} · Автоматизация: {runLabels[String(run.status)]??String(run.status)}</button>),...(state.snapshot.attention_count>state.snapshot.attention.length?[<button key={`${project.id}:more`} onClick={()=>onProject(project.id)}>{project.id}: показано {state.snapshot.attention.length} из {state.snapshot.attention_count} — открыть все задачи</button>]:[])]:[])}{loaded.length===projects.length&&!attention&&!loaded.some(({state})=>state?.snapshot?.runs.some(runNeedsAttention))&&<p className="muted">Нет задач и запусков, требующих внимания</p>}{loaded.length<projects.length&&<p className="muted">Сводка неполная: часть проектов ещё загружается или недоступна.</p>}</section>
  <div className="overview-projects">{rows.map(({project,state})=><article className="overview-project" key={project.id}><div className="pane-bar"><h3>{project.id}</h3><span className={`project-indicator project-indicator-${health[project.id]?.value??'checking'}`} title={projectHealthLabel(health[project.id])}><span className="project-status-dot" aria-hidden="true"/></span><button onClick={()=>onProject(project.id)}>Задачи проекта</button></div><p className="muted">Сервисы: {projectHealthLabel(health[project.id])}</p>{state?.error&&<p role="alert" className="error">{state.error}{state.updated&&` Последние данные: ${new Date(state.updated).toLocaleTimeString()}.`}</p>}{!state?.snapshot&&!state?.error&&<p className="muted">Загрузка…</p>}{state?.snapshot&&<><p className="muted">{state.error?'Последние сохранённые данные · ':''}Активных: {state.snapshot.active_count} · Требуют внимания: {state.snapshot.attention_count} · Обновлено {new Date(state.updated!).toLocaleTimeString()}</p>{state.snapshot.runs.map(run=><button className="overview-run-link" key={String(run.run_id)} onClick={()=>onAutomation(project.id)}>Автоматизация: {runLabels[String(run.status)]??String(run.status)}{runProgress(run)}{run.control==='pause'?' · Пауза':''}</button>)}{state.snapshot.tasks.map(taskRow)}{state.snapshot.active_count>state.snapshot.tasks.length&&<button onClick={()=>onProject(project.id)}>Все задачи ({state.snapshot.active_count})</button>}{!state.snapshot.active_count&&<p className="muted">Нет активных задач</p>}</>}</article>)}</div>
 </>}</section>;
}
