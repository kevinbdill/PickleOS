#!/usr/bin/env python3
"""Boot PICKLE OS (with disks) and exercise the shell's command history and
line-editing keys via QMP, capturing screenshots + serial for verification."""
import json, os, socket, subprocess, time, sys

ROOT = "/home/ubuntu/pickleos"
BIOS = f"{ROOT}/target/x86_64-pickleos/release/bios.img"
QMP = "/tmp/qmp_edit.sock"
SER = "/tmp/pickle_edit_serial.log"

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
print(">> launching QEMU...")
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
f.readline()
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

def tap(q, shift=False):
    if shift: keyev("shift", True)
    keyev(q, True); time.sleep(0.02); keyev(q, False)
    if shift: keyev("shift", False)
    time.sleep(0.05)

def keystroke(ch):
    if ch.isalnum():
        tap(ch.lower(), shift=ch.isupper())
    else:
        q = QMAP.get(ch)
        if q: tap(q, shift=ch in "|>_")

def typ(text):
    for ch in text:
        keystroke(ch)

boot = int(sys.argv[1]) if len(sys.argv) > 1 else 48
print(f">> waiting {boot}s for desktop...")
time.sleep(boot)
shot("edit_01_desktop")

# 1. Build up some history.
print(">> echo alpha")
typ("echo alpha\n"); time.sleep(1.0)
print(">> echo bravo")
typ("echo bravo\n"); time.sleep(1.0)
shot("edit_02_after_two")

# 2. History recall: Up twice should land on 'echo alpha', then run it.
print(">> Up, Up, Enter (recall 'echo alpha')")
tap("up"); time.sleep(0.4)
tap("up"); time.sleep(0.4)
shot("edit_03_recalled")
tap("ret"); time.sleep(1.0)
shot("edit_04_recall_ran")

# 3. Line editing: type 'world', Home, type 'echo ' -> 'echo world', run.
print(">> type 'world', Home, prepend 'echo '")
typ("world"); time.sleep(0.3)
tap("home"); time.sleep(0.3)
typ("echo "); time.sleep(0.3)
shot("edit_05_edited")
tap("ret"); time.sleep(1.0)
shot("edit_06_edited_ran")

# 4. Forward-delete + left-arrow: type 'echo HxI', Left, Delete -> 'echo HI'
print(">> type 'echo HXI', Left, Delete (remove X)")
typ("echo HXI"); time.sleep(0.3)
tap("left"); time.sleep(0.3)
tap("delete"); time.sleep(0.3)
shot("edit_07_deleted")
tap("ret"); time.sleep(1.0)
shot("edit_08_deleted_ran")

# 5. history command.
print(">> history")
typ("history\n"); time.sleep(1.0)
shot("edit_09_history")

print(">> done; shutting down")
cmd({"execute": "quit"})
time.sleep(1)
try: qemu.terminate()
except Exception: pass
print("SERIAL TAIL:")
try:
    with open(SER) as fh:
        print("".join(fh.readlines()[-50:]))
except OSError:
    pass
