#!/usr/bin/env python3
"""Disposable OAuth/PKCE provider for docs/connectors-e2e.md; synthetic data only."""
import http.server,json,pathlib,urllib.parse,secrets,hashlib,base64,threading,time,sys
import argparse,os
parser=argparse.ArgumentParser()
parser.add_argument('--state-dir',required=True)
parser.add_argument('--port',type=int,default=19122)
args=parser.parse_args()
root=pathlib.Path(args.state_dir).resolve();root.mkdir(parents=True,exist_ok=True)
os.umask(0o077)
path=root/'provider-state.json'
state=json.loads(path.read_text()) if path.exists() else {'codes':{},'refresh':{},'access':{},'events':[],'revoked':[]}
lock=threading.Lock()
def save():
 path.write_text(json.dumps(state,indent=2))
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def response(self,code,body):
  data=json.dumps(body).encode();self.send_response(code);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
 def do_GET(self):
  u=urllib.parse.urlparse(self.path);q=dict(urllib.parse.parse_qsl(u.query))
  with lock:
   if u.path=='/issue':
    assert q.get('code_challenge_method')=='S256'; assert q.get('response_type')=='code';assert q.get('client_id')=='fixture-client'
    c=secrets.token_urlsafe(24); state['codes'][c]=q;state['events'].append({'kind':'authorize','identity':q['identity']});save();return self.response(200,{'code':c})
   if u.path=='/stats':return self.response(200,{'events':state['events'],'active_refresh_count':len(state['refresh'])})
   if u.path=='/data':
    bearer=self.headers.get('Authorization','').removeprefix('Bearer ')
    identity=state['access'].get(bearer)
    if bearer=='fixture-api-key': identity='api-key'
    if not identity or identity in state['revoked']: return self.response(401,{'error':'invalid_token'})
    state['events'].append({'kind':'data','identity':identity});save()
    records=[{'id':'A','rev':1,'amount':110,'status':'billable'},{'id':'B','rev':1,'amount':200,'status':'billable'},{'id':'C','rev':1,'amount':45,'status':'billable'},{'id':'A','rev':2,'amount':80,'status':'billable'},{'id':'B','rev':2,'amount':200,'status':'void'},{'id':'C','rev':1,'amount':45,'status':'billable'},{'id':'D','rev':1,'amount':12,'status':'billable'}]
    return self.response(200,{'identity':identity,'records':records if identity=='beth' else [{'id':'X','rev':1,'amount':9,'status':'billable'}]})
   return self.response(404,{'error':'unknown'})
 def do_POST(self):
  q=dict(urllib.parse.parse_qsl(self.rfile.read(int(self.headers.get('Content-Length','0'))).decode()))
  with lock:
   if self.path=='/revoke': state['revoked'].append(q['identity']);save();return self.response(200,{'ok':True})
   if self.path!='/token':return self.response(404,{'error':'unknown'})
   if q.get('client_id')!='fixture-client' or q.get('client_secret')!='fixture-secret':return self.response(401,{'error':'invalid_client'})
   grant=q.get('grant_type');identity=None
   if grant=='authorization_code':
    code=state['codes'].pop(q.get('code'),None)
    if not code:return self.response(400,{'error':'invalid_grant'})
    challenge=base64.urlsafe_b64encode(hashlib.sha256(q.get('code_verifier','').encode()).digest()).decode().rstrip('=')
    if challenge!=code['code_challenge'] or q.get('redirect_uri')!=code['redirect_uri']: return self.response(400,{'error':'invalid_grant'})
    identity=code['identity'];state['revoked']=[x for x in state['revoked'] if x!=identity];lifetime=2
   elif grant=='refresh_token':
    identity=state['refresh'].pop(q.get('refresh_token'),None);lifetime=3600
   if not identity or identity in state['revoked']: save();return self.response(400,{'error':'invalid_grant'})
   access='fixture-access-'+secrets.token_urlsafe(12);refresh='fixture-refresh-'+secrets.token_urlsafe(12)
   state['refresh'][refresh]=identity;state['access'][access]=identity
   state['events'].append({'kind':grant,'identity':identity,'pkce_verified':grant=='authorization_code','exact_redirect_verified':grant=='authorization_code'});save()
   return self.response(200,{'access_token':access,'refresh_token':refresh,'expires_in':lifetime,'scope':'invoices.read'})
http.server.ThreadingHTTPServer(('127.0.0.1',args.port),Handler).serve_forever()
