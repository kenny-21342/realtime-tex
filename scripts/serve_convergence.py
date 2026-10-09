"""Drive `rtex serve`: wait for convergence, apply one edit after FIND (an \\emph{} wrap of FIND), request a
layout and print every LayoutUpdate/convergence state with timestamps. Used to chase the post-edit
non-convergence seen in the mutation test on math/phy documents.
Usage: RTEX_BIN=target/release/rtex python3 scripts/serve_convergence.py PROJECT MAIN FIND BUILDDIR"""
import json, subprocess, sys, time, threading, os
proj, main, find, build = sys.argv[1:5]
env = dict(os.environ)  # RTEX_TEXLIVE_BIN must be set by the caller
p = subprocess.Popen([os.environ.get('RTEX_BIN', 'target/release/rtex'),'serve','--project',proj,'--main',main,'--build',build],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
t0 = time.time()
def send(o): p.stdin.write(json.dumps(o)+'\n'); p.stdin.flush()
src = open(os.path.join(proj, main), 'rb').read()
at = src.index(find.encode()) + len(find.encode())
state = {'conv': None, 'edited': False}
def reader():
    for l in p.stdout:
        try: e = json.loads(l)
        except Exception: continue
        ev = e.get('event')
        if ev == 'LayoutUpdate':
            c = e['convergence']
            print(f"{time.time()-t0:6.1f}s LayoutUpdate rev={e['versions']['source_revision']} passes={e['passes']} compile={e['compile']} conv={c}", flush=True)
            state['conv'] = c
        elif ev == 'Diagnostics' and any(i['severity']=='error' for i in e['items']):
            print(f"{time.time()-t0:6.1f}s ERRORS", [i['message'][:100] for i in e['items'] if i['severity']=='error'][:3], flush=True)
        elif ev == 'BackgroundScheduled':
            print(f"{time.time()-t0:6.1f}s BackgroundScheduled {e.get('reasons')}", flush=True)
threading.Thread(target=reader, daemon=True).start()
while time.time()-t0 < 200:
    time.sleep(1)
    c = state['conv']
    if c and c.get('state') == 'Converged' and not state['edited']:
        state['edited'] = True
        n=len(find.encode()); send({'cmd':'edit','path':main,'start':at-n,'end':at,'text':'\\emph{'+find+'}'})
        print(f"{time.time()-t0:6.1f}s edit sent", flush=True)
        time.sleep(1); send({'cmd':'request_layout'})
        state['conv'] = None
send({'cmd':'quit'}); time.sleep(2); p.kill()
