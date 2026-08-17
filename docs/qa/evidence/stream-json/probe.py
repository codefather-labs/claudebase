import json, subprocess, sys, threading, time

proc = subprocess.Popen(
    ["claude", "-p", "--input-format", "stream-json", "--output-format", "stream-json",
     "--verbose", "--replay-user-messages", "--dangerously-skip-permissions"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1,
)

lines = []
def pump():
    for ln in proc.stdout:
        lines.append((round(time.time()-t0,1), ln.rstrip()[:300]))
threading.Thread(target=pump, daemon=True).start()

def send(text):
    msg = {"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}
    proc.stdin.write(json.dumps(msg) + "\n"); proc.stdin.flush()

t0 = time.time()
send("MSG-A ответь ровно одним словом: альфа")
time.sleep(12)
print(">>> sending second message into the SAME process", flush=True)
send("MSG-B ответь ровно одним словом: браво")
time.sleep(15)
proc.stdin.close()
try: proc.wait(timeout=10)
except Exception: proc.kill()

for ts, ln in lines: print(f"[{ts:5}] {ln}")
err = proc.stderr.read()
if err.strip(): print("STDERR:", err[:800])
