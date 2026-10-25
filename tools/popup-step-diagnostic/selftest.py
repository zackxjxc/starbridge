"""Headless self-test. Does not launch the StarBridge application."""
import contextlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

import diagnostic as d


def check(condition, message):
  if not condition:
    raise AssertionError(message)


def main():
  with tempfile.TemporaryDirectory(prefix="popup-step-selftest-") as temporary:
    session = d.Session(Path(temporary) / "session")
    sibling = None
    worker = threading.Thread(target=d.serve, args=(session, False), daemon=True)
    try:
      check(not session.children and session.index == -1, "Tests must wait for clicks")
      rows = d.snapshot()
      check(any(row["pid"] == __import__("os").getpid() and row["path"] for row in rows), "Process snapshot")
      check(all(set(row) == {"pid", "parent_pid", "name", "path", "started_at"} for row in rows), "Metadata allowlist")
      worker.start()
      end = time.monotonic() + 5
      while not hasattr(session, "url") and time.monotonic() < end:
        time.sleep(0.05)
      base = session.url
      with d.OPENER.open(base, timeout=5) as response:
        check("下一步" in response.read().decode("utf-8"), "Chinese UI served")
      d.api(base, "next", {})
      d.api(base, "mark", {})
      d.api(base, "skip", {})
      d.api(base, "mark", {})
      check(session.index == 1 and not session.children, "One action per click, skip does not run component")
      marks = [event for event in session.events if event["kind"] == "popup_mark"]
      check(marks[-1]["current_action"]["step"] == 2 and marks[-1]["previous_action"]["step"] == 1, "Marker attribution")
      try:
        d.OPENER.open(urllib.request.Request(base + "next", data=b"{}", headers={"Origin": "https://example.invalid"}), timeout=5)
        raise AssertionError("Cross-origin mutation accepted")
      except urllib.error.HTTPError as error:
        check(error.code == 403, "Origin rejected")
      # A sibling process is outside the diagnostic job and must survive cleanup.
      sibling = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(120)"], creationflags=subprocess.CREATE_NO_WINDOW)
      session.launch("selftest-child", [sys.executable, "-c", "import time;time.sleep(120)"])
      child = session.children["selftest-child"][0]
      check(child.poll() is None, "Owned child running")
      with d.OPENER.open(base + "report", timeout=5) as response:
        report = response.read().decode("utf-8")
      check(session.password not in report and session.environment["STARBRIDGE_FRP_TOKEN"] not in report, "Report excludes credentials")
      check(json.loads(report)["events"], "Export is JSON")
      d.api(base, "stop", {})
      worker.join(timeout=5)
      check(not worker.is_alive() and child.poll() is not None and not session.children, "Server and owned child stopped")
      check(sibling.poll() is None, "Unowned sibling preserved")
      check(json.loads((session.output / "report.json").read_text(encoding="utf-8"))["events"][-1]["kind"] == "finished", "Final report retained")
      print("PASS: manual gating, metadata allowlist, HTTP UI/export, origin protection, markers, Job cleanup, sibling survival, credential exclusion")
    finally:
      session.cleanup()
      worker.join(timeout=5)
      if sibling:
        with contextlib.suppress(Exception):
          sibling.kill()
          sibling.wait(timeout=5)


if __name__ == "__main__":
  main()
