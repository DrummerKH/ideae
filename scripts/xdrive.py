"""Drives an X display through XTest; see drive.sh for the steps."""
import ctypes, sys, time, subprocess, os
x11 = ctypes.CDLL("libX11.so.6"); xt = ctypes.CDLL("libXtst.so.6")
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
for f in (xt.XTestFakeMotionEvent, xt.XTestFakeKeyEvent, xt.XTestFakeButtonEvent, x11.XFlush, x11.XKeysymToKeycode):
    f.argtypes = None
d = x11.XOpenDisplay(os.environ["DISPLAY"].encode())
dp = ctypes.c_void_p(d)
KEYS = {"ctrl": 0xffe3, "shift": 0xffe1, "alt": 0xffe9}
def code(name): return x11.XKeysymToKeycode(dp, ctypes.c_ulong(KEYS[name]))
for step in sys.argv[1:]:
    op, _, arg = step.partition(":")
    if op == "move":
        x, y = map(int, arg.split(","))
        xt.XTestFakeMotionEvent(dp, -1, x, y, ctypes.c_ulong(0))
    elif op in ("down", "up"):
        xt.XTestFakeKeyEvent(dp, code(arg), op == "down", ctypes.c_ulong(0))
    elif op == "wheel":
        b = 5 if arg != "up" else 4
        for _ in range(3):
            xt.XTestFakeButtonEvent(dp, b, True, ctypes.c_ulong(0)); xt.XTestFakeButtonEvent(dp, b, False, ctypes.c_ulong(0)); x11.XFlush(dp); time.sleep(0.03)
    elif op in ("press", "release"):
        xt.XTestFakeButtonEvent(dp, 1, op == "press", ctypes.c_ulong(0))
    elif op in ("click", "rclick", "mclick"):
        # click:8 and click:9 are the mouse's Back and Forward buttons.
        b = int(arg) if arg else {"click": 1, "rclick": 3, "mclick": 2}[op]
        xt.XTestFakeButtonEvent(dp, b, True, ctypes.c_ulong(0)); x11.XFlush(dp); time.sleep(0.05)
        xt.XTestFakeButtonEvent(dp, b, False, ctypes.c_ulong(0))
    elif op == "wait":
        x11.XFlush(dp); time.sleep(float(arg))
    elif op == "type":
        # lowercase ascii and '_' for space (steps are space-separated)
        for ch in arg:
            sym = 0x20 if ch == "_" else ord(ch)
            kc = x11.XKeysymToKeycode(dp, ctypes.c_ulong(sym))
            xt.XTestFakeKeyEvent(dp, kc, True, ctypes.c_ulong(0)); x11.XFlush(dp); time.sleep(0.02)
            xt.XTestFakeKeyEvent(dp, kc, False, ctypes.c_ulong(0)); x11.XFlush(dp); time.sleep(0.02)
    elif op == "key":
        sym = {"escape": 0xff1b, "tab": 0xff09, "backspace": 0xff08, "f12": 0xffc9, "pageup": 0xff55, "pagedown": 0xff56, "down": 0xff54, "up": 0xff52, "right": 0xff53, "left": 0xff51, "return": 0xff0d}[arg]
        kc = x11.XKeysymToKeycode(dp, ctypes.c_ulong(sym))
        xt.XTestFakeKeyEvent(dp, kc, True, ctypes.c_ulong(0)); x11.XFlush(dp); time.sleep(0.02)
        xt.XTestFakeKeyEvent(dp, kc, False, ctypes.c_ulong(0))
    elif op == "enter":
        kc = x11.XKeysymToKeycode(dp, ctypes.c_ulong(0xff0d))
        xt.XTestFakeKeyEvent(dp, kc, True, ctypes.c_ulong(0)); x11.XFlush(dp); time.sleep(0.02)
        xt.XTestFakeKeyEvent(dp, kc, False, ctypes.c_ulong(0))
    elif op == "sh":
        subprocess.run(["bash", arg], check=False)
    elif op == "shot":
        x11.XFlush(dp); subprocess.run(["import", "-window", "root", arg], check=True)
    x11.XFlush(dp)
