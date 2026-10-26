"""Manual, localhost-only popup isolation tool. Python standard library only."""
from __future__ import annotations

import argparse
import contextlib
import ctypes
from ctypes import wintypes
from datetime import datetime, timezone
import http.server
import hashlib
import json
import os
from pathlib import Path
import runpy
import secrets
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.request
import webbrowser

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
STEPS = [
  ("基线静置", "不启动组件。请等待约 20 秒，观察是否自行出现弹窗。"),
  ("FRPC 版本", "仅执行独立 runtime/frpc.exe --version，等待观察。"),
  ("FRPS 版本", "仅执行独立 runtime/frps.exe --version，等待观察。"),
  ("SSH 版本", "仅执行独立 runtime/ssh.exe -V，等待观察。"),
  ("SSHD 版本", "仅执行独立 runtime/sshd.exe -V，等待观察。"),
  ("打开未登录桌面窗口", "启动独立数据目录的 Tauri 窗口。不要输入真实账号；等待观察。"),
  ("关闭未登录窗口", "停止工具刚启动的桌面进程树，观察关闭时是否出现弹窗。"),
  ("启动隔离本地中心", "只启动临时本地 server + FRPS；HTTP 和 FRPS 都仅绑定 127.0.0.1。"),
  ("启动节点 A 后台", "只启动未登录 headless 客户端 A，观察后台启动路径。"),
  ("临时账号注册 / A 登录", "只向本地中心注册随机临时账号并让 A 登录；此动作可能初始化 SSH 组件。"),
  ("启动节点 B 后台", "只启动未登录 headless 客户端 B。"),
  ("节点 B 登录", "让 B 登录相同本地临时账号；不会操作真实账号。"),
  ("关闭 A 后台", "停止 A 后台进程树，为同一隔离数据目录切换到 GUI。"),
  ("打开已登录桌面窗口", "以 A 的临时登录状态启动 GUI，观察恢复登录 / WebView 路径。"),
  ("手动点击连接", "本步仅记录时间：切换到工具打开的客户端，对列表中的临时节点 B 点击连接一次，然后回来观察。不要改服务器或登录真实账号。"),
  ("手动终止连接", "本步仅记录时间：在测试客户端终止刚才建立的连接，确认本机空闲，再回来观察。尚无连接可用跳过按钮。"),
  ("手动点击测试", "本步仅记录时间：确认本机空闲，进入本机连接测试页并点击开始连接测试，然后回来观察。测试结束前不要推进。"),
  ("完成并清理", "停止本工具拥有的全部组件进程树并保留可导出的时间线报告。"),
]


def now():
  return datetime.now(timezone.utc).astimezone().isoformat(timespec="milliseconds")


def api(base, path, body=None):
  data = None if body is None else json.dumps(body).encode("utf-8")
  request = urllib.request.Request(base + path, data=data,
                                   headers={"Content-Type": "application/json"})
  with OPENER.open(request, timeout=20) as response:
    return json.load(response)


def free_ports(count):
  with contextlib.ExitStack() as stack:
    sockets = [stack.enter_context(socket.socket()) for _ in range(count)]
    for item in sockets:
      item.bind(("127.0.0.1", 0))
    return [item.getsockname()[1] for item in sockets]


