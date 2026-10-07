import {useEffect,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {FieldHelp} from './FieldHelp';

interface Keys {file:string|null;names:string[]}
export function OpenCodeKeys({project,onConfigured}:{project:string;onConfigured:(file:string)=>void}){
 const[view,setView]=useState<Keys|null>(null),[name,setName]=useState(''),[value,setValue]=useState(''),[busy,setBusy]=useState(false),[error,setError]=useState(''),[saved,setSaved]=useState(false);
 useEffect(()=>{let cancelled=false;invoke<Keys>('opencode_keys_read',{project}).then(v=>{if(!cancelled)setView(v);}).catch(e=>{if(!cancelled)setError(String(e));});return()=>{cancelled=true;};},[project]);
 const load=async()=>{setBusy(true);setError('');try{setView(await invoke<Keys>('opencode_keys_read',{project}));}catch(e){setError(String(e));}finally{setBusy(false);}};
 const save=async(key:string,secret:string|null)=>{if(!view)return;setBusy(true);setError('');setSaved(false);try{const next=await invoke<Keys>('opencode_key_save',{project,name:key,value:secret,expectedFile:view.file});setView(next);setValue('');setSaved(true);if(next.file)onConfigured(next.file);}catch(e){setError(String(e));}finally{setBusy(false);}};
 return <section className="opencode-keys"><h3><FieldHelp label="Ключи провайдеров OpenCode" description="Введите имя переменной, указанное в настройках вашего провайдера в opencode.json, и её значение. Ключи сохраняются отдельно для этого проекта в приватном env-файле с правами 0600. Существующие значения не загружаются в интерфейс. При первом сохранении переменные из выбранного env-файла копируются в файл проекта; исходный файл остаётся без изменений. Для сохранения остановите исполнителей, сервисы и контроллеры проекта. Незавершённые задачи сами по себе сохранение не блокируют."/></h3>
  {error&&<p className="error" role="alert">{error}</p>}
  {!view?<button type="button" disabled={busy} onClick={()=>void load()}>Загрузить список ключей</button>:<>
   <div className="provider-key-list">{view.names.map(key=><div key={key}><code>{key}</code><span className="muted">Значение скрыто</span><button type="button" disabled={busy} onClick={()=>{setName(key);setValue('');setSaved(false);}}>Заменить</button><button type="button" disabled={busy} aria-label={`Удалить ключ ${key}`} onClick={()=>void save(key,null)}>Удалить</button></div>)}</div>
   {!view.names.length&&<p className="muted">Ключи ещё не добавлены.</p>}
   <form onSubmit={e=>{e.preventDefault();void save(name,value);}}><div className="provider-key-inputs"><label>Имя переменной<input aria-label="Имя переменной провайдера" required pattern="[A-Za-z_][A-Za-z0-9_]*" maxLength={256} placeholder="PROVIDER_API_KEY" value={name} disabled={busy} onChange={e=>{setName(e.target.value);setSaved(false);}}/></label><label>Значение ключа<input aria-label="Значение ключа провайдера" type="password" autoComplete="new-password" spellCheck={false} required maxLength={8192} value={value} disabled={busy} onChange={e=>{setValue(e.target.value);setSaved(false);}}/></label></div><button className="primary" disabled={busy||!name||!value}>{busy?'Сохранение…':view.names.includes(name)?'Заменить ключ':'Сохранить ключ'}</button></form>
   <p className="muted">В opencode.json используйте {'{env:ИМЯ_ПЕРЕМЕННОЙ}'}. Значение вводится без export и обрамляющих кавычек.</p>
  </>}
  {saved&&<p role="status">Ключи сохранены. Запустите сервисы и заново откройте OpenCode.</p>}
 </section>;
}
