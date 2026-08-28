#!/usr/bin/env python3
"""End-to-end check of the terminal drawer against the live gateway.

Unlike the earlier probe this injects nothing: it opens the real conversation
whose worker holds a real shell, and reads what the host actually pushed. Scratch
file — delete before finishing.
"""
import json
import os
import socket
import struct
import sys
import time
import urllib.request

CDP = "http://127.0.0.1:9222"
LOGS = []


def new_target():
    req = urllib.request.Request(f"{CDP}/json/new?about:blank", method="PUT")
    return json.load(urllib.request.urlopen(req))


class WS:
    def __init__(self, url):
        _, rest = url.split("://", 1)
        hostport, path = rest.split("/", 1)
        host, port = hostport.split(":")
        self.sock = socket.create_connection((host, int(port)))
        self.sock.settimeout(60)
        self.sock.sendall(
            f"GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        self.buf = buf.split(b"\r\n\r\n", 1)[1]
        self.next_id = 0

    def send(self, method, params=None):
        self.next_id += 1
        payload = json.dumps({"id": self.next_id, "method": method,
                              "params": params or {}}).encode()
        header = bytearray([0x81])
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        mask = os.urandom(4)
        header += mask
        self.sock.sendall(bytes(header) + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))
        return self.next_id

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def frame(self):
        _, b1 = self._read(2)
        n = b1 & 0x7F
        if n == 126:
            n = struct.unpack(">H", self._read(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", self._read(8))[0]
        return json.loads(self._read(n))

    def call(self, method, params=None, timeout=60):
        want = self.send(method, params)
        end = time.time() + timeout
        while time.time() < end:
            msg = self.frame()
            if msg.get("id") == want:
                return msg
            note(msg)
        raise TimeoutError(method)


def note(msg):
    m = msg.get("method")
    if m == "Runtime.consoleAPICalled":
        p = msg["params"]
        text = " ".join(str(a.get("value", a.get("description", "?"))) for a in p["args"])
        LOGS.append(f"console.{p['type']}: {text}")
    elif m == "Log.entryAdded":
        e = msg["params"]["entry"]
        LOGS.append(f"log.{e['level']}: {e.get('text')} {e.get('url','')}")
    elif m == "Runtime.exceptionThrown":
        d = msg["params"]["exceptionDetails"]
        LOGS.append(f"exception: {d.get('text')} "
                    f"{d.get('exception',{}).get('description','')}")


def ev(ws, expr, timeout=90):
    r = ws.call("Runtime.evaluate",
                {"expression": expr, "returnByValue": True, "awaitPromise": True},
                timeout)
    res = r.get("result", {})
    if "exceptionDetails" in res:
        return {"__error": str(res["exceptionDetails"].get("exception", {})
                              .get("description"))[:400]}
    return res.get("result", {}).get("value")


def drain(ws, seconds):
    end = time.time() + seconds
    ws.sock.settimeout(0.4)
    try:
        while time.time() < end:
            try:
                note(ws.frame())
            except socket.timeout:
                pass
    finally:
        ws.sock.settimeout(60)


def main():
    base, session = sys.argv[1], sys.argv[2]
    target = new_target()
    ws = WS(target["webSocketDebuggerUrl"])
    ws.call("Runtime.enable")
    ws.call("Log.enable")
    ws.call("Page.enable")
    ws.call("Emulation.setDeviceMetricsOverride",
            {"width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False})

    # Record what the host sends, without altering it. This is observation only:
    # every frame still reaches the app's own handlers untouched.
    ws.call("Page.addScriptToEvaluateOnNewDocument", {"source": """
      (() => {
        const Real = window.WebSocket;
        window.__seen = [];
        window.WebSocket = function (...a) {
          const s = new Real(...a);
          window.__ws = s;
          s.addEventListener('message', (e) => {
            try {
              const f = JSON.parse(e.data);
              if (f.type === 'terminal' || f.type === 'terminals') {
                window.__seen.push({type: f.type, id: f.id, kind: f.kind,
                                    n: (f.terminals||[]).length,
                                    text: (f.text||'').slice(0,60)});
              }
            } catch {}
          });
          return s;
        };
        window.WebSocket.prototype = Real.prototype;
        Object.assign(window.WebSocket, Real);
      })();
    """})

    print("== load the live page ==")
    ws.call("Page.navigate", {"url": base})
    drain(ws, 7)
    print(json.dumps(ev(ws, """
      (() => ({
        dock: !!document.getElementById('terminal-dock'),
        hidden: document.getElementById('terminal-dock')?.hidden,
        rows: document.querySelectorAll('.session-row,[data-session]').length,
      }))()
    """), indent=1))

    print("\n== open the conversation whose worker holds a real shell ==")
    print(json.dumps(ev(ws, f"""
      (async () => {{
        const want = {json.dumps(session)};
        const rows = [...document.querySelectorAll('[data-session]')];
        const row = rows.find(r => r.dataset.session === want);
        if (!row) return {{found: false, ids: rows.map(r => r.dataset.session).slice(0,8)}};
        row.click();
        // The history round trip is a worker call; give it room.
        for (let i = 0; i < 60; i++) {{
          await new Promise(r => setTimeout(r, 500));
          if (document.querySelectorAll('.term-tab').length) break;
        }}
        const dock = document.getElementById('terminal-dock');
        const sb = document.getElementById('statusbar').getBoundingClientRect();
        const d = dock.getBoundingClientRect();
        return {{
          found: true,
          frames: window.__seen,
          dock_hidden: dock.hidden,
          dock_height: d.height,
          above_statusbar: d.bottom <= sb.top + 1.5,
          tabs: [...document.querySelectorAll('.term-tab')].map(t => t.textContent.trim()),
          foot: document.querySelector('.term-foot')?.textContent.trim(),
          xterm_mounted: document.querySelectorAll('.xterm-rows').length,
          transcript_tail: document.querySelector('.xterm-rows')?.textContent.slice(-260),
        }};
      }})()
    """, 120), indent=1))
    drain(ws, 2)

    # Hand back control so the caller can run a command in the real shell.
    print("\nREADY_FOR_COMMAND", flush=True)
    with open("/tmp/probe_go", "w") as f:
        f.write("waiting")
    for _ in range(240):
        if not os.path.exists("/tmp/probe_go"):
            break
        drain(ws, 0.5)

    print("\n== after a real command ran in the real shell ==")
    print(json.dumps(ev(ws, """
      (() => {
        const rows = document.querySelector('.xterm-rows');
        return {
          frames: window.__seen.slice(-14),
          tabs: [...document.querySelectorAll('.term-tab')].map(t => t.textContent.trim()),
          busy_dots: document.querySelectorAll('.term-dot.is-busy').length,
          foot: document.querySelector('.term-foot')?.textContent.trim(),
          transcript_tail: rows?.textContent.slice(-400),
        };
      })()
    """, 60), indent=1))
    drain(ws, 2)

    print("\n== console ==")
    print("\n".join(f"  {l}" for l in LOGS) if LOGS else "  (clean)")


main()
