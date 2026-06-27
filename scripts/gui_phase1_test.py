#!/usr/bin/env python3
"""Phase 1 verification: ANSI colours, terminal scrollback, and command-history
persistence across reboots.

Two boots in one run:
  Boot A (fresh disks): format on first boot, run `colors` (ANSI), run `help`
    (long output for scrollback), PageUp to scroll back, and type a couple of
    distinctive `echo` commands so they are saved to /.pickleos_history.
  Boot B (same disks):  confirm the serial log reports the persisted history was
    loaded (N>0), and that ArrowUp recalls a command typed in the previous boot.
"""
import json, os, socket, subprocess, time, sys

ROOT = "/home/ubuntu/pickleos"
BIOS = f"{ROOT}/target/x86_64-pickleos/release/bios.img"
QMP = "/tmp/qmp_p1.sock"
SER = "/tmp/pickle_p1_serial.log"
DISK0 = f"{ROOT}/disk0.img"
DISK1 = f"{ROOT}/disk1.img"

SCREEN_W, SCREEN_H = 1280, 720
BOOT = int(sys.argv[1]) if len(sys.argv) > 1 else 52

QMAP = {
    " ": "spc", "\n": "ret", "-": "minus", ".": "dot", "/": "slash",
    "|": "bar", ">": "greater", "_": "underscore",
}

def fresh_disks():
    for d in (DISK0, DISK1):
        try: os.remove(d)
        except OSError: pass
        subprocess.run(["qemu-img", "create", "-f", "raw", d, "16M"], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def launch():
    for p in (QMP,):
        try: os.remove(p)
        except OSError: pass
    args = [
        "qemu-system-x86_64",
        "-drive", f"format=raw,file={BIOS}", "-m", "256M", "-no-reboot",
        "-device", "ich9-ahci,id=ahci",
        "-drive", f"id=d0,format=raw,file={DISK0},if=none",
        "-drive", f"id=d1,format=raw,file={DISK1},if=none",
        "-device", "ide-hd,drive=d0,bus=ahci.0",
        "-device", "ide-hd,drive=d1,bus=ahci.1",
        "-serial", f"file:{SER}",
        "-display", "none",
        "-qmp", f"unix:{QMP},server,nowait",
    ]
    return subprocess.Popen(args)

def connect():
    for _ in range(50):
        try:
            s = socket.socket(socket.AF_UNIX); s.connect(QMP); return s
        except OSError:
            time.sleep(0.2)
    raise RuntimeError("no QMP")

class Mon:
    def __init__(self):
        self.s = connect()
        self.f = self.s.makefile("rw")
        self.f.readline()
        self.cmd({"execute": "qmp_capabilities"})
    def cmd(self, obj):
        self.f.write(json.dumps(obj) + "\n"); self.f.flush()
        while True:
            line = self.f.readline()
            if not line: return None
            r = json.loads(line)
            if "return" in r or "error" in r: return r
    def shot(self, name):
        path = f"/tmp/{name}.ppm"
        self.cmd({"execute": "screendump", "arguments": {"filename": path}})
        time.sleep(0.5)
        return path
    def keyev(self, q, down):
        self.cmd({"execute": "input-send-event", "arguments": {
            "events": [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": q}}}]}})
    def tap(self, q, shift=False):
        if shift: self.keyev("shift", True)
        self.keyev(q, True); time.sleep(0.02); self.keyev(q, False)
        if shift: self.keyev("shift", False)
        time.sleep(0.05)
    def keystroke(self, ch):
        if ch.isalnum():
            self.tap(ch.lower(), shift=ch.isupper())
        else:
            q = QMAP.get(ch)
            if q: self.tap(q, shift=ch in "|>_")
    def typ(self, text):
        for ch in text:
            self.keystroke(ch)

def serial_text():
    try:
        with open(SER) as fh: return fh.read()
    except OSError:
        return ""

def wait_for(marker, timeout):
    """Poll the serial log until `marker` appears or `timeout` seconds pass."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker in serial_text():
            return True
        time.sleep(1.0)
    return False

# ---------------------------------------------------------------------------
# Boot A: fresh disks -> format, exercise ANSI + scrollback, save history.
# ---------------------------------------------------------------------------
print(">> Boot A: fresh disks")
fresh_disks()
try: os.remove(SER)
except OSError: pass
qemu = launch()
m = Mon()
print(f">> waiting {BOOT}s for desktop...")
time.sleep(BOOT)
m.shot("p1_a01_desktop")

print(">> [A] colors")
m.typ("colors\n"); time.sleep(1.0)
m.shot("p1_a02_colors")

print(">> [A] help (long output -> fills scrollback)")
m.typ("help\n"); time.sleep(1.5)
m.shot("p1_a03_help_bottom")

print(">> [A] PageUp x3 (scroll back into history)")
for _ in range(3):
    m.tap("pgup"); time.sleep(0.3)
time.sleep(0.5)
m.shot("p1_a04_scrolled_up")

print(">> [A] PageDown x5 (back to live bottom)")
for _ in range(5):
    m.tap("pgdn"); time.sleep(0.2)
time.sleep(0.5)
m.shot("p1_a05_scrolled_bottom")

print(">> [A] waiting for filesystem to finish first-boot format/self-test...")
if wait_for("SELFTEST PASSED", 60):
    print("   filesystem ready (selftest passed)")
else:
    print("   WARNING: selftest marker not seen; history save may fail")
time.sleep(2.0)

print(">> [A] type distinctive commands to persist in history")
m.typ("echo alpha-bravo\n"); time.sleep(0.8)
m.typ("echo charlie-delta\n"); time.sleep(0.8)
m.shot("p1_a06_typed")
# Give history_save() time to sync to the disk image.
time.sleep(2.0)

print(">> Boot A serial (history lines):")
for ln in serial_text().splitlines():
    if "history" in ln or "nextfs" in ln.lower():
        print("   ", ln)

print(">> shutting down Boot A")
m.cmd({"execute": "quit"})
time.sleep(2)
try: qemu.terminate()
except Exception: pass
time.sleep(2)

# ---------------------------------------------------------------------------
# Boot B: same disks -> history should load; ArrowUp recalls prior command.
# ---------------------------------------------------------------------------
print(">> Boot B: reuse disks (persistence check)")
try: os.remove(SER)
except OSError: pass
qemu = launch()
m = Mon()
print(f">> waiting {BOOT}s for desktop...")
time.sleep(BOOT)
m.shot("p1_b01_desktop")

print(">> [B] ArrowUp to recall persisted history")
m.tap("up"); time.sleep(0.4)
m.tap("up"); time.sleep(0.4)
m.shot("p1_b02_history_recall")
# Clear the recalled line so it does not execute.
m.tap("ret"); time.sleep(0.5)

txt = serial_text()
print(">> Boot B serial (history + mount lines):")
loaded_ok = False
for ln in txt.splitlines():
    low = ln.lower()
    if "history" in low or "persistent" in low or "mounted existing" in low:
        print("   ", ln)
        if "[history] loaded" in ln:
            # parse N
            try:
                n = int(ln.split("loaded")[1].split("entries")[0].strip())
                if n > 0: loaded_ok = True
            except Exception:
                pass

print(f">> RESULT: history persisted across reboot = {loaded_ok}")

print(">> shutting down Boot B")
m.cmd({"execute": "quit"})
time.sleep(1)
try: qemu.terminate()
except Exception: pass
print("DONE")
