#!/usr/bin/env python3
"""Check owner bootstrap through the real local Tailscale daemon on an isolated server."""
import http.client,ssl,json,subprocess,os,re
port=int(os.environ.get('AMUX_AUTH_TEST_PORT','18856'))
assert 18000 <= port <= 65535, 'Use an isolated test server; production is refused'
status=json.loads(subprocess.check_output(['/usr/local/bin/tailscale','status','--json']))
ip=next(ip for ip in status['Self']['TailscaleIPs'] if ':' not in ip)
def req(path,headers=None,local=False):
    c=http.client.HTTPSConnection('127.0.0.1' if local else ip,port,context=ssl._create_unverified_context(),timeout=15,source_address=None if local else (ip,0))
    c.request('GET',path,headers=headers or {});r=c.getresponse();body=r.read();out=(r.status,dict((k.lower(),v) for k,v in r.getheaders()),body);c.close();return out
before=json.loads(req('/health',local=True)[2])
assert req('/api/prefs')[0]==401,'nonloopback API must require auth before bootstrap'
code,headers,body=req('/')
assert code in (302,303,307),code
cookie=headers.get('set-cookie','')
for flag in ['HttpOnly','Secure','SameSite=Lax']:assert flag in cookie
assert headers.get('location')=='/api/_clear_sw'
code,_,html=req('/',{'Cookie':cookie.split(';')[0]})
assert code==200
match=re.search(rb'window\._AMUX_AUTH_TOKEN=("(?:\\.|[^"\\])*")',html)
assert match
token=json.loads(match[1])
assert token
assert req('/api/prefs',{'Authorization':'Bearer '+token})[0]==200
assert 'set-cookie' not in req('/',{'X-Forwarded-For':ip},local=True)[1]
assert 'set-cookie' not in req('/',{'Cookie':'amux_member=invalid-member'})[1]
after=json.loads(req('/health',local=True)[2])
assert before['build']==after['build'], 'Server image changed during verification'
print('SOURCE',json.dumps({k:after.get(k) for k in ['commit','build']}))
print('PASS: daemon-verified same-owner device gets HttpOnly owner session; API 401 before / authenticated bootstrap and API 200 after; forwarded IP and member cookie do not mint owner cookies')
