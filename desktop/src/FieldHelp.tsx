import {useId, useLayoutEffect, useRef, useState} from 'react';

export function FieldHelp({label, description}: {label: string; description: string}) {
 const id = useId();
 const [open, setOpen] = useState(false);
 const anchor = useRef<HTMLSpanElement>(null);
 const [position, setPosition] = useState({left:0, width:280});
 useLayoutEffect(()=>{
  if (!open || !anchor.current) return;
  const element = anchor.current;
  const pane = element.closest('.content');
  const place = ()=>{
   const rect = element.getBoundingClientRect();
   const bounds = pane?.getBoundingClientRect();
   const minLeft = Math.max(8, (bounds?.left ?? 0)+8);
   const maxRight = Math.min(window.innerWidth-8, (bounds?.right ?? window.innerWidth)-8);
   const width = Math.min(280, Math.max(0, maxRight-minLeft));
   const preferredLeft = rect.left+width > maxRight ? rect.right-width : rect.left;
   const left = Math.max(minLeft, Math.min(preferredLeft, maxRight-width));
   setPosition({left:left-rect.left, width});
  };
  place();
  const observer = new ResizeObserver(place);
  observer.observe(pane ?? element);
  observer.observe(element);
  window.addEventListener('resize', place);
  return ()=>{observer.disconnect();window.removeEventListener('resize', place);};
 }, [open]);
 return <span className="field-heading">{label}<span ref={anchor} className="field-help" onMouseEnter={()=>setOpen(true)} onMouseLeave={()=>setOpen(false)}>
  <button type="button" className="field-help-button" aria-label={`Справка: ${label}`} aria-describedby={open?id:undefined} aria-expanded={open} onFocus={()=>setOpen(true)} onBlur={()=>setOpen(false)} onClick={e=>{e.preventDefault();setOpen(true);}} onKeyDown={e=>{if(e.key==='Escape'){e.preventDefault();setOpen(false);}}}>?</button>
  {open&&<span id={id} role="tooltip" className="field-help-tooltip" style={position}>{description}</span>}
 </span></span>;
}
