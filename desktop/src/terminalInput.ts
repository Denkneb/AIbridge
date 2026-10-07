import type {Terminal} from '@xterm/xterm';

// GTK/WebKit can report ordinary layout characters as keyCode 229 without
// composition events. xterm's deferred textarea diff can then resend old text.
// IBus also emits compositionend without compositionstart for single commits.
// Use committed event data for these paths; leave full IME composition and
// ordinary terminal key handling to xterm.
export function installTerminalInput(terminal:Terminal, host:HTMLElement){
 let composing=false,awaitingInput=false;
 const start=()=>{composing=true;awaitingInput=false;};
 const end=(event:Event)=>{
  if(event.target===terminal.textarea&&!composing&&awaitingInput){
   // xterm otherwise finalizes from offset zero and resends the entire textarea.
   event.stopImmediatePropagation();
   terminal.input((event as CompositionEvent).data);
   awaitingInput=false;
  }
  composing=false;
 };
 const input=(event:Event)=>{
  const e=event as InputEvent;
  if(e.target!==terminal.textarea||!awaitingInput||composing||e.isComposing)return;
  if(e.inputType==='insertText'&&e.data!==null){
   e.stopImmediatePropagation();
   terminal.input(e.data);
  }
 };
 host.addEventListener('compositionstart',start,true);
 host.addEventListener('compositionend',end,true);
 host.addEventListener('input',input,true);
 return {
  key(event:KeyboardEvent){
   if(event.type==='keydown')awaitingInput=event.keyCode===229&&!composing&&!event.isComposing;
   return !(awaitingInput&&(event.type==='keydown'||event.type==='keypress'));
  },
  dispose(){
   host.removeEventListener('compositionstart',start,true);
   host.removeEventListener('compositionend',end,true);
   host.removeEventListener('input',input,true);
  }
 };
}
