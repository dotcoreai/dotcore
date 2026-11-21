#!/usr/bin/env python3
"""Drives install.sh against fake tools in a pseudo-terminal, in a throwaway HOME.

Each case builds a clean environment and runs the real installer script, so what it
asserts is whether the script reaches its boundaries: the Docker preflight decision,
the Nix/profile install and the fresh join handoff. Nothing here changes the host.

A fake Unix socket, the real curl, an entirely clean PATH (no real docker, no real nix,
no development environment) and a separate bundle directory stand in for the host's tools.
"""

import os
import pty
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
INSTALLER = os.path.join(REPO_ROOT, "installer", "install.sh")

# Bounded waits, never fixed sleeps: a case fails when its boundary does not appear.
READ_DELAY = 0.1

# Standard utilities the installer calls; the cases link them from the host into their PATH.
HOST_UTILITIES = ["basename", "cat", "cut", "dd", "dirname", "env", "head", "id", "mkdir", "od", "rm", "sed", "stty", "tr", "wc"]
PROMPT_TIMEOUT = 15
EXIT_TIMEOUT = 15
MARKER_TIMEOUT = 15

FAKE_NIX = """#!/bin/sh
case "$1" in
  profile)
    case "$2" in
      list)
        printf '{"elements":{}}\\n'
        ;;
      add)
        if [ "$3" = "--help" ]; then
          exit 0
        fi
        mkdir -p "$DOTCORE_TEST_MARKERS"
        : > "$DOTCORE_TEST_MARKERS/profile-add"
        ;;
    esac
    exit 0
    ;;
  flake)
    exit 0
    ;;
  eval)
    if [ "$2" = "--raw" ] && [ -f "$DOTCORE_TEST_MARKERS/profile-add" ]; then
      printf 'dotcore\\tfake\\t%s\\n' "$DOTCORE_TEST_BUNDLE"
    fi
    exit 0
    ;;
  --version)
    echo "1.0.0"
    exit 0
    ;;
esac
exit 0
"""

def fake_uname(system, arch):
    return f"""#!/bin/sh
if [ "$1" = "-m" ]; then
  echo {arch}
else
  echo {system}
fi
"""

FAKE_BUNDLE_DOT = """#!/bin/sh
case "$1" in
  --help)
    exit 0
    ;;
  node)
    if [ "$2" = "join" ]; then
      mkdir -p "$DOTCORE_TEST_MARKERS"
      : > "$DOTCORE_TEST_MARKERS/join-called"
      exit 0
    fi
    ;;
esac
exit 0
"""


class TestFailure(Exception):
    pass


class Case:
    """A throwaway HOME with a curated PATH and the fake tools the installer calls."""

    def __init__(self, kind):
        self.kind = kind
        self.tmp = tempfile.mkdtemp(prefix="installer-test-", dir="/tmp")
        self.bin = os.path.join(self.tmp, "bin")
        self.markers = os.path.join(self.tmp, "markers")
        self.bundle = os.path.join(self.tmp, "bundle")
        self.socket_path = os.path.join(self.tmp, "docker.sock")
        self.server = None

        os.mkdir(self.bin)
        os.mkdir(self.markers)
        os.makedirs(os.path.join(self.bundle, "bin"))

        # The installer probes the socket with the same curl a real host would use.
        real_curl = shutil.which("curl")
        if not real_curl:
            raise TestFailure("no curl on PATH; the socket probe cannot be tested")
        os.symlink(real_curl, os.path.join(self.bin, "curl"))

        # The PATH holds only these links and fakes, so a Docker CLI on the host never leaks in.
        for tool in HOST_UTILITIES:
            real_tool = shutil.which(tool)
            if real_tool:
                os.symlink(real_tool, os.path.join(self.bin, tool))

        self._write_fake("nix", FAKE_NIX)
        # No dotcore is running here, by definition of the throwaway environment.
        self._write_fake("pgrep", "#!/bin/sh\nexit 1\n")
        self._write_fake(os.path.join(self.bundle, "bin", "dotc"), FAKE_BUNDLE_DOT)

        # The host may differ; Mac and Linux cases fake `uname` so both platforms run everywhere.
        system, arch = ("Linux", "x86_64") if kind == "linux" else ("Darwin", "arm64")
        self._write_fake("uname", fake_uname(system, arch))

        self.env = {
            "HOME": self.tmp,
            "PATH": self.bin,
            "NO_COLOR": "1",
            "TERM": "dumb",
            "DOCKER_HOST": f"unix://{self.socket_path}",
            "DOTCORE_TEST_MARKERS": self.markers,
            "DOTCORE_TEST_BUNDLE": self.bundle,
        }

    def _write_fake(self, name, content):
        path = name if name.startswith("/") else os.path.join(self.bin, name)
        with open(path, "w") as handle:
            handle.write(content)
        os.chmod(path, 0o755)

    def prepare(self, socket_state):
        """Prepare the docker socket state for this case."""
        if socket_state == "reachable":
            self.server = ReachableSocketServer(self.socket_path)
            self.server.daemon = True
            self.server.start()
        elif socket_state == "stopped":
            # The file exists but nobody listens: the daemon is stopped.
            fd = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            fd.bind(self.socket_path)
            fd.close()
        # absent socket: nothing to prepare

    def cleanup(self):
        if self.server is not None:
            self.server.stop()
        shutil.rmtree(self.tmp, ignore_errors=True)

    def marker(self, name):
        return os.path.exists(os.path.join(self.markers, name))


