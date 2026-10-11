import {useEffect,useRef,useState} from 'react';
import {TaskDiff} from './TaskDiff';
export function ProjectDiff({project}:{project:string}){
 const [open,setOpen]=useState(false);const dialog=useRef<HTMLDialogElement>(null);
 useEffect(()=>{if(open)dialog.current?.showModal();else dialog.current?.close();},[open]);
 return <><button title="Незакоммиченные изменения проекта" onClick={()=>setOpen(true)}>Изменения</button><dialog ref={dialog} className="git-dialog project-diff-dialog" aria-label="Изменения Git проекта" onCancel={()=>setOpen(false)} onClose={()=>setOpen(false)}><div className="git-dialog-header"><strong>Изменения проекта · {project}</strong><button onClick={()=>setOpen(false)}>Закрыть</button></div>{open&&<TaskDiff project={project}/>}</dialog></>;
}