def snapshot():
  """Read metadata only: never request process command lines or environment."""
  if os.name != "nt":
    return []
  class Entry(ctypes.Structure):
    _fields_ = [("size", wintypes.DWORD), ("usage", wintypes.DWORD),
                ("pid", wintypes.DWORD), ("heap", ctypes.c_size_t),
                ("module", wintypes.DWORD), ("threads", wintypes.DWORD),
                ("ppid", wintypes.DWORD), ("priority", wintypes.LONG),
                ("flags", wintypes.DWORD), ("name", wintypes.WCHAR * 260)]
  kernel = ctypes.WinDLL("kernel32", use_last_error=True)
  kernel.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
  kernel.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(Entry)]
  kernel.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(Entry)]
  kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
  kernel.OpenProcess.restype = wintypes.HANDLE
  kernel.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                              wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)]
  kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.c_void_p] * 4
  kernel.CloseHandle.argtypes = [wintypes.HANDLE]
  handle = kernel.CreateToolhelp32Snapshot(2, 0)
  if handle == wintypes.HANDLE(-1).value:
    raise ctypes.WinError(ctypes.get_last_error())
  rows = []
  try:
    entry = Entry()
    entry.size = ctypes.sizeof(entry)
    valid = kernel.Process32FirstW(handle, ctypes.byref(entry))
    while valid:
      row = {"pid": entry.pid, "parent_pid": entry.ppid, "name": entry.name,
             "path": None, "started_at": None}
      proc = kernel.OpenProcess(0x1000, False, entry.pid)
      if proc:
        try:
          buf, size = ctypes.create_unicode_buffer(32768), wintypes.DWORD(32768)
          if kernel.QueryFullProcessImageNameW(proc, 0, buf, ctypes.byref(size)):
            row["path"] = buf.value
          values = [ctypes.c_ulonglong() for _ in range(4)]
          if kernel.GetProcessTimes(proc, *(ctypes.byref(value) for value in values)):
            stamp = (values[0].value - 116444736000000000) / 10000000
            row["started_at"] = datetime.fromtimestamp(stamp, timezone.utc).isoformat()
        finally:
          kernel.CloseHandle(proc)
      rows.append(row)
      valid = kernel.Process32NextW(handle, ctypes.byref(entry))
  finally:
    kernel.CloseHandle(handle)
  return rows