class ReachableSocketServer(threading.Thread):
    """Answers the Docker ping API on a fake Unix socket, as a reachable daemon would."""

    def __init__(self, path):
        super().__init__()
        self.path = path
        self._stop = threading.Event()
        self._fd = None

    def run(self):
        if os.path.exists(self.path):
            os.unlink(self.path)
        self._fd = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._fd.setblocking(False)
        self._fd.bind(self.path)
        self._fd.listen(2)
        while not self._stop.is_set():
            try:
                conn, _ = self._fd.accept()
            except OSError:
                continue
            try:
                conn.setblocking(False)
                conn.recv(4096)
                conn.sendall(
                    b"HTTP/1.1 200 OK\r\n"
                    b"Content-Type: text/plain\r\n"
                    b"Content-Length: 2\r\n\r\nOK"
                )
            except OSError:
                pass
            finally:
                conn.close()

    def stop(self):
        self._stop.set()
        try:
            self._fd.close()
        except OSError:
            pass
        try:
            os.unlink(self.path)
        except OSError:
            pass


# Makes the pseudo-terminal on stdin the controlling terminal of the new session, then runs the
# installer. The installer reads its prompts from /dev/tty; Linux attaches a controlling terminal
# to a new session only on request, macOS does it implicitly.
_LAUNCHER = """
import fcntl, os, sys, termios
fcntl.ioctl(0, termios.TIOCSCTTY, 0)
os.execv(sys.argv[1], sys.argv[1:])
"""


