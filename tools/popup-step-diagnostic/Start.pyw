"""AI-authored double-click launcher; requires installed Python 3.10+."""
from pathlib import Path
import runpy
import traceback
import ctypes

try:
  runpy.run_path(str(Path(__file__).with_name("diagnostic.py")), run_name="__main__")
except Exception:
  ctypes.windll.user32.MessageBoxW(None, traceback.format_exc(), "Popup diagnostic startup error", 0x10)
