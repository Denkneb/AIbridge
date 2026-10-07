import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {FieldHelp} from './FieldHelp';

interface FileView {file:string;exists:boolean;content:string}
export function OpenCodeConfigEditor({project,workspace}:{project:string;workspace:string}){
 const [file,setFile]=useState('opencode.json'),[content,setContent]=useState(''),[original,setOriginal]=useState(''),[loaded,setLoaded]=useState(false),[exists,setExists]=useState(false),[busy,setBusy]=useState(false),[error,setError]=useState(''),[saved,setSaved]=useState(false),[review,setReview]=useState<string|null>(null);
 const pending=useRef<string|null>(null);
 const cancel=()=>{const id=pending.current;pending.current=null;setReview(null);if(id)void invoke('opencode_config_cancel',{reviewId:id}).catch(()=>{});};
 useEffect(()=>()=>{if(pending.current)void invoke('opencode_config_cancel',{reviewId:pending.current}).catch(()=>{});},[]);
 const load=async()=>{setBusy(true);setError('');setSaved(false);cancel();try{const view=await invoke<FileView>('opencode_config_read',{project,file});setContent(view.content);setOriginal(view.exists?view.content:'');setExists(view.exists);setLoaded(true);}catch(e){setError(String(e));setLoaded(false);}finally{setBusy(false);}};
 const preview=async()=>{setBusy(true);setError('');setSaved(false);cancel();try{const result=await invoke<{review_id:string}>('opencode_config_preview',{project,file,content,originalContent:exists?original:null});pending.current=result.review_id;setReview(result.review_id);}catch(e){setError(String(e));}finally{setBusy(false);}};
 const save=async()=>{if(!review)return;setBusy(true);setError('');try{await invoke('opencode_config_apply',{reviewId:review});pending.current=null;setReview(null);setOriginal(content);setExists(true);setSaved(true);}catch(e){setError(String(e));}finally{setBusy(false);}};
 return <section className="opencode-editor"><h3><FieldHelp label="Конфигурация OpenCode проекта" description="Откройте opencode.json или opencode.jsonc в сохранённом workspace. Проверка учитывает синтаксис JSON/JSONC и настройки, которыми управляет AIbridge. Сохранение создаёт резервную копию и требует остановки исполнителей, сервисов и контроллеров. Незавершённые задачи сами по себе сохранение не блокируют. Env-файл здесь не читается."/></h3>
  <p className="muted">{workspace}</p><div className="input-action"><select aria-label="Файл конфигурации OpenCode" disabled={busy} value={file} onChange={e=>{cancel();setFile(e.target.value);setLoaded(false);setContent('');setSaved(false);setError('');}}><option>opencode.json</option><option>opencode.jsonc</option></select><button type="button" disabled={busy} onClick={()=>void load()}>Загрузить</button></div>
  {error&&<p role="alert" className="error">{error}</p>}
  {loaded&&<><p className="muted">{exists?'Редактирование существующего файла':'Файл отсутствует — будет создан при сохранении'}</p><textarea aria-label="Содержимое конфигурации OpenCode" disabled={busy} spellCheck={false} value={content} onChange={e=>{cancel();setContent(e.target.value);setSaved(false);}}/><button type="button" disabled={busy} onClick={()=>void preview()}>Проверить конфигурацию OpenCode</button></>}
  {review&&<div className="review" role="dialog" aria-label="Просмотр конфигурации OpenCode"><div className="diff-columns"><div><h4>Сейчас</h4><pre>{original||'Файл отсутствует'}</pre></div><div><h4>После сохранения</h4><pre>{content}</pre></div></div><button type="button" className="primary" disabled={busy} onClick={()=>void save()}>Сохранить конфигурацию OpenCode</button><button type="button" disabled={busy} onClick={cancel}>Отмена</button></div>}
  {saved&&<p role="status">Конфигурация OpenCode сохранена.</p>}
 </section>;
}
