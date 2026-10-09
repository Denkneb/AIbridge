import type {ButtonHTMLAttributes,ReactNode} from 'react';

type IconName='dashboard'|'automation'|'settings'|'check'|'play'|'stop'|'sun'|'moon'|'switch';
const icons:Record<IconName,ReactNode>={
 dashboard:<><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/></>,
 automation:<><rect x="9" y="3" width="6" height="5" rx="1"/><path d="M12 8v5M5 16v-3h14v3"/><rect x="2" y="16" width="6" height="5" rx="1"/><rect x="16" y="16" width="6" height="5" rx="1"/></>,
 settings:<><path d="m9 3-.5 3-2 .9-2.5-1-2 3.4L4.5 11v2L2 14.7l2 3.4 2.5-1 2 .9.5 3h6l.5-3 2-.9 2.5 1 2-3.4-2.5-1.7v-2L22 9.3l-2-3.4-2.5 1-2-.9-.5-3Z"/><circle cx="12" cy="12" r="3"/></>,
 check:<><circle cx="12" cy="12" r="9"/><path d="m8 12 3 3 5-6"/></>,
 play:<path d="m8 4 12 8-12 8Z"/>,
 stop:<rect x="5" y="5" width="14" height="14" rx="1"/>,
 sun:<><circle cx="12" cy="12" r="4"/><path d="M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M5 19l1.5-1.5m11-11L19 5"/></>,
 moon:<path d="M20.5 14A9 9 0 0 1 10 3.5 9 9 0 1 0 20.5 14Z"/>,
 switch:<><path d="M4 7h16m-4-4 4 4-4 4M20 17H4m4-4-4 4 4 4"/></>,
};

export function IconButton({icon,label,className='',title=label,...props}:Omit<ButtonHTMLAttributes<HTMLButtonElement>,'children'|'aria-label'>&{icon:IconName;label:string}){
 return <button type="button" {...props} className={`icon-button ${className}`.trim()} aria-label={label} title={title}><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">{icons[icon]}</svg></button>;
}