class Runner:
    """Runs install.sh in a pseudo-terminal and answers the prompts the cases expect."""

    def __init__(self, case, args):
        self.case = case
        self.output = bytearray()
        self._master, slave = pty.openpty()
        self._tty_open = True
        self._proc = subprocess.Popen(
            [sys.executable, "-c", _LAUNCHER, "/bin/sh", INSTALLER] + args,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=case.env,
            start_new_session=True,
        )
        os.close(slave)

    @property
    def text(self):
        return self.output.decode("utf-8", "replace")

    def expect(self, pattern, description, timeout=PROMPT_TIMEOUT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self._tty_open:
                readable, _, _ = select.select([self._master], [], [], READ_DELAY)
                if readable:
                    try:
                        chunk = os.read(self._master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    self.output.extend(chunk)
            if re.search(pattern, self.text):
                return
            if self._proc.poll() is not None:
                raise TestFailure(
                    f"the installer stopped before reaching: {description}\n--- output ---\n{self.text}"
                )
        raise TestFailure(
            f"the installer never reached: {description}\n--- output ---\n{self.text}"
        )

    def write(self, data):
        os.write(self._master, data)

    def close_tty(self):
        self._tty_open = False
        os.close(self._master)

    def wait(self):
        """Wait for the installer to exit, draining its pseudo-terminal as it goes.

        The child blocks writing to the terminal while the harness does not read it, so
        this drains continuously until the child exits or a bounded deadline passes.
        """
        deadline = time.monotonic() + EXIT_TIMEOUT
        while time.monotonic() < deadline:
            if self._tty_open:
                readable, _, _ = select.select([self._master], [], [], READ_DELAY)
                if readable:
                    try:
                        chunk = os.read(self._master, 65536)
                    except OSError:
                        self._tty_open = False
                        continue
                    if not chunk:
                        self._tty_open = False
                        continue
                    self.output.extend(chunk)
            if self._proc.poll() is not None:
                return self._proc.poll()
        try:
            return self._proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait()
            raise TestFailure(f"the installer did not exit\n--- output ---\n{self.text}")

    def wait_for_marker(self, name, timeout=MARKER_TIMEOUT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.case.marker(name):
                return
            time.sleep(0.05)
            self._read_more()
        raise TestFailure(f"marker {name!r} never appeared\n--- output ---\n{self.text}")

    def _read_more(self):
        readable, _, _ = select.select([self._master], [], [], 0)
        if readable:
            try:
                chunk = os.read(self._master, 65536)
            except OSError:
                return
            if chunk:
                self.output.extend(chunk)

def check_output(runner, needles):
    for needle in needles:
        if needle not in runner.text:
            raise TestFailure(f"expected {needle!r} in output:\n--- output ---\n{runner.text}")


def case_mac_decline():
    case = Case("absent")
    case.prepare("absent")
    try:
        r = Runner(case, [])
        r.expect(r"Continue without Docker\?", "the macOS Docker warning prompt")
        r.write(b"n\n")
        code = r.wait()
        if code != 0:
            raise TestFailure(f"expected exit 0 after declining, got {code}\n{r.text}")
        check_output(r, ["Stopped"])
        if case.marker("profile-add"):
            raise TestFailure("the profile install ran after declining Docker")
    finally:
        case.cleanup()


def case_mac_tty_closed():
    case = Case("absent")
    case.prepare("absent")
    try:
        r = Runner(case, [])
        r.expect(r"Continue without Docker\?", "the macOS Docker warning prompt")
        r.close_tty()
        code = r.wait()
        if code == 0:
            raise TestFailure("closing the terminal must stop the installer non-successfully")
        if case.marker("profile-add"):
            raise TestFailure("the profile install ran after the terminal closed")
    finally:
        case.cleanup()


def case_mac_accept_stopped():
    case = Case("stopped")
    case.prepare("stopped")
    try:
        r = Runner(case, [])
        r.expect(r"Continue without Docker\?", "the macOS Docker warning prompt")
        r.write(b"y\n")
        r.expect(r"Starting Dotcore", "the fresh-install countdown")
        r.write(b"\x1b")
        code = r.wait()
        if code != 0:
            raise TestFailure(f"expected exit 0 on accept, got {code}\n{r.text}")
        check_output(r, ["Continuing without Docker"])
        if not case.marker("profile-add"):
            raise TestFailure("the installer did not reach the nix profile install")
    finally:
        case.cleanup()


def case_mac_accept_absent():
    case = Case("absent")
    case.prepare("absent")
    try:
        r = Runner(case, [])
        r.expect(r"Continue without Docker\?", "the macOS Docker warning prompt")
        r.write(b"y\n")
        r.expect(r"Starting Dotcore", "the fresh-install countdown")
        r.write(b"\x1b")
        code = r.wait()
        if code != 0:
            raise TestFailure(f"expected exit 0 on accept, got {code}\n{r.text}")
        check_output(r, ["Continuing without Docker"])
        if not case.marker("profile-add"):
            raise TestFailure("the installer did not reach the nix profile install")
    finally:
        case.cleanup()


def case_mac_reachable_no_cli():
    case = Case("reachable")
    case.prepare("reachable")
    try:
        r = Runner(case, [])
        r.expect(r"Starting Dotcore", "the fresh-install countdown")
        r.write(b"\x1b")
        code = r.wait()
        if code != 0:
            raise TestFailure(f"expected exit 0 with reachable Docker, got {code}\n{r.text}")
        check_output(r, ["reachable"])
        if re.search(r"isn't reachable", r.text):
            raise TestFailure(f"the installer warned about Docker despite a reachable daemon\n{r.text}")
        if not case.marker("profile-add"):
            raise TestFailure("the installer did not reach the nix profile install")
    finally:
        case.cleanup()


def case_mac_join_handoff():
    case = Case("absent")
    case.prepare("absent")
    try:
        r = Runner(case, ["--join", "dcjoin-test-token"])
        r.expect(r"Continue without Docker\?", "the macOS Docker warning prompt")
        r.write(b"y\n")
        r.wait_for_marker("join-called")
        code = r.wait()
        if code != 0:
            raise TestFailure(f"expected exit 0 on the join handoff, got {code}\n{r.text}")
        if not case.marker("profile-add"):
            raise TestFailure("the installer did not reach the nix profile install")
    finally:
        case.cleanup()


def case_linux_unchanged():
    case = Case("linux")
    case.prepare("absent")
    try:
        r = Runner(case, [])
        r.expect(r"Docker isn't installed", "the Linux Docker prerequisite failure")
        code = r.wait()
        if code != 1:
            raise TestFailure(f"expected exit 1 on the Linux failure, got {code}\n{r.text}")
        if case.marker("profile-add"):
            raise TestFailure("the Linux failure must stop before the nix profile install")
    finally:
        case.cleanup()


def main():
    cases = [
        ("macOS: decline without Docker stops before the profile install", case_mac_decline),
        ("macOS: closing the terminal stops before the profile install", case_mac_tty_closed),
        ("macOS: a stopped socket accepts with no Docker CLI and installs", case_mac_accept_stopped),
        ("macOS: an absent socket accepts and installs with no Docker CLI", case_mac_accept_absent),
        ("macOS: a reachable socket installs without a warning", case_mac_reachable_no_cli),
        ("macOS: a fresh join handoff runs without Docker", case_mac_join_handoff),
        ("Linux: the Docker prerequisite fails unchanged with no CLI", case_linux_unchanged),
    ]

    failed = 0
    for name, fn in cases:
        print(f"- {name}", flush=True)
        try:
            fn()
            print("  ok", flush=True)
        except TestFailure as error:
            failed += 1
            print(f"  FAILED: {error}", flush=True)
        except Exception as error:  # a harness error is still a failure
            failed += 1
            print(f"  HARNESS ERROR: {error}", flush=True)

    if failed:
        print(f"\n{failed} of {len(cases)} installer cases failed")
        sys.exit(1)
    print(f"\nAll {len(cases)} installer cases passed")


if __name__ == "__main__":
    main()
