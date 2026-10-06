(async()=>{
 const invoke=window.__TAURI_INTERNALS__.invoke;
 const checks={};
 try {
  for(let i=0;i<100&&!document.querySelector('.brand');i++)await new Promise(r=>setTimeout(r,50));
  if(!document.querySelector('.brand'))throw Error('frontend_missing');checks.react_rendered=true;
  const projects=await invoke('projects');if(projects.length!==1||projects[0].id!=='proof')throw Error('project_binding');checks.project_binding=true;
  const result=await invoke('dashboard',{query:{project:'proof',active_only:false,linked:false,offset:0,limit:100}});if(result.tasks.length)throw Error('unexpected_tasks');checks.readonly_dashboard=true;
  const review=await invoke('project_preview',{draft:{...projects[0],max_rounds:4},password:'fixture-private-password',token:null});if(JSON.stringify(review).includes('fixture-private-password'))throw Error('credential_leak');await invoke('project_cancel',{reviewId:review.review_id});checks.preview_redaction=true;
  const session=await invoke('terminal_open',{project:'proof',profile:'shell',task:null,rows:24,cols:80});await invoke('terminal_resize',{session,rows:37,cols:101});await invoke('terminal_write',{session,bytes:Array.from(new TextEncoder().encode("printf 'DESKTOP_PTY_PROOF\\n'; stty size\n"))});
  let output='';const decoder=new TextDecoder();for(let i=0;i<100;i++){for(const event of await invoke('terminal_read',{session})){if(event.type==='data')output+=decoder.decode(new Uint8Array(event.bytes),{stream:true});}if(output.includes('DESKTOP_PTY_PROOF')&&output.includes('37 101'))break;await new Promise(r=>setTimeout(r,30));}if(!output.includes('37 101'))throw Error('pty_io_resize');await invoke('terminal_close',{session});checks.real_webview_ipc_pty=true;
  [...document.querySelectorAll('.terminal-pane button')].find(b=>b.textContent==='Открыть').click();
  const binding=()=>document.querySelector('.terminal-pane .muted')?.textContent;
  for(let i=0;i<100&&binding()!=='proof · shell';i++)await new Promise(r=>setTimeout(r,30));
  if(binding()!=='proof · shell')throw Error('xterm_session_missing');checks.xterm_connected=true;
  const xterm=document.querySelector('.xterm');if(!xterm)throw Error('xterm_missing');
  const separator=document.querySelector('[role="separator"]'),before=separator.getAttribute('aria-valuenow');separator.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowLeft',bubbles:true}));await new Promise(r=>setTimeout(r,100));if(separator.getAttribute('aria-valuenow')===before)throw Error('split_unchanged');checks.split_keyboard=true;
  const settings=[...document.querySelectorAll('nav button')].find(b=>b.textContent==='Настройки');settings.click();await new Promise(r=>setTimeout(r,100));if(!document.querySelector('input[type="password"]'))throw Error('settings_missing');checks.settings_rendered=true;
  if(document.querySelector('.xterm')!==xterm||binding()!=='proof · shell')throw Error('terminal_recreated_on_rerender');checks.terminal_survives_rerender=true;
  document.querySelector('nav button').click();await new Promise(r=>setTimeout(r,100));
  [...document.querySelectorAll('.terminal-pane button')].find(b=>b.textContent==='Завершить').click();
  await invoke('smoke_complete',{passed:true,checks});
 }catch(_){await invoke('smoke_complete',{passed:false,checks});}
})();
