#!/usr/bin/env python3
"""Focused capture: ANSI colour swatches + scrollback. Reuses the already
formatted disks so the filesystem mounts fast and the shell is interactive
quickly. Captures `colors` output and a PageUp-scrolled view."""
import json, os, socket, subprocess, time, sys

ROOT = "/home/ubuntu/pickleos"
BIOS = f"{ROOT}/target/x86_64-pickleos/release/bios.img"
QMP = "/tmp/qmp_cc.sock"
SER = "/tmp/pickle_cc_serial.log"
DISK0 = f"{ROOT}/disk0.img"
DISK1 = f"{ROOT}/disk1.img"
BOOT = int(sys.argv[1]) if len(sys.argv) > 1 else 52

QMAP = {" ": "spc", "\n": "ret", "-": "minus", ".": "dot", "/": "slash"}

def launch():
    try: os.remove(QMP)
    except OSError: pass
    args = ["qemu-system-x86_64", "-drive", f"format=raw,file={BIOS}", "-m", "256M",
        "-no-reboot", "-device", "ich9-ahci,id=ahci",
        "-drive", f"id=d0,format=raw,file={DISK0},if=none",
        "-drive", f"id=d1,format=raw,file={DISK1},if=none",
        "-device", "ide-hd,drive=d0,bus=ahci.0",
        "-device", "ide-hd,drive=d1,bus=ahci.1",
        "-serial", f"file:{SER}", "-display", "none",
        "-qmp", f"unix:{QMP},server,nowait"]
    return subprocess.Popen(args)

def connect():
    for _ in range(50):
        try:
            s = socket.socket(socket.AF_UNIX); s.connect(QMP); return s
        except OSError: time.sleep(0.2)
    raise RuntimeError("no QMP")

class Mon:
    def __init__(self):
        self.s = connect(); self.f = self.s.makefile("rw")
        self.f.readline(); self.cmd({"execute": "qmp_capabilities"})
    def cmd(self, obj):
        self.f.write(json.dumps(obj) + "\n"); self.f.flush()
        while True:
            line = self.f.readline()
            if not line: return None
            r = json.loads(line)
            if "return" in r or "error" in r: return r
    def shot(self, name):
        self.cmd({"execute": "screendump", "arguments": {"filename": f"/tmp/{name}.ppm"}})
        time.sleep(0.5)
    def keyev(self, q, down):
        self.cmd({"execute": "input-send-event", "arguments": {
            "events": [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": q}}}]}})
    def tap(self, q):
        self.keyev(q, True); time.sleep(0.03); self.keyev(q, False); time.sleep(0.06)
    def typ(self, text):
        for ch in text:
            if ch.isalnum(): self.tap(ch.lower())
            else:
                q = QMAP.get(ch)
                if q: self.tap(q)

try: os.remove(SER)
except OSError: pass
qemu = launch(); m = Mon()
print(f">> waiting {BOOT}s ...")
time.sleep(BOOT)
m.shot("cc_01_boot")
print(">> colors")
m.typ("colors\n"); time.sleep(2.0)
m.shot("cc_02_colors")
print(">> help (fill), then PageUp")
m.typ("help\n"); time.sleep(2.0)
m.shot("cc_03_help_bottom")
for _ in range(4):
    m.tap("pgup"); time.sleep(0.3)
time.sleep(0.6)
m.shot("cc_04_scrolled")
for _ in range(8):
    m.tap("pgdn"); time.sleep(0.15)
time.sleep(0.6)
m.shot("cc_05_bottom")
print(">> done; quitting")
m.cmd({"execute": "quit"}); time.sleep(1)
try: qemu.terminate()
except Exception: pass
print("DONE")
