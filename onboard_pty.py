"""Exercise the actual interactive CLI in the QA user's session."""
import json
import os
from pathlib import Path
import re
import select
import subprocess
import sys
import time

root = Path(sys.argv[1]).resolve()
binary = root / ("rmx.exe" if os.name == "nt" else "rmx")
dashboard_port = int(sys.argv[2])
test_state = root / "state"
test_gui = root / "gui"
test_gui.mkdir(exist_ok=True)
(test_gui / "index.html").write_text("<html><body>Isolated Remodex sign-in QA</body></html>")
config = root / "setup.json"
config.write_text(json.dumps({"workspace": str(root), "connection": {
    "dashboard_bind": f"127.0.0.1:{dashboard_port}", "label": "Remodex sign-in QA"
}}))
os.environ["OPENCODEX_HOME"] = str(test_state)
os.environ["REMODEX_GUI_DIR"] = str(test_gui)
args = [str(binary), "onboard", "--no-open", "--config", str(config)]
prompt = "Start Remodex automatically whenever you sign in to this computer? [y/N]"
success = "Remodex will start automatically when you sign in."
terminal_output = root / "onboard.log"
escape_sequence = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")

if os.name == "nt":
    from winpty import Backend, PTY
    import queue
    import threading

    stream = queue.Queue()
    terminal = PTY(230, 40, backend=Backend.ConPTY)
    command_line = subprocess.list2cmdline(args)
    # Quoting is required for argv[0] as well as the config path. The binary
    # path passed to CreateProcess is provided separately by winpty.
    terminal.spawn(str(binary), cmdline=command_line[command_line.index(" onboard"):])

    def read_terminal():
        try:
            while True:
                output = terminal.read()
                if output:
                    stream.put(output)
                elif not terminal.isalive():
                    break
        except (EOFError, OSError):
            pass
        finally:
            stream.put(None)

    threading.Thread(target=read_terminal, daemon=True).start()

    def read_output():
        try:
            return stream.get(timeout=0.2)
        except queue.Empty:
            return ""

    def send_yes():
        terminal.write("y\r")
else:
    import errno
    import pty

    parent, terminal = pty.openpty()
    process = subprocess.Popen(args, stdin=terminal, stdout=terminal, stderr=terminal, start_new_session=True)
    os.close(terminal)

    def read_output():
        if not select.select([parent], [], [], 0.2)[0]:
            return ""
        try:
            chunk = os.read(parent, 65536)
            return chunk.decode("utf-8", errors="replace") if chunk else None
        except OSError as error:
            if error.errno == errno.EIO:
                return None
            raise

    def send_yes():
        os.write(parent, b"y\n")

answered_yes = False
transcript = ""
deadline = time.monotonic() + 115
with terminal_output.open("w", encoding="utf-8") as log:
    while time.monotonic() < deadline:
        chunk = read_output()
        if chunk is None:
            break
        if not chunk:
            continue
        log.write(chunk)
        log.flush()
        transcript += chunk
        plain_text = escape_sequence.sub("", transcript).replace("\r", "")
        if not answered_yes and prompt in plain_text:
            send_yes()
            answered_yes = True
        if answered_yes and success in plain_text:
            for line in plain_text.splitlines():
                if line.strip().startswith("{") and line.strip().endswith("}"):
                    try:
                        result = json.loads(line.strip())
                    except json.JSONDecodeError:
                        continue
                    if result.get("startAtLogin") and result.get("running"):
                        (root / "onboard-result.json").write_text(json.dumps({
                            "answeredYes": True,
                            "initialPid": result["pid"],
                            "environmentId": result["environmentId"],
                        }))
                        raise SystemExit(0)
raise SystemExit("The interactive Yes choice did not produce a verified registration")
