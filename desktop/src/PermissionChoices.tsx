import {FieldHelp} from './FieldHelp';

const permissions = [
 ['read', 'Чтение файлов'],
 ['edit', 'Изменение файлов'],
 ['bash', 'Команды терминала (с проверкой политики команд)'],
 ['glob', 'Поиск файлов по шаблону'],
 ['grep', 'Поиск текста в файлах'],
 ['webfetch', 'Загрузка веб-страниц'],
 ['websearch', 'Поиск в интернете'],
 ['task', 'Запуск подзадач OpenCode'],
 ['todowrite', 'Ведение списка задач'],
 ['lsp', 'Работа с языковым сервером'],
 ['skill', 'Использование навыков OpenCode'],
];

export function PermissionChoices({value, onChange}: {value: string[]; onChange: (value: string[])=>void}) {
 const options = [...permissions, ...value.filter(name=>!permissions.some(([key])=>key===name)).map(name=>[name, 'Сохранено в конфигурации; автоматическое одобрение не поддерживается'])];
 return <fieldset className="permission-choices">
  <legend><FieldHelp label="Разрешения" description="Выберите запросы OpenCode, которые AIbridge может автоматически одобрять. Если ничего не выбрано, автоматическое одобрение этих запросов отключено. Для bash также проверяется политика команд. Доступ к внешним каталогам и Rust state настраивается отдельно."/></legend>
  <button type="button" disabled={permissions.every(([name])=>value.includes(name))} onClick={()=>onChange([...new Set([...value, ...permissions.map(([name])=>name)])])}>Выбрать все</button>
  {options.map(([name, description])=><label key={name} className="permission-option">
   <input type="checkbox" checked={value.includes(name)} onChange={e=>onChange(e.target.checked?[...value, name]:value.filter(item=>item!==name))}/>
   <span><code>{name}</code><small>{description}</small></span>
  </label>)}
 </fieldset>;
}
