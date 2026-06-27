#!/usr/bin/env python3
"""Boot PICKLE OS, type shell commands into the GUI Terminal via QMP, screenshot."""
import json, os, socket, subprocess, time, sys

ROOT = "/home/ubuntu/pickleos"
BIOS = f"{ROOT}/target/x86_64-pickleos/release/bios.img"
QMP = "/tmp/qmp_term.sock"
SER = "/tmp/pickle_term_serial.log"

for p in (QMP, SER):
    try: os.remove(p)
    except OSError: pass

args = [
    "qemu-system-x86_64",
    "-drive", f"format=raw,file={BIOS}", "-m", "256M", "-no-reboot",
    "-device", "ich9-ahci,id=ahci",
    "-drive", f"id=d0,format=raw,file={ROOT}/disk0.img,if=none",
    "-drive", f"id=d1,format=raw,file={ROOT}/disk1.img,if=none",
    "-device", "ide-hd,drive=d0,bus=ahci.0",
    "-device", "ide-hd,drive=d1,bus=ahci.1",
    "-serial", f"file:{SER}",
    "-display", "none",
    "-qmp", f"unix:{QMP},server,nowait",
]
print(">> launching QEMU…")
qemu = subprocess.Popen(args)

def connect():
    for _ in range(50):
        try:
            s = socket.socket(socket.AF_UNIX); s.connect(QMP); return s
        except OSError:
            time.sleep(0.2)
    raise RuntimeError("no QMP")

s = connect()
f = s.makefile("rw")
f.readline()  # greeting
def cmd(obj):
    f.write(json.dumps(obj) + "\n"); f.flush()
    while True:
        line = f.readline()
        if not line: return None
        r = json.loads(line)
        if "return" in r or "error" in r: return r
cmd({"execute": "qmp_capabilities"})

def shot(name):
    path = f"/tmp/{name}.ppm"
    cmd({"execute": "screendump", "arguments": {"filename": path}})
    time.sleep(0.5)
    return path

QMAP = {
    " ": "spc", "\n": "ret", "-": "minus", ".": "dot", "/": "slash",
    "|": "bar", ">": "greater", "_": "underscore",
}
def keyev(q, down):
    cmd({"execute": "input-send-event", "arguments": {
        "events": [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": q}}}]}})

def keystroke(ch):
    if ch.isalnum():
        q = ch.lower()
        shift = ch.isupper()
    else:
        q = QMAP.get(ch)
        shift = ch in "|>_"
    if not q:
        return
    if shift:
        keyev("shift", True)
    keyev(q, True)
    time.sleep(0.02)
    keyev(q, False)
    if shift:
        keyev("shift", False)
    time.sleep(0.05)

def typ(text):
    for ch in text:
        keystroke(ch)

# Wait for boot to reach the desktop (TCG is slow).
boot = int(sys.argv[1]) if len(sys.argv) > 1 else 38
print(f">> waiting {boot}s for desktop…")
time.sleep(boot)
shot("term_01_desktop")

print(">> typing: help")
typ("help\n"); time.sleep(2.0); shot("term_02_help")

print(">> typing: ps")
typ("ps\n"); time.sleep(2.0); shot("term_03_ps")

print(">> typing: echo hello pickle os")
typ("echo hello pickle os\n"); time.sleep(1.5); shot("term_04_echo")

print(">> typing: uptime")
typ("uptime\n"); time.sleep(1.5); shot("term_05_uptime")

print(">> done; shutting down")
cmd({"execute": "quit"})
time.sleep(1)
try: qemu.terminate()
except Exception: pass
print("SERIAL TAIL:")
try:
    with open(SER) as fh:
        print("".join(fh.readlines()[-40:]))
except OSError:
    pass
