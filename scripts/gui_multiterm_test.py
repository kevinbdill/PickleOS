#!/usr/bin/env python3
"""Boot PICKLE OS (with disks) and exercise multiple terminal windows: spawn a
second terminal via the taskbar "+ Terminal" button, type into each, and switch
focus back to the first. Captures screenshots + serial for verification."""
import json, os, socket, subprocess, time, sys

ROOT = "/home/ubuntu/pickleos"
BIOS = f"{ROOT}/target/x86_64-pickleos/release/bios.img"
QMP = "/tmp/qmp_mt.sock"
SER = "/tmp/pickle_mt_serial.log"

for p in (QMP, SER):
    try: os.remove(p)
    except OSError: pass

# Actual framebuffer mode picked by the bootloader is 1280x720 (confirmed via
# a QMP screendump), regardless of the -W/-H hints to `make image`.
SCREEN_W, SCREEN_H = 1280, 720

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

# --- keyboard ---------------------------------------------------------------
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

# --- mouse (PS/2 relative; QEMU rel y maps to screen-down) ------------------
def rel(axis, value):
    cmd({"execute": "input-send-event", "arguments": {
        "events": [{"type": "rel", "data": {"axis": axis, "value": value}}]}})
def move_to(x, y):
    # PS/2 deltas are capped at ~255/packet and QEMU drops oversized/too-fast
    # events, so move in small, well-paced steps. First pin to (0,0) by sending
    # plenty of up-left steps (the driver clamps to screen bounds), then step
    # right/down to the target.
    for _ in range(50):
        rel("x", -25); rel("y", -25); time.sleep(0.015)
    time.sleep(0.2)
    dx = 0
    while dx < x:
        d = min(20, x - dx); rel("x", d); dx += d; time.sleep(0.02)
    dy = 0
    while dy < y:
        d = min(20, y - dy); rel("y", d); dy += d; time.sleep(0.02)
    time.sleep(0.3)
def click(x, y):
    move_to(x, y)
    cmd({"execute": "input-send-event", "arguments": {
        "events": [{"type": "btn", "data": {"down": True, "button": "left"}}]}})
    time.sleep(0.12)
    cmd({"execute": "input-send-event", "arguments": {
        "events": [{"type": "btn", "data": {"down": False, "button": "left"}}]}})
    time.sleep(0.3)

boot = int(sys.argv[1]) if len(sys.argv) > 1 else 50
print(f">> waiting {boot}s for desktop...")
time.sleep(boot)
shot("mt_01_desktop")

# 1. Type into the primary terminal (focused at boot).
print(">> [term1] echo primary")
typ("echo primary\n"); time.sleep(1.0)
shot("mt_02_term1_typed")

# 2. Click the "+ Terminal" taskbar button to open a second terminal.
#    Button: x in [96,200], on the taskbar (h=28) at the screen bottom.
btn_x, btn_y = 148, SCREEN_H - 14
print(f">> move to + Terminal button at ({btn_x},{btn_y})")
move_to(btn_x, btn_y)
shot("mt_02b_cursor_on_button")  # verify cursor landed on the button
cmd({"execute": "input-send-event", "arguments": {
    "events": [{"type": "btn", "data": {"down": True, "button": "left"}}]}})
time.sleep(0.12)
cmd({"execute": "input-send-event", "arguments": {
    "events": [{"type": "btn", "data": {"down": False, "button": "left"}}]}})
time.sleep(1.5)
shot("mt_03_second_opened")

# 3. Type into the second (now focused, topmost) terminal.
print(">> [term2] echo second-window")
typ("echo second-window\n"); time.sleep(1.0)
shot("mt_04_term2_typed")

# 4. Click the first terminal's title bar (y 150..170, above the 2nd window at
#    y>=178) to raise + refocus it, then type there.
print(">> click term1 title to refocus")
click(360, 160); time.sleep(1.0)
shot("mt_05_term1_refocused")
print(">> [term1] echo focus-back")
typ("echo focus-back\n"); time.sleep(1.0)
shot("mt_06_term1_typed_again")

print(">> done; shutting down")
cmd({"execute": "quit"})
time.sleep(1)
try: qemu.terminate()
except Exception: pass
print("SERIAL TAIL:")
try:
    with open(SER) as fh:
        print("".join(fh.readlines()[-60:]))
except OSError:
    pass
