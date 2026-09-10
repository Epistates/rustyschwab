#!/usr/bin/env python3
"""Identical loopback quote payloads; measures SDK call plus JSON decoding.
No broker credentials. Reference authentication alone is stubbed to avoid browsers.
Results are a local sequential HTTP benchmark, not production trading latency.
"""
import argparse, hashlib, http.server, json, os, pathlib, platform, resource, socket, subprocess, sys, threading, time
from unittest.mock import patch
ROOT = pathlib.Path(__file__).resolve().parents[1]

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--reference',type=pathlib.Path,default=ROOT.parent/'reference/Schwabdev');parser.add_argument('--samples',type=int,default=1000);parser.add_argument('--rust-only',action='store_true');parser.add_argument('--output',type=pathlib.Path,required=True);args=parser.parse_args()
    payload=(ROOT/'tools/fixtures/quotes.json').read_bytes()
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version='HTTP/1.1'
        def setup(self):
            super().setup();self.connection.setsockopt(socket.IPPROTO_TCP,socket.TCP_NODELAY,1)
        def do_GET(self):
            self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload)
        def log_message(self,*args): pass
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);server.daemon_threads=True
    threading.Thread(target=server.serve_forever,daemon=True).start();url=f'http://127.0.0.1:{server.server_port}'
    result={'platform':platform.platform(),'python':sys.version,'fixture_sha256':hashlib.sha256(payload).hexdigest(),'scope':'sequential loopback HTTP plus JSON decode; no TLS/auth refresh/quota waits','results':[]}
    binary=ROOT/'target/release/enterprise-bench'
    result['results'].append(json.loads(subprocess.check_output([str(binary),url,str(args.samples)],text=True)))
    if not args.rust_only:
        sys.path.insert(0,str(args.reference.resolve()))
        import schwabdev.client as reference
        class Tokens:
            access_token='synthetic'
            def __init__(self,*args,**kwargs): pass
            def update_tokens(self,*args,**kwargs): return False
        with patch.object(reference,'Tokens',Tokens):
            client=reference.Client(app_key='k'*32,app_secret='s'*16,open_browser_for_auth=False)
        client._base_api_url=url
        for _ in range(20): client.quotes(['AAPL']).json()
        samples=[];start=time.perf_counter()
        for _ in range(args.samples):
            at=time.perf_counter();body=client.quotes(['AAPL']).json();assert body['AAPL']['symbol']=='AAPL';samples.append((time.perf_counter()-at)*1000)
        elapsed=time.perf_counter()-start;samples.sort();pct=lambda p:samples[min(len(samples)-1,int((len(samples)-1)*p+0.999999))]
        result['results'].append({'implementation':'Schwabdev','samples':args.samples,'elapsed_seconds':elapsed,'requests_per_second':args.samples/elapsed,'p50_ms':pct(.5),'p95_ms':pct(.95),'p99_ms':pct(.99),'max_ms':samples[-1]})
        result['reference_commit']=subprocess.check_output(['git','-C',str(args.reference),'rev-parse','HEAD'],text=True).strip()
        client.close()
    result['rustc']=subprocess.check_output(['rustc','--version'],text=True).strip()
    args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));server.shutdown()
if __name__=='__main__': main()
