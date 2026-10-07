"""Local implementation-model double; production Rust owns lifecycle/verification."""
import argparse, json, os, re, time
from pathlib import Path
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
p=argparse.ArgumentParser();p.add_argument('--doc',required=True);p.add_argument('--rendezvous');p.add_argument('serve');p.add_argument('--hostname');p.add_argument('--port',type=int);a=p.parse_args()
root=Path.cwd(); runtime=root.parent/'runtime'; runtime.mkdir(exist_ok=True)
sessions={}; messages={}; doc=json.loads(Path(a.doc).read_text())
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self): self.handle_request()
 def do_POST(self): self.handle_request()
 def handle_request(self):
  path=self.path.split('?')[0]; data=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}')
  with (runtime/'proof-requests.jsonl').open('a') as f: f.write(json.dumps({'method':self.command,'path':path})+'\n')
  status=200
  if path=='/global/health': value={'healthy':True,'version':'proof'}
  elif path=='/path': value={'directory':str(root)}
  elif path=='/doc': value=doc
  elif path=='/permission' or path=='/question': value=[]
  elif path=='/session/status': value={}
  elif path=='/session' and self.command=='GET': value=list(sessions.values())
  elif path=='/session' and self.command=='POST':
   sid='ses_proof_'+str(len(sessions)+1);value={'id':sid,'title':data.get('title'),'directory':str(root)};sessions[sid]=value
  elif path.endswith('/prompt_async'):
   sid=path.split('/')[2]; text='\n'.join(part.get('text','') for part in data.get('parts',[])); match=re.search(r'PROOF:(auth|fix|consumer|final|left|right)',text)
   if not match: self.send_error(400);return
   step=match.group(1);trace={'message_id':data['messageID'],'step':step,'started':time.monotonic()}
   if a.rendezvous:
    gate=Path(a.rendezvous);(gate/step).write_text('started'); deadline=time.monotonic()+15
    while not (gate/'release').exists() and time.monotonic()<deadline: time.sleep(.01)
    if not (gate/'release').exists(): self.send_error(503);return
   if step=='fix': (root/'module.py').write_text('def add(a,b):\n    return a+b\nassert add(2,3)==5\n')
   elif step=='consumer': (root/'consumer.py').write_text('from module import add\nassert add(4,5)==9\n')
   elif step in ('left','right'): (root/(step+'.txt')).write_text(step+'\n')
   trace['finished']=time.monotonic()
   with (runtime/'proof-prompts.jsonl').open('a') as f: f.write(json.dumps(trace)+'\n')
   mid=data['messageID'];messages.setdefault(sid,[]).extend([{'info':{'id':mid,'role':'user','sessionID':sid},'parts':[]},{'info':{'id':'answer_'+mid,'role':'assistant','parentID':mid,'sessionID':sid,'finish':'stop','time':{'completed':1}},'parts':[{'type':'text','text':'done'}]}])
   if step=='auth': messages[sid][-1]['info']['error']={'name':'AI_APICallError','data':{'statusCode':401}}
   status=204;value={}
  elif path.endswith('/message'): value=messages.get(path.split('/')[2],[])
  elif path.startswith('/session/'): value=sessions.get(path.split('/')[2],{})
  else: value={}
  raw=b'' if status==204 else json.dumps(value).encode();self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
ThreadingHTTPServer(('127.0.0.1',a.port),Handler).serve_forever()
