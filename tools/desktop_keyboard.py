#!/usr/bin/env python3
"""Inject physical Russian-layout keys into the disposable X11 smoke display."""
import ctypes
import os
import subprocess
import time

if os.environ.get('DISPLAY') != ':193':
    raise SystemExit('disposable smoke display required')
subprocess.run(['setxkbmap', '-layout', 'ru'], check=True)
if os.environ.get('AIBRIDGE_DESKTOP_IBUS_KEYS'):
    subprocess.run(['ibus', 'engine', 'xkb:ru::rus'], check=True)
    time.sleep(.2)
x11 = ctypes.CDLL('libX11.so.6')
xtst = ctypes.CDLL('libXtst.so.6')
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XFlush.argtypes = [ctypes.c_void_p]
xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
display = x11.XOpenDisplay(None)
if not display:
    raise SystemExit('smoke display unavailable')
# Physical keycodes for "отправь задачу" on the standard Russian layout.
for code in [44, 57, 42, 43, 41, 40, 58, 65, 33, 41, 46, 41, 53, 26]:
    xtst.XTestFakeKeyEvent(display, code, 1, 0)
    xtst.XTestFakeKeyEvent(display, code, 0, 0)
    x11.XFlush(display)
    time.sleep(.03)
time.sleep(.1)
subprocess.run(['setxkbmap', '-layout', 'us'], check=True)
if os.environ.get('AIBRIDGE_DESKTOP_IBUS_KEYS'):
    subprocess.run(['ibus', 'engine', 'xkb:us::eng'], check=True)