class Session:
  def __init__(self, output=None):
    self.output = output or HERE / "output" / (datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + secrets.token_hex(3))
    self.output.mkdir(parents=True, exist_ok=False)
    self.private = self.output / "private-state-do-not-share"
    self.private.mkdir()
    self.events = []
    self.index = -1
    self.lock = threading.RLock()
    self.children = {}
    self.ready = False
    self.closed = False
    self.last_ping = time.monotonic()
    self.stop_event = threading.Event()
    self.ports = free_ports(4)
    self.center = f"http://127.0.0.1:{self.ports[0]}"
    self.nodes = [f"http://127.0.0.1:{port}" for port in self.ports[2:]]
    self.username = "popup-local-" + secrets.token_hex(5)
    self.password = secrets.token_urlsafe(24)
    self.environment = os.environ.copy()
    for key in list(self.environment):
      if key.lower() in {"http_proxy", "https_proxy", "all_proxy"} or key.startswith("STARBRIDGE_"):
        self.environment.pop(key)
    self.environment.update(NO_PROXY="127.0.0.1,localhost,::1", STARBRIDGE_SERVER_URL=self.center,
                            STARBRIDGE_FRP_TOKEN=secrets.token_urlsafe(24), RUST_LOG="off")
    self.record("session", detail="等待手动点击；启动工具不会启动任何测试组件。")

  def record(self, kind, **fields):
    with self.lock:
      self.events.append({"at": now(), "kind": kind, **fields})
      self.save()

  def save(self):
    with self.lock:
      report = {"format": 1, "timezone": "timestamps include UTC offset", "step": self.index,
                "limitations": "时间相关性不是因果证明；1秒进程采样可能漏掉短进程；无权限路径为null。未采集命令行、键盘、截图、密码或token。",
                "events": self.events}
      temp = self.output / "report.tmp"
      temp.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
      temp.replace(self.output / "report.json")

  def prepare(self):
    if self.ready:
      return
    required = [REPO / "target/release" / name for name in ("starbridge-client.exe", "starbridge-server.exe", "starbridge-ssh-session.exe")]
    required += [REPO / "state/runtime" / name for name in ("frpc.exe", "frps.exe", "ssh.exe", "sshd.exe")]
    if any(not path.is_file() for path in required):
      raise RuntimeError("缺少 release 客户端/服务端或 state/runtime 组件；请先完成构建。")
    shutil.copytree(REPO / "state/runtime", self.private / "runtime")
    for source, dest in ((required[0], "starbridge.exe"), (required[1], "starbridge-server.exe"),
                         (required[2], "starbridge-ssh-session.exe")):
      shutil.copy2(source, self.private / dest)
    for source in required:
      self.record("component_identity", name=source.name,
                  sha256=hashlib.sha256(source.read_bytes()).hexdigest(), size=source.stat().st_size)
    # Reuse the repository's proven stdlib Job Object implementation, never its test runner.
    self.job_class = runpy.run_path(str(REPO / "tools/test-peer-connectivity.py"))["WindowsJob"]
    self.ready = True

  def launch(self, name, command):
    self.prepare()
    flags = subprocess.CREATE_NO_WINDOW | 0x00000004  # CREATE_SUSPENDED
    proc = subprocess.Popen([str(value) for value in command], cwd=self.private,
                            env=self.environment, creationflags=flags,
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    job = None
    try:
      job = self.job_class(proc)
      native = ctypes.WinDLL("ntdll")
      native.NtResumeProcess.argtypes = [wintypes.HANDLE]
      native.NtResumeProcess.restype = wintypes.LONG
      if native.NtResumeProcess(int(proc._handle)) != 0:
        raise RuntimeError("无法恢复受 Job Object 保护的进程")
      self.children[name] = (proc, job)
      self.record("launched", component=name, pid=proc.pid)
    except BaseException:
      proc.kill()
      proc.wait(timeout=5)
      if job:
        job.close()
      raise

  def stop(self, name):
    item = self.children.pop(name, None)
    if item:
      proc, job = item
      job.close()
      proc.wait(timeout=10)
      self.record("stopped", component=name, pid=proc.pid, exit_code=proc.returncode)

  def wait_api(self, base, path):
    end = time.monotonic() + 25
    while time.monotonic() < end:
      try:
        api(base, path)
        return
      except Exception:
        time.sleep(0.3)
    raise RuntimeError("本地组件未在25秒内就绪；请标记并结束，查看报告中的步骤与退出码。")

  def advance(self, skip=False):
    with self.lock:
      if self.closed or self.index + 1 >= len(STEPS):
        return
      self.index += 1
      self.record("action", step=self.index + 1, title=STEPS[self.index][0], skipped=skip)
    if skip:
      return
    try:
      self.perform(self.index)
      self.record("action_done", step=self.index + 1)
    except Exception as error:
      # Never serialize API bodies, HTTP error text, environment, or credentials.
      self.record("error", step=self.index + 1, error_type=type(error).__name__,
                  detail=str(error) if isinstance(error, RuntimeError) else "动作失败；未记录可能含凭据的底层错误正文。")

  def perform(self, step):
    if step == 0:
      return
    self.prepare()
    root = self.private
    if 1 <= step <= 4:
      component = ["frpc", "frps", "ssh", "sshd"][step - 1]
      self.launch(component, [root / "runtime" / (component + ".exe"), "--version" if step <= 2 else "-V"])
      proc = self.children[component][0]
      try:
        proc.wait(timeout=8)
      except subprocess.TimeoutExpired:
        self.record("version_timeout", component=component)
      self.stop(component)
    elif step in (5, 13):
      self.launch("gui", [root / "starbridge.exe", "--data-dir", root / "node-a"])
    elif step == 6:
      self.stop("gui")
    elif step == 7:
      self.launch("center", [root / "starbridge-server.exe", "--listen", self.center[7:],
                              "--data-dir", root / "center", "--frps-path", root / "runtime/frps.exe",
                              "--frps-port", self.ports[1], "--frps-addr", "127.0.0.1", "--frps-bind-addr", "127.0.0.1"])
      self.wait_api(self.center, "/api/health")
    elif step in (8, 10):
      index = 0 if step == 8 else 1
      self.launch("node-" + "ab"[index], [root / "starbridge.exe", "--headless", "--listen", self.nodes[index][7:],
                                          "--data-dir", root / ("node-" + "ab"[index])])
      self.wait_api(self.nodes[index], "/api/v1/state")
    elif step in (9, 11):
      index = 0 if step == 9 else 1
      body = {"username": self.username, "password": self.password, "remember": True}
      if step == 9:
        body["confirm_password"] = self.password
      api(self.nodes[index], "/api/v1/register" if step == 9 else "/api/v1/login", body)
    elif step == 12:
      self.stop("node-a")
    elif step == 17:
      self.cleanup()

  def mark(self):
    with self.lock:
      actions = [item for item in self.events if item["kind"] == "action"]
      self.record("popup_mark", current_action=actions[-1] if actions else None,
                  previous_action=actions[-2] if len(actions) > 1 else None)

  def state(self):
    with self.lock:
      return {"index": self.index, "steps": STEPS, "closed": self.closed,
              "output": str(self.output), "events": list(self.events[-70:])}

  def cleanup(self):
    with self.lock:
      if self.closed:
        return
      self.closed = True
      self.stop_event.set()
    for name in reversed(list(self.children)):
      try:
        self.stop(name)
      except Exception as error:
        self.record("cleanup_error", component=name, error_type=type(error).__name__)
    self.record("finished", detail="已关闭本工具拥有的 Job Objects；未按进程名结束其他实例。")

  def monitor(self):
    sampling = True
    try:
      seen = {(row["pid"], row["started_at"]) for row in snapshot()}
    except Exception as error:
      self.record("monitor_unavailable", error_type=type(error).__name__)
      seen = set()
      sampling = False
    while not self.stop_event.wait(1):
      if time.monotonic() - self.last_ping > 60:
        self.cleanup()
        return
      if not sampling:
        continue
      try:
        rows = snapshot()
        current = {(row["pid"], row["started_at"]) for row in rows}
        for row in rows:
          if (row["pid"], row["started_at"]) not in seen:
            self.record("new_process", **row)
        seen = current
      except Exception as error:
        self.record("monitor_error", error_type=type(error).__name__)
        sampling = False  # Keep the independent browser-heartbeat cleanup alive.


def serve(session, open_browser=True):
  token = secrets.token_urlsafe(24)
  action_lock = threading.Lock()

  class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *unused):
      pass

    def respond(self, body, content_type="application/json; charset=utf-8"):
      self.send_response(200)
      self.send_header("Content-Type", content_type)
      self.send_header("Cache-Control", "no-store")
      self.send_header("X-Content-Type-Options", "nosniff")
      self.end_headers()
      with contextlib.suppress(BrokenPipeError, ConnectionResetError):
        self.wfile.write(body)

    def authorized(self):
      if self.path.split("?", 1)[0] != "/" + token + "/" + self.path.rsplit("/", 1)[-1]:
        self.send_error(403)
        return False
      if self.headers.get("Host") != f"127.0.0.1:{server.server_port}":
        self.send_error(403)
        return False
      return True

    def do_GET(self):
      if not self.authorized():
        return
      endpoint = self.path.rsplit("/", 1)[-1]
      if endpoint == "":
        self.respond((HERE / "index.html").read_bytes(), "text/html; charset=utf-8")
      elif endpoint == "state":
        session.last_ping = time.monotonic()
        self.respond(json.dumps(session.state(), ensure_ascii=False).encode("utf-8"))
      elif endpoint == "report":
        session.save()
        self.respond((session.output / "report.json").read_bytes())
      else:
        self.send_error(404)

    def do_POST(self):
      if not self.authorized():
        return
      origin = self.headers.get("Origin")
      if origin and origin != f"http://127.0.0.1:{server.server_port}":
        self.send_error(403)
        return
      endpoint = self.path.rsplit("/", 1)[-1]
      if endpoint == "mark":
        session.mark()
      elif endpoint in ("next", "skip", "stop"):
        if not action_lock.acquire(blocking=False):
          self.send_error(409)
          return
        try:
          if endpoint == "stop":
            session.cleanup()
          else:
            session.advance(skip=endpoint == "skip")
        finally:
          action_lock.release()
      else:
        self.send_error(404)
        return
      self.respond(b"{}")

  server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
  url = f"http://127.0.0.1:{server.server_port}/{token}/"
  session.url = url
  session.record("browser_ready", detail="本机页面就绪；随机访问路径不写入报告。")
  print("Open local diagnostic page: " + url, flush=True)
  threading.Thread(target=session.monitor, daemon=True).start()
  if open_browser:
    webbrowser.open(url)
  server.timeout = 1
  try:
    while not session.closed:
      server.handle_request()
  except KeyboardInterrupt:
    pass
  finally:
    session.cleanup()
    # Allow the last response to finish before closing its listener.
    server.server_close()


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--no-browser", action="store_true")
  options = parser.parse_args()
  if os.name != "nt":
    parser.error("This diagnostic tool requires Windows.")
  session = Session()
  try:
    serve(session, not options.no_browser)
  finally:
    session.cleanup()


if __name__ == "__main__":
  main()
