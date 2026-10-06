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
  const terminal=document.querySelector('.terminal-host').smokeTerminal;
  if(!terminal)throw Error('smoke_frontend_build_required');
  const renderHost=document.createElement('div');renderHost.style.cssText='position:fixed;left:-10000px;width:800px;height:600px';document.body.appendChild(renderHost);
  const renderer=new terminal.constructor({cols:80,rows:24,scrollback:5000});renderer.open(renderHost);
  let write=bytes=>new Promise(resolve=>renderer.write(bytes,resolve));
  await write('\x1b[?1049h\x1b[2J\x1b[H\x1b[31mRED\x1b[0m');
  if(renderer.buffer.active.type!=='alternate'||renderer.buffer.active.getLine(0).translateToString(true)!=='RED'||renderer.buffer.active.getLine(0).getCell(0).getFgColor()!==1)throw Error('ansi_alternate');
  await write('\x1b[?1049l');checks.ansi_cursor_alternate=true;
  await write('\x1b[2J\x1b[H');const unicode=new TextEncoder().encode('界е\u0301');
  await write(unicode.subarray(0,2));await write(unicode.subarray(2));
  const line=renderer.buffer.active.getLine(0);if(line.getCell(0).getWidth()!==2||line.getCell(2).getChars()!=='е\u0301')throw Error('unicode_cells');checks.unicode_wide_combining=true;
  renderer.select(0,0,3);if(renderer.getSelection()!=='界е\u0301')throw Error('selection');renderer.clearSelection();checks.selection=true;
  renderer.dispose();renderHost.remove();write=bytes=>new Promise(resolve=>terminal.write(bytes,resolve));
  const inputs=[];const listener=terminal.onData(data=>inputs.push(data));
  await write('\x1b[?2004h');terminal.paste('printf BRACKETED_PROOF');listener.dispose();
  if(inputs[0]!=='\x1b[200~printf BRACKETED_PROOF\x1b[201~')throw Error('bracketed_paste');
  // Clear the shell edit buffer before exercising a large ordered paste.
  terminal.input('\x15');terminal.paste("python3 -c \"print('LARGE_PASTE_OK',len('"+'ю'.repeat(12000)+"'))\" ");terminal.input('\r');
  let pastePassed=false;
  for(let i=0;i<300;i++){
   for(let row=0;row<terminal.buffer.active.length;row++)if(terminal.buffer.active.getLine(row).translateToString(true)==='LARGE_PASTE_OK 12000')pastePassed=true;
   if(pastePassed)break;await new Promise(r=>setTimeout(r,20));
  }
  if(!pastePassed)throw Error('large_paste_bytes');checks.large_paste_bytes=true;
  await write('\x1b[?2004l');checks.bracketed_paste=true;
  for(let i=0;i<5100;i++)terminal.writeln('scrollback');await write('');
  if(terminal.buffer.active.length>terminal.rows+5000)throw Error('scrollback_limit');checks.bounded_scrollback=true;
  terminal.clear();terminal.focus();checks.large_paste_queued=!document.querySelector('.terminal-pane .error');
  if(!checks.large_paste_queued)throw Error('large_paste_failed');
  const keyboard=[];const keyboardListener=terminal.onData(data=>keyboard.push(data));
  terminal.textarea.dispatchEvent(new KeyboardEvent('keydown',{key:'c',code:'KeyC',keyCode:67,ctrlKey:true,bubbles:true}));
  keyboardListener.dispose();if(!keyboard.includes('\x03'))throw Error('ctrl_c');checks.keyboard_sigint=true;
  await write('\x1b[?1000h\x1b[?1006h');
  const mouse=[];const mouseListener=terminal.onData(data=>mouse.push(data));
  const screen=document.querySelector('.xterm-screen'),rect=screen.getBoundingClientRect();
  screen.dispatchEvent(new MouseEvent('mousedown',{button:0,buttons:1,clientX:rect.left+10,clientY:rect.top+10,bubbles:true}));
  document.dispatchEvent(new MouseEvent('mouseup',{button:0,buttons:0,clientX:rect.left+10,clientY:rect.top+10,bubbles:true}));
  mouseListener.dispose();await write('\x1b[?1000l\x1b[?1006l');
  if(!mouse.some(data=>data.startsWith('\x1b[<')))throw Error('mouse_sgr');checks.mouse_reporting=true;
  await invoke('clipboard_write',{text:'CLIPBOARD_界е\u0301'});
  if(await invoke('clipboard_read')!=='CLIPBOARD_界е\u0301')throw Error('clipboard_roundtrip');checks.native_clipboard_unicode=true;
  const xterm=document.querySelector('.xterm');if(!xterm)throw Error('xterm_missing');
  const separator=document.querySelector('[role="separator"]'),before=separator.getAttribute('aria-valuenow');separator.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowLeft',bubbles:true}));await new Promise(r=>setTimeout(r,100));if(separator.getAttribute('aria-valuenow')===before)throw Error('split_unchanged');checks.split_keyboard=true;
  const settings=[...document.querySelectorAll('nav button')].find(b=>b.textContent==='Настройки');settings.click();await new Promise(r=>setTimeout(r,100));if(!document.querySelector('input[type="password"]'))throw Error('settings_missing');checks.settings_rendered=true;
  if(document.querySelector('.xterm')!==xterm||binding()!=='proof · shell')throw Error('terminal_recreated_on_rerender');checks.terminal_survives_rerender=true;
  document.querySelector('nav button').click();await new Promise(r=>setTimeout(r,100));
  [...document.querySelectorAll('.terminal-pane button')].find(b=>b.textContent==='Завершить').click();
  const options=await invoke('smoke_options');
  if(options.live_tui){
   for(const profile of ['codex','opencode']){
    const select=document.querySelector('[aria-label="Программа терминала"]');select.value=profile;select.dispatchEvent(new Event('change',{bubbles:true}));await new Promise(r=>setTimeout(r,50));
    [...document.querySelectorAll('.terminal-pane button')].find(b=>b.textContent==='Открыть').click();
    let ready=false;
    for(let i=0;i<600;i++){
     let text='';for(let row=0;row<terminal.buffer.active.length;row++)text+=terminal.buffer.active.getLine(row).translateToString(true)+'\n';
     if(binding()===`proof · ${profile}`&&new RegExp(profile==='codex'?'codex|OpenAI|trust':'opencode|OpenCode|Trust','i').test(text)){ready=true;break;}
     await new Promise(r=>setTimeout(r,50));
    }
    if(!ready)throw Error(profile+'_tui_missing');
    terminal.input('\x1b[B');terminal.input('\t');
    const size=document.querySelector('[aria-label="Размер шрифта терминала"]');size.value='16';size.dispatchEvent(new Event('change',{bubbles:true}));await new Promise(r=>setTimeout(r,100));
    if(!document.querySelector('.xterm')||binding()!==`proof · ${profile}`)throw Error(profile+'_tui_resize');
    checks[profile+'_actual_tui_render_input_resize']=true;
    [...document.querySelectorAll('.terminal-pane button')].find(b=>b.textContent==='Завершить').click();await new Promise(r=>setTimeout(r,800));
   }
  }
  await invoke('smoke_complete',{passed:true,checks});
 }catch(error){checks.failure=error.message;await invoke('smoke_complete',{passed:false,checks});}
})();
