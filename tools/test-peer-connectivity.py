#!/usr/bin/env python3
"""Run real, isolated FRP/SSH ownership regressions using Python's stdlib.

Build first: cargo build -p starbridge-client -p starbridge-server --no-default-features
Run: python tools/test-peer-connectivity.py
Other paths: add --policy direct_preferred or --policy direct_only.
Use --check-only for dependency checks or --keep-work-dir to retain diagnostics.
Exit codes: 0 passed, 1 regression, 2 setup/dependency error, 130 interrupted.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import contextlib
import ctypes
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


REPO = Path(__file__).resolve().parents[1]
SUFFIX = ".exe" if os.name == "nt" else ""
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
ACTIVE = {"negotiating", "connected"}


class RegressionError(Exception):
  pass


class FatalRegressionError(RegressionError):
  pass


class SetupError(Exception):
  pass


class WindowsJob:
  """Closing a job kills its complete process tree, including orphaned FRP/SSH."""

  def __init__(self, process):
    from ctypes import wintypes

    class BasicLimits(ctypes.Structure):
      _fields_ = [("process_time", ctypes.c_longlong),
                  ("job_time", ctypes.c_longlong), ("flags", wintypes.DWORD),
                  ("min_working_set", ctypes.c_size_t),
                  ("max_working_set", ctypes.c_size_t),
                  ("active_processes", wintypes.DWORD),
                  ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                  ("scheduling", wintypes.DWORD)]

    class IoCounters(ctypes.Structure):
      _fields_ = [(name, ctypes.c_ulonglong) for name in (
        "read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes")]

    class ExtendedLimits(ctypes.Structure):
      _fields_ = [("basic", BasicLimits), ("io", IoCounters),
                  ("process_memory", ctypes.c_size_t),
                  ("job_memory", ctypes.c_size_t),
                  ("peak_process_memory", ctypes.c_size_t),
                  ("peak_job_memory", ctypes.c_size_t)]

    self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    self.kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    self.kernel.CreateJobObjectW.restype = wintypes.HANDLE
    self.kernel.SetInformationJobObject.argtypes = [
      wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    self.kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    self.handle = self.kernel.CreateJobObjectW(None, None)
    if not self.handle:
      raise ctypes.WinError(ctypes.get_last_error())
    limits = ExtendedLimits()
    limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    try:
      if not self.kernel.SetInformationJobObject(
          self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
        raise ctypes.WinError(ctypes.get_last_error())
      if not self.kernel.AssignProcessToJobObject(self.handle, int(process._handle)):
        raise ctypes.WinError(ctypes.get_last_error())
    except BaseException:
      self.close()
      raise

  def close(self):
    if self.handle:
      self.kernel.CloseHandle(self.handle)
      self.handle = None


class ManagedProcess:
  def __init__(self, name, argv, log_path, environment):
    self.name = name
    self.log = log_path.open("ab")
    self.job = None
    options = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {
      "start_new_session": True}
    try:
      self.process = subprocess.Popen(
        [str(value) for value in argv], cwd=REPO, env=environment,
        stdin=subprocess.DEVNULL, stdout=self.log, stderr=subprocess.STDOUT, **options)
      if os.name == "nt":
        self.job = WindowsJob(self.process)
    except BaseException:
      if hasattr(self, "process"):
        self.process.kill()
        self.process.wait()
      self.log.close()
      raise

  def check(self):
    code = self.process.poll()
    if code is not None:
      raise RegressionError(f"{self.name} exited unexpectedly with code {code}")

  def stop(self):
    if self.job:
      self.job.close()
      self.job = None
    elif os.name != "nt":
      with contextlib.suppress(ProcessLookupError):
        os.killpg(self.process.pid, signal.SIGTERM)
    try:
      self.process.wait(timeout=5)
    except subprocess.TimeoutExpired:
      self.process.kill()
      self.process.wait(timeout=5)
    if os.name != "nt":
      with contextlib.suppress(ProcessLookupError):
        os.killpg(self.process.pid, signal.SIGKILL)
    self.log.close()


def api(url, path, method="GET", body=None, token=None):
  headers = {"Content-Type": "application/json"}
  if token:
    headers["Authorization"] = f"Bearer {token}"
  payload = json.dumps(body).encode("utf-8") if body is not None else None
  request = urllib.request.Request(url + path, data=payload, headers=headers, method=method)
  try:
    with OPENER.open(request, timeout=15) as response:
      return json.load(response)
  except urllib.error.HTTPError as error:
    try:
      message = json.load(error).get("error", f"HTTP {error.code}")
    except (ValueError, AttributeError):
      message = f"HTTP {error.code}"
    raise RegressionError(f"{method} {path}: {message}") from None


def wait_for(description, predicate, processes, timeout):
  deadline = time.monotonic() + timeout
  last_error = None
  while time.monotonic() < deadline:
    for process in processes:
      process.check()
    try:
      value = predicate()
      if value:
        return value
    except FatalRegressionError:
      raise
    except (OSError, ValueError, RegressionError) as error:
      last_error = error
    time.sleep(0.3)
  suffix = f": {last_error}" if last_error else ""
  raise RegressionError(f"Timed out waiting for {description}{suffix}")


def free_ports(count):
  with contextlib.ExitStack() as stack:
    sockets = [stack.enter_context(socket.socket()) for _ in range(count)]
    for listener in sockets:
      listener.bind(("127.0.0.1", 0))
    return [listener.getsockname()[1] for listener in sockets]


def active_sessions(snapshot):
  if not isinstance(snapshot.get("sessions"), list):
    raise RegressionError("Snapshot lacks sessions[]; rebuild the peer-capable client")
  return [session for session in snapshot["sessions"] if session["status"] in ACTIVE]


def ssh_result(runtime, command, marker):
  argv = shlex.split(command)
  if os.name == "nt" and argv and argv[0] == "&":
    argv = argv[1:]
  if not argv or Path(argv[0]).name.lower() not in {"ssh", "ssh.exe"}:
    raise RegressionError("Unexpected SSH command in the local snapshot")
  options = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
  try:
    result = subprocess.run([
      ssh_executable(runtime), "-T", "-o", "BatchMode=yes",
      "-o", "ConnectionAttempts=1", "-o", "ConnectTimeout=4",
      "-o", "ServerAliveInterval=2", "-o", "ServerAliveCountMax=2",
      *argv[1:], f"echo {marker}"],
      stdin=subprocess.DEVNULL, capture_output=True, text=True,
      encoding="utf-8", errors="replace", timeout=10, **options)
    return result.returncode == 0 and marker in result.stdout, result.stderr.strip()[-600:]
  except subprocess.TimeoutExpired:
    return False, "SSH command timed out"


class PeerRegression:
  def __init__(self, args, work):
    self.args = args
    self.work = work
    self.runtime = work / "runtime"
    self.processes = []
    self.environment = os.environ.copy()
    for key in list(self.environment):
      if key.lower() in {"http_proxy", "https_proxy", "all_proxy"}:
        self.environment.pop(key)
    self.environment["NO_PROXY"] = "127.0.0.1,localhost,::1"
    self.environment.pop("STARBRIDGE_SSH_PORT", None)
    self.environment["STARBRIDGE_FRP_TOKEN"] = secrets.token_urlsafe(24)
    self.environment["RUST_LOG"] = "info"
    self.username = "peer-test-" + secrets.token_hex(6)
    self.password = secrets.token_urlsafe(24)
    ports = free_ports(5)
    self.center = f"http://127.0.0.1:{ports[0]}"
    self.frps_port = ports[1]
    self.urls = [f"http://127.0.0.1:{port}" for port in ports[2:]]
    self.nodes = [None, None, None]
    self.token = None

  def launch(self, name, command):
    process = ManagedProcess(name, command, self.work / f"{name}.log", self.environment)
    self.processes.append(process)
    return process

  def start_node(self, index):
    name = "node-" + "abc"[index]
    self.nodes[index] = self.launch(name, [
      self.args.client, "--headless", "--listen", self.urls[index][7:],
      "--data-dir", self.work / name])
    wait_for(f"{name} HTTP console", lambda: self.state(index),
             self.processes, self.args.timeout)

  def state(self, index):
    return api(self.urls[index], "/api/v1/state")

  def wait_sessions(self, ids):
    expected = set(ids)

    def synchronized():
      snapshots = [self.state(index) for index in range(3)]
      summary = api(self.center, "/api/v1/admin/summary", token=self.token)
      records = [session for session in summary["sessions"]
                 if session["session_id"] in expected and session["status"] in ACTIVE]
      if len(records) != len(expected):
        return False
      return snapshots if all(
        {session["session_id"] for session in active_sessions(snapshot)} == {
          session["session_id"] for session in records
          if snapshot["node_id"] in (session["initiator_node_id"], session["target_node_id"])
        } for snapshot in snapshots) else False

    return wait_for(f"nodes to report {len(expected)} active session(s) with correct ownership",
                    synchronized, self.processes, self.args.timeout)

  def visible(self):
    def synchronized():
      snapshots = [self.state(index) for index in range(3)]
      ids = {snapshot["node_id"] for snapshot in snapshots}
      return snapshots if all(
        snapshot["logged_in"] and snapshot["username"] == self.username
        and ids <= {device["node_id"] for device in snapshot["devices"] if device["online"]}
        for snapshot in snapshots) else False

    return wait_for("three logged-in peers to see each other online", synchronized,
                    self.processes, self.args.timeout)

  def connect(self, index, target):
    api(self.urls[index], "/api/v1/connect", "POST", {
      "target_node_id": target, "policy": self.args.policy})

    def outbound():
      snapshot = self.state(index)
      sessions = [session for session in active_sessions(snapshot)
                  if session["initiator_node_id"] == snapshot["node_id"]
                  and session["target_node_id"] == target]
      if len(sessions) > 1:
        raise FatalRegressionError("Repeated connect created multiple active outbound sessions")
      if (sessions and sessions[0]["status"] == "connected"
          and sessions[0].get("ssh_ready") and sessions[0].get("ssh_command")):
        if sessions[0]["policy"] != self.args.policy:
          raise FatalRegressionError("The effective session policy changed unexpectedly")
        if self.args.policy == "relay_only" and sessions[0]["path"] != "relay":
          raise FatalRegressionError("relay_only session did not use the relay path")
        if self.args.policy == "direct_only" and sessions[0]["path"] == "relay":
          raise FatalRegressionError("direct_only session used a forbidden relay path")
        command = shlex.split(sessions[0]["ssh_command"])
        config = Path(command[command.index("-F") + 1]).read_text(encoding="utf-8")
        sessions[0]["_visitor_port"] = next(
          int(line.split()[1]) for line in config.splitlines()
          if line.strip().lower().startswith("port "))
        sessions[0]["_source_index"] = index
        return sessions[0]
      return False

    return wait_for("connected outbound SSH session", outbound,
                    self.processes, self.args.timeout)

  def verify_ssh(self, sessions):
    def execute(item):
      label, session = item
      marker = "STARBRIDGE_" + secrets.token_hex(10)

      def attempt():
        success, error = ssh_result(self.runtime, session["ssh_command"], marker)
        if not success:
          snapshot = self.state(session["_source_index"])
          current = next((item for item in snapshot["sessions"]
                          if item["session_id"] == session["session_id"]), {})
          raise RegressionError(f"{label}: {error or 'SSH marker missing'}; "
                                f"policy={self.args.policy}, path={current.get('path', 'absent')}, "
                                f"stage={snapshot['stage']}, reason={current.get('failure_reason')}")
        return True

      wait_for(f"real SSH command {label}", attempt, self.processes, self.args.timeout)

      def resolved_path():
        current = next((item for item in self.state(session["_source_index"])["sessions"]
                        if item["session_id"] == session["session_id"]), None)
        if not current:
          return False
        allowed = {"relay"} if self.args.policy == "relay_only" else (
          {"direct"} if self.args.policy == "direct_only" else {"direct", "relay"})
        if current["path"] not in allowed and current["path"] != "unknown":
          raise FatalRegressionError(f"{self.args.policy} used forbidden path={current['path']}")
        if current["path"] not in allowed:
          return False
        session["path"] = current["path"]
        return current["path"]

      path = wait_for(f"actual {self.args.policy} path for {label}", resolved_path,
                      self.processes, self.args.timeout)
      print(f"PASS: {label} actual path={path}", flush=True)

    with ThreadPoolExecutor(max_workers=len(sessions)) as executor:
      list(executor.map(execute, sessions))

  def disconnect(self, index, session):
    api(self.urls[index], "/api/v1/sessions/" + session["session_id"], "DELETE")

  def assert_occupied(self, index, target):
    identity = json.loads((self.work / ("node-" + "abc"[index]) / "identity.json").read_text(encoding="utf-8"))
    try:
      api(self.center, "/api/v1/sessions", "POST", {
        "initiator_node_id": identity["node_id"], "device_token": identity["device_token"],
        "target_node_id": target, "policy": self.args.policy}, token=self.token)
    except RegressionError as error:
      if not any(word in str(error).lower() for word in ("占用", "连接", "控制", "busy", "occupied", "incoming")):
        raise RegressionError(f"Expected an occupancy rejection, received: {error}") from None
      return
    raise RegressionError("The center accepted a session involving an occupied device")

  def assert_closed_route(self, session):
    try:
      connection = socket.create_connection(("127.0.0.1", session["_visitor_port"]), timeout=1)
    except OSError:
      pass
    else:
      connection.close()
      raise RegressionError("A disconnected SSH visitor port is still listening")
    success, _ = ssh_result(self.runtime, session["ssh_command"], "SHOULD_NOT_RUN")
    if success:
      raise RegressionError("A disconnected SSH route still accepts commands")

  def center_idle(self):
    summary = api(self.center, "/api/v1/admin/summary", token=self.token)
    if any(session["status"] in ACTIVE for session in summary["sessions"]):
      raise RegressionError("The center retained an active superseded/disconnected session")

  def verify_offline_disconnect(self, target):
    session = self.connect(0, target)
    self.wait_sessions([session["session_id"]])
    self.verify_ssh([("A -> B before center outage", session)])
    config = api(self.urls[0], "/api/v1/config")
    # Keep this unused port bound without listening so no unrelated service can claim it.
    with socket.socket() as unavailable:
      unavailable.bind(("127.0.0.1", 0))
      offline_config = dict(config, server_url=f"http://127.0.0.1:{unavailable.getsockname()[1]}")
      try:
        api(self.urls[0], "/api/v1/config", "PUT", offline_config)
        self.disconnect(0, session)
        self.assert_closed_route(session)
        if active_sessions(self.state(0)):
          raise RegressionError("The offline disconnect retained a local active session")
        pending = json.loads((self.work / "node-a" / "pending-closes.json").read_text(encoding="utf-8"))
        if session["session_id"] not in pending:
          raise RegressionError("The offline disconnect did not persist its pending revocation")
      finally:
        api(self.urls[0], "/api/v1/config", "PUT", config)
    self.wait_sessions([])
    self.center_idle()
    pending = json.loads((self.work / "node-a" / "pending-closes.json").read_text(encoding="utf-8"))
    if session["session_id"] in pending:
      raise RegressionError("The restored center did not acknowledge the pending revocation")
    self.assert_closed_route(session)
    print("PASS: offline disconnect closes the listener and revokes the session after recovery", flush=True)

  def log_secret_leaks(self):
    sensitive = {self.password, self.environment["STARBRIDGE_FRP_TOKEN"], self.token}

    def collect(value):
      if isinstance(value, dict):
        for key, child in value.items():
          if key in {"access_token", "refresh_token", "device_token", "secret_key", "auth_token", "password"}:
            if isinstance(child, str):
              sensitive.add(child)
          collect(child)
      elif isinstance(value, list):
        for child in value:
          collect(child)

    for path in self.work.rglob("*.json"):
      with contextlib.suppress(OSError, ValueError):
        collect(json.loads(path.read_text(encoding="utf-8")))
    sensitive = {value for value in sensitive if value and len(value) >= 8}
    leaks = []
    for path in self.work.rglob("*.log"):
      content = path.read_text(encoding="utf-8", errors="replace")
      if any(value in content for value in sensitive):
        leaks.append(str(path.relative_to(self.work)))
    return leaks

  def run(self):
    shutil.copytree(self.args.runtime, self.runtime)
    self.launch("center", [
      self.args.server, "--listen", self.center[7:], "--frps-path",
      self.runtime / ("frps" + SUFFIX), "--frps-port", self.frps_port,
      "--frps-addr", "127.0.0.1", "--frps-bind-addr", "127.0.0.1",
      "--data-dir", self.work / "center"])
    wait_for("center HTTP service", lambda: api(self.center, "/api/health"),
             self.processes, self.args.timeout)
    self.token = api(self.center, "/api/v1/auth/register", "POST", {
      "username": self.username, "password": self.password})["access_token"]
    for index in range(3):
      self.start_node(index)
      config = api(self.urls[index], "/api/v1/config")
      config.update({"server_url": self.center, "display_name": "Peer Test " + "ABC"[index],
                     "ssh_alias": "peer-test-" + "abc"[index],
                     "allow_relay": True, "direct_only": False})
      api(self.urls[index], "/api/v1/config", "PUT", config)
      api(self.urls[index], "/api/v1/login", "POST", {
        "username": self.username, "password": self.password, "remember": True})
    self.token = api(self.center, "/api/v1/auth/login", "POST", {
      "username": self.username, "password": self.password})["access_token"]
    snapshots = self.visible()
    ids = [snapshot["node_id"] for snapshot in snapshots]
    if len(set(ids)) != 3:
      raise RegressionError("Independent data directories produced the same node identity")
    print("PASS: A, B and C see the same three online peers", flush=True)

    api(self.urls[0], "/api/v1/connect/all", "POST", {
      "policy": self.args.policy, "rebuild_relay": False})
    ab = self.connect(0, ids[1])
    ac = self.connect(0, ids[2])
    if ab["_visitor_port"] == ac["_visitor_port"]:
      raise RegressionError("Parallel connections allocated the same local SSH visitor port")
    self.wait_sessions([ab["session_id"], ac["session_id"]])
    self.verify_ssh([("parallel A -> B", ab), ("parallel A -> C", ac)])
    self.disconnect(0, ac)
    self.wait_sessions([ab["session_id"]])
    self.assert_occupied(1, ids[0])
    self.assert_occupied(1, ids[2])
    self.assert_occupied(2, ids[0])
    self.assert_occupied(2, ids[1])
    ac = self.connect(0, ids[2])
    self.wait_sessions([ab["session_id"], ac["session_id"]])
    self.verify_ssh([("A -> B", ab), ("A -> C", ac)])
    print("PASS: A controls B/C while incoming and occupied-target requests are rejected", flush=True)
    keys = [(self.work / ("node-" + name) / "ssh" / filename).read_bytes()
            for name in "abc" for filename in ("id_ed25519.pub", "host_ed25519.pub")]

    self.disconnect(0, ab)
    self.wait_sessions([ac["session_id"]])
    self.assert_closed_route(ab)
    self.verify_ssh([("A -> C after A -> B terminates", ac)])
    print("PASS: terminating A -> B preserves A -> C", flush=True)

    first_repeat = self.connect(0, ids[2])
    second_repeat = self.connect(0, ids[2])
    if (first_repeat["session_id"] != ac["session_id"]
        or second_repeat["session_id"] != ac["session_id"]
        or second_repeat["_visitor_port"] != ac["_visitor_port"]
        or second_repeat["ssh_command"] != ac["ssh_command"]):
      raise RegressionError("Same-direction reuse replaced the session, port or SSH configuration")
    self.verify_ssh([("A -> C after repeated connect", second_repeat)])
    api(self.urls[0], "/api/v1/terminate", "POST", {"target_node_id": None})
    self.wait_sessions([])
    self.center_idle()
    api(self.urls[0], "/api/v1/connect/all", "POST", {
      "policy": self.args.policy, "rebuild_relay": False})
    api(self.urls[0], "/api/v1/terminate", "POST", {"target_node_id": None})
    self.wait_sessions([])
    print(f"Checking for stale-session revival for {self.args.settle_seconds:g}s...", flush=True)
    deadline = time.monotonic() + self.args.settle_seconds
    while time.monotonic() < deadline:
      for index in range(3):
        if active_sessions(self.state(index)):
          raise RegressionError("A disconnected/superseded session revived after a heartbeat")
      self.center_idle()
      time.sleep(0.5)
    for session in {item["session_id"]: item for item in (ab, ac, first_repeat, second_repeat)}.values():
      self.assert_closed_route(session)
    print("PASS: repeated connect leaves no active or revived old session", flush=True)

    self.verify_offline_disconnect(ids[1])

    identities = [(self.work / ("node-" + name) / "identity.json").read_bytes() for name in "abc"]
    for process in self.nodes:
      process.stop()
      self.processes.remove(process)
    for index in range(3):
      self.start_node(index)
    restored = self.visible()
    if [snapshot["node_id"] for snapshot in restored] != ids:
      raise RegressionError("Node identity changed after restart")
    if [(self.work / ("node-" + name) / "ssh" / filename).read_bytes()
        for name in "abc" for filename in ("id_ed25519.pub", "host_ed25519.pub")] != keys:
      raise RegressionError("SSH client/host identity changed after restart")
    if [(self.work / ("node-" + name) / "identity.json").read_bytes() for name in "abc"] != identities:
      raise RegressionError("Persisted device credentials changed after restart")
    self.wait_sessions([])
    ba = self.connect(1, ids[0])
    self.wait_sessions([ba["session_id"]])
    self.verify_ssh([("B -> A after restart, when both devices are idle", ba)])
    self.disconnect(1, ba)
    self.wait_sessions([])
    self.center_idle()
    print("PASS: restart restores login/device identity; an idle device can reverse the connection direction", flush=True)

  def close(self):
    failure = None
    for process in reversed(self.processes):
      try:
        process.stop()
      except (OSError, subprocess.TimeoutExpired) as error:
        failure = error
    if failure:
      raise RegressionError(f"Process cleanup failed: {failure}")


def ssh_executable(runtime):
  bundled = runtime / ("ssh" + SUFFIX)
  if bundled.is_file():
    return str(bundled)
  system = shutil.which("ssh") if os.name != "nt" else None
  if system:
    return system
  raise SetupError("Missing bundled or system SSH client")


def preflight(args):
  required = [args.client, args.server]
  required += [args.runtime / (name + SUFFIX) for name in (
    "frpc", "frps", "ssh-keygen", "sshd", "sshd-session", "sshd-auth", "sftp-server")]
  if os.name == "nt":
    required += [args.runtime / name for name in ("libcrypto.dll", "ssh-shellhost.exe")]
  missing = [str(path) for path in required if not path.is_file()]
  if missing:
    raise SetupError("Missing binaries/runtime files:\n  " + "\n  ".join(missing)
                     + "\nBuild both binaries and prepare state/runtime first.")
  options = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
  for name, command in (
      ("client", [args.client, "--help"]), ("server", [args.server, "--help"]),
      ("frpc", [args.runtime / ("frpc" + SUFFIX), "--version"]),
      ("frps", [args.runtime / ("frps" + SUFFIX), "--version"]),
      ("ssh", [ssh_executable(args.runtime), "-V"])):
    try:
      result = subprocess.run([str(value) for value in command], capture_output=True,
                              text=True, encoding="utf-8", errors="replace",
                              timeout=15, **options)
    except (OSError, subprocess.TimeoutExpired) as error:
      raise SetupError(f"Cannot run {name}: {error}") from None
    if result.returncode:
      raise SetupError(f"{name} startup check failed with code {result.returncode}")
    if name == "client" and "--headless" not in result.stdout:
      raise SetupError("Client lacks --headless; rebuild the current client")
  print("PASS: binaries and FRP/OpenSSH runtime are available", flush=True)


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--client", type=Path, default=REPO / "target" / "debug" / ("starbridge-client" + SUFFIX))
  parser.add_argument("--server", type=Path, default=REPO / "target" / "debug" / ("starbridge-server" + SUFFIX))
  parser.add_argument("--runtime", type=Path, default=REPO / "state" / "runtime")
  parser.add_argument("--policy", choices=("relay_only", "direct_preferred", "direct_only"),
                      default="relay_only", help="session path policy (default: relay_only)")
  parser.add_argument("--timeout", type=float, default=60, help="maximum seconds per readiness/SSH check")
  parser.add_argument("--settle-seconds", type=float, default=22,
                      help="watch for stale-session revival (default spans two 10s heartbeats)")
  parser.add_argument("--check-only", action="store_true", help="check dependencies without starting services")
  parser.add_argument("--keep-work-dir", action="store_true",
                      help="retain isolated logs and test credentials for diagnosis")
  args = parser.parse_args()
  if args.timeout <= 0 or args.settle_seconds < 0:
    parser.error("--timeout must be positive and --settle-seconds must be nonnegative")
  for name in ("client", "server", "runtime"):
    setattr(args, name, getattr(args, name).resolve())
  work = None
  regression = None
  exit_code = 0
  started = time.monotonic()
  try:
    preflight(args)
    if args.check_only:
      return 0
    work = Path(tempfile.mkdtemp(prefix="starbridge-peer-")).resolve()
    regression = PeerRegression(args, work)
    regression.run()
    print(f"PASS: all peer regressions completed in {time.monotonic() - started:.1f}s", flush=True)
  except SetupError as error:
    print(f"SETUP ERROR: {error}", file=sys.stderr)
    exit_code = 2
  except (RegressionError, OSError, ValueError, KeyError) as error:
    print(f"FAIL: {error}", file=sys.stderr)
    if not args.keep_work_dir:
      print("Use --keep-work-dir to retain isolated diagnostics.", file=sys.stderr)
    exit_code = 1
  except KeyboardInterrupt:
    print("Interrupted; stopping test processes.", file=sys.stderr)
    exit_code = 130
  finally:
    try:
      if regression:
        regression.close()
        leaks = regression.log_secret_leaks()
        if leaks:
          print("FAIL: test credentials appeared in logs: " + ", ".join(leaks), file=sys.stderr)
          exit_code = 1
        else:
          print("PASS: logs contain no test passwords or access/device/FRP/session tokens", flush=True)
    except (OSError, RegressionError) as error:
      print(f"FAIL: cleanup/log check: {error}", file=sys.stderr)
      exit_code = 1
    finally:
      if work:
        if args.keep_work_dir:
          print(f"Test directory (includes temporary test credentials): {work}", flush=True)
        else:
          shutil.rmtree(work)
  return exit_code


if __name__ == "__main__":
  sys.exit(main())
