#!/usr/bin/env python3
import json, os, socket, subprocess, time
ROOT="/home/ubuntu/pickleos"
BIOS=f"{ROOT}/target/x86_64-pickleos/release/bios.img"
QMP="/tmp/qmp_diag.sock"; SER="/tmp/diag_serial.log"
for p in (QMP,SER):
    try: os.remove(p)
    except OSError: pass
args=["qemu-system-x86_64","-drive",f"format=raw,file={BIOS}","-m","256M","-no-reboot",
      "-serial",f"file:{SER}","-display","none","-qmp",f"unix:{QMP},server,nowait"]
q=subprocess.Popen(args)
def conn():
    for _ in range(50):
        try:
            s=socket.socket(socket.AF_UNIX); s.connect(QMP); return s
        except OSError: time.sleep(0.2)
s=conn(); f=s.makefile("rw"); f.readline()
def cmd(o):
    f.write(json.dumps(o)+"\n"); f.flush()
    while True:
        l=f.readline()
        if not l: return None
        r=json.loads(l)
        if "return" in r or "error" in r: return r
print("caps:",cmd({"execute":"qmp_capabilities"}))
print("query-kbd:",cmd({"execute":"query-status"}))
time.sleep(42)
# Try send-key
print("send-key a:",cmd({"execute":"send-key","arguments":{"keys":[{"type":"qcode","data":"a"}]}}))
time.sleep(0.5)
# Try input-send-event down+up
print("ise a down:",cmd({"execute":"input-send-event","arguments":{"events":[{"type":"key","data":{"down":True,"key":{"type":"qcode","data":"a"}}}]}}))
print("ise a up:",cmd({"execute":"input-send-event","arguments":{"events":[{"type":"key","data":{"down":False,"key":{"type":"qcode","data":"a"}}}]}}))
time.sleep(0.5)
# Try a 'number' key code form
print("send-key by number 0x1e:",cmd({"execute":"send-key","arguments":{"keys":[{"type":"number","data":30}]}}))
time.sleep(1)
cmd({"execute":"quit"}); time.sleep(1)
try: q.terminate()
except: pass
print("=== scancodes in serial ===")
os.system(f"grep -a scancode {SER} | tail -20")
