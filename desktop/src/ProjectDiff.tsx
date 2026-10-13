import {useEffect,useRef,useState} from 'react';
import {TaskDiff} from './TaskDiff';
import type {ConsoleCheckout} from './ConsoleCheckoutPicker';
export function ProjectDiff({project,checkout}:{project:string;checkout?:ConsoleCheckout}){
 const [open,setOpen]=useState(false);const dialog=useRef<HTMLDialogElement>(null);
 useEffect(()=>{if(open)dialog.current?.showModal();else dialog.current?.close();},[open]);
 return <><button title="Незакоммиченные изменения проекта" onClick={()=>setOpen(true)}>Изменения</button><dialog ref={dialog} className="git-dialog project-diff-dialog" aria-label="Изменения Git проекта" onCancel={()=>setOpen(false)} onClose={()=>setOpen(false)}><div className="git-dialog-header"><strong>Изменения · {project}{checkout?' · '+(checkout.current?.replace(/^refs\/heads\//,'')??'detached HEAD'):''}</strong><button onClick={()=>setOpen(false)}>Закрыть</button></div>{open&&<TaskDiff project={project} checkout={checkout}/>}</dialog></>;
}
