"""Exercise real Windows OpenSSH sessions and inspect the inherited console.

Build: cargo build --locked -p starbridge-client --no-default-features --bins
Run: python tools/test_windows_ssh_console.py
Only uses temporary keys, loopback SSH, and this run's owned process tree.
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
from pathlib import Path
import runpy
import socket
import subprocess
import sys
import tempfile
import time


REPO = Path(__file__).resolve().parents[1]
PROBE = r'''
import ctypes, json, sys
kernel = ctypes.WinDLL("kernel32")
kernel.GetConsoleWindow.restype = ctypes.c_void_p
user = ctypes.WinDLL("user32")
user.IsWindowVisible.argtypes = [ctypes.c_void_p]
window = kernel.GetConsoleWindow()
print(json.dumps({"console": bool(window), "visible": bool(window and user.IsWindowVisible(window)),
                  "arguments": sys.argv[1:], "stdin": sys.stdin.read()}), flush=True)
print("stderr-marker", file=sys.stderr, flush=True)
sys.exit(37)
'''


def run(argv, **options):
    return subprocess.run([str(value) for value in argv], capture_output=True,
                          timeout=20, creationflags=subprocess.CREATE_NO_WINDOW, **options)


def quoted(path):
    return '"' + str(path).replace("\\", "/") + '"'


def main():
    if os.name != "nt":
        raise SystemExit("Windows only")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--launcher", type=Path,
                        default=REPO / "target/debug/starbridge-ssh-session.exe")
    parser.add_argument("--runtime", type=Path, default=REPO / "state/runtime")
    parser.add_argument("--baseline", action="store_true",
                        help="inspect the original session behavior without asserting hidden windows")
    args = parser.parse_args()
    runtime = args.runtime.resolve()
    launcher = args.launcher.resolve()
    if not launcher.is_file():
        raise SystemExit("Build starbridge-ssh-session.exe first")
    job_class = runpy.run_path(str(REPO / "tools/test-peer-connectivity.py"))["WindowsJob"]
    with tempfile.TemporaryDirectory(prefix="starbridge ssh console ") as temporary:
        root = Path(temporary)
        for name in ("identity", "host"):
            result = run([runtime / "ssh-keygen.exe", "-q", "-t", "ed25519",
                          "-N", "", "-f", root / name])
            assert result.returncode == 0, "key generation failed"
        (root / "authorized_keys").write_bytes((root / "identity.pub").read_bytes())
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        config = (
            f"Port {port}\nListenAddress 127.0.0.1\nHostKey {quoted(root / 'host')}\n"
            f"AuthorizedKeysFile {quoted(root / 'authorized_keys')}\nStrictModes no\n"
            "PubkeyAuthentication yes\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n"
            f"Subsystem sftp {quoted(runtime / 'sftp-server.exe')}\n"
        )
        if not args.baseline:
            config += f"SshdSessionPath {quoted(launcher)}\n"
        (root / "sshd_config").write_text(config, encoding="utf-8")
        environment = os.environ.copy()
        environment["STARBRIDGE_SSHD_SESSION_PATH"] = str(runtime / "sshd-session.exe")
        environment.pop("SSH_TEST_ENVIRONMENT", None)
        ssh = [runtime / "ssh.exe", "-F", "none", "-p", str(port), "-i", root / "identity",
               "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=3",
               "-o", "StrictHostKeyChecking=accept-new", "-o", f"UserKnownHostsFile={root / 'known_hosts'}",
               getpass.getuser() + "@127.0.0.1"]
        job = None
        with (root / "sshd.log").open("wb") as log:
            process = subprocess.Popen([str(runtime / "sshd.exe"), "-D", "-e", "-f", str(root / "sshd_config")],
                                       env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                       creationflags=subprocess.CREATE_NO_WINDOW)
            try:
                job = job_class(process)
                deadline = time.monotonic() + 12
                while True:
                    assert process.poll() is None, "sshd exited before accepting connections"
                    result = run([*ssh[:-1], "-T", ssh[-1], "echo ready"])
                    if result.returncode == 0 and b"ready" in result.stdout:
                        break
                    if time.monotonic() > deadline:
                        raise AssertionError("SSH readiness failed: " + result.stderr.decode(errors="replace")[-500:])
                    time.sleep(0.2)
                probe = root / "console probe.py"
                probe.write_text(PROBE, encoding="utf-8")
                command = f'{quoted(sys.executable)} -X utf8 {quoted(probe)} "argument with spaces" "second-argument"'
                payload = "stdin 中文 marker\n"
                for attempt in range(6):
                    result = run([*ssh[:-1], "-T", ssh[-1], command], input=payload.encode())
                    assert result.returncode == 37, f"exit status lost: {result.returncode}"
                    assert b"stderr-marker" in result.stderr, "stderr lost"
                    report = json.loads(result.stdout)
                    assert report["arguments"] == ["argument with spaces", "second-argument"], report
                    assert report["stdin"] == payload, report
                    if not args.baseline:
                        assert report["console"] and not report["visible"], report
                print("BASELINE" if args.baseline else "PASS", "6 SSH executions:", json.dumps(report, ensure_ascii=True))
                # An explicit PTY must remain usable after the session launch changes.
                terminal = subprocess.Popen([str(value) for value in [*ssh[:-1], "-tt", ssh[-1],
                    "echo pty-marker & ping -n 2 127.0.0.1 >nul"]], stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=subprocess.CREATE_NO_WINDOW)
                try:
                    # PTY sessions treat immediate stdin EOF as terminal closure.
                    terminal.wait(timeout=15)
                    output, error = terminal.communicate()
                    assert terminal.returncode == 0 and b"pty-marker" in output, (
                        f"PTY execution failed ({terminal.returncode}): {output!r} {error!r}")
                finally:
                    if terminal.poll() is None:
                        terminal.kill()
                    terminal.communicate()
                # OpenSSH SCP uses the SFTP subsystem by default; exercise both directions.
                source = root / "upload.txt"
                source.write_bytes("file transfer 中文\n".encode())
                remote = root / "remote.txt"
                destination = root / "download.txt"
                scp_options = [runtime / "scp.exe", "-F", "none", "-P", str(port), "-i", root / "identity",
                               "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
                               "-o", f"UserKnownHostsFile={root / 'known_hosts'}"]
                target = ssh[-1] + ":" + str(remote).replace("\\", "/")
                assert run([*scp_options, source, target]).returncode == 0, "SFTP upload failed"
                assert run([*scp_options, target, destination]).returncode == 0, "SFTP download failed"
                assert destination.read_bytes() == source.read_bytes(), "SFTP content changed"
                print("PASS: PTY, SFTP upload/download, Unicode stdin, stdout/stderr, arguments and exit status")
            finally:
                if job:
                    job.close()
                else:
                    process.kill()
                process.wait(timeout=10)
        print("PASS: owned SSH process tree cleaned up")


if __name__ == "__main__":
    main()
