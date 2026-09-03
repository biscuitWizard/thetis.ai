---
name = "Verifying a UI change on a branch"
brief = "Prove a UI change works before merging, by running your branch's own gateway on a spare port and driving headless Chrome over CDP."
when_to_use = "Use when a UI edit under gateways/gateway-web/src/ui builds green but the running page does not show it, when curl on the live port 404s a file you just added to assets.rs, or when a change needs real browser evidence — layout geometry, console errors, responsive behaviour — before it is merged to trunk. Also use when playwright MCP tools are unavailable and there is no node on the box. Not for reasoning about CSS or picking tokens; that is the parent skill."
tags = ["ui", "verify", "browser", "headless", "chrome", "cdp", "gateway", "branch", "404", "stale", "tool-group:shell", "tool-group:selfmod"]
version = 2
---

# Verifying a UI change on a branch

The gateway serves the UI **built from committed trunk**. A conversation works
on its own branch, so a new `ui/` file 404s on the live port no matter how many
times the guest builds green — and that is correct behaviour, not a bug to
chase. `curl -s http://127.0.0.1:7777/views/new.js` returning 404 while
`branch_status` says "15 ahead of trunk" is exactly this.

## Try `/preview/` first

**`/preview/<your session id>/` serves your branch's build against the real
running system.** Rebuild `gateway:web`, then open it. That is the sanctioned
route, it needs no second instance, and it costs seconds rather than the ~5
minutes a cold bootstrap takes.

Only fall through to a second gateway when `/preview/` genuinely cannot answer
the question, and say why. It starts another Thetis, which
`thetis-internals/working-alongside-others` tells you not to do casually: the
processes are detached, so nothing but you will clean them up, and forgetting
leaves a stray instance holding a port. If you do start one, kill it by port in
the same session — see Clean up below.

## 1. Clone the branch and start a second gateway

The branch's commits are what matter — an uncommitted edit is invisible to the
bootstrap build, same as on trunk. Every guest build already commits, so the
branch is usually ready.

```
git clone -q --no-hardlinks <worktree-path> /tmp/uitest
```

Then launch, **overriding every shared path**. This is the step that goes wrong:
`THETIS_TARGET_DIR`, `THETIS_ARTIFACTS_DIR` and `THETIS_DATA_DIR` are
inherited from your own environment, and if you leave them the test gateway
loads the *real* instance's cached artifact and shows you trunk's UI while
claiming success.

```
cd /tmp/uitest && env -i \
  PATH=$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin HOME=$HOME \
  THETIS_ROOT=/tmp/uitest THETIS_BIND=127.0.0.1:7788 \
  THETIS_DATA_DIR=/tmp/uitest/data \
  THETIS_ARTIFACTS_DIR=/tmp/uitest/artifacts \
  THETIS_TARGET_DIR=/tmp/uitest/target-wasm \
  nohup <worktree>/target/release/thetis > /tmp/uitest/gw.log 2>&1 &
```

`env -i` needs `PATH` to include cargo, or the bootstrap build dies with
`bootstrap build did not run: running cargo for gateway/web`. Wait for
`UI bootstrapped and serving` in the log — a cold wasm build is 2–4 minutes.

Confirm the guest is genuinely yours before drawing conclusions:

```
curl -s http://127.0.0.1:7788/app.css | grep -c <a-new-class>
```

### Survive your own restarts

A shell session dies when Thetis restarts, and **it takes its children with it**
— gateway and Chrome both. A foreground `terminal_run` that blocks for minutes
is also cut off mid-flight. A cold bootstrap plus a full tool build is ~5
minutes, so a restart landing in the middle is likely, not unlucky.

So: launch every long-lived process with `setsid ... > log 2>&1 < /dev/null &`,
run the probe itself detached to a file, and poll with short commands. `setsid`
is the reason these survive — and equally the reason they are yours to kill, so
do not end the session without the cleanup step.

```
setsid python3 cdp_probe.py http://127.0.0.1:7788 1440 > /tmp/p.txt 2>&1 </dev/null &
# then, in a separate short call:
cat /tmp/p.txt
```

After a restart, do not rebuild from scratch — check what is still up:

```
ss -ltn | grep -E '7788|9222'
curl -s http://127.0.0.1:7788/app.css | grep -c <a-new-class>
```

Note also that `terminal_run` refuses a `cd` outside your own worktree, so drive
the clone with `git -C /tmp/uitest ...` rather than changing directory into it.
To move the test gateway onto a new commit, keep the warm data dir and
`git -C /tmp/uitest fetch && reset --hard origin/HEAD`, then restart it — that
is seconds, against minutes for a fresh clone.

## 2. Drive headless Chrome over CDP

Playwright's chromium is on the box even when the MCP tools are not:

```
$HOME/.cache/ms-playwright/chromium-*/chrome-linux64/chrome \
  --headless=new --remote-debugging-port=9222 --no-sandbox --disable-gpu \
  --user-data-dir=/tmp/chromeprofile about:blank &
```

With no node and no python websocket library, a ~40-line hand-rolled client is
the shortest path: `PUT /json/new?about:blank` for a target, then a raw
websocket to `webSocketDebuggerUrl`, then `Runtime.enable`, `Log.enable`,
`Emulation.setDeviceMetricsOverride`, `Page.navigate`, and
`Runtime.evaluate` with `returnByValue`. Client frames must be masked; server
frames are not.

Assert on **computed geometry**, not on screenshots you cannot see:

```js
const r = bar.getBoundingClientRect();
return {
  bar_at_foot: Math.abs(r.bottom - innerHeight) < 1.5,
  app_stops_at_bar: Math.abs(app.getBoundingClientRect().bottom - r.top) < 1.5,
  overflowing: bar.scrollWidth > bar.clientWidth + 1,
  doc_scroll: document.documentElement.scrollWidth,   // catches stray overflow
};
```

Collect `Runtime.consoleAPICalled`, `Log.entryAdded` and
`Runtime.exceptionThrown` throughout. **Zero entries is the bar.**

Two assertions that look right and are not:

- **A closed `<details>` still reports a bounding box.** Its children have real
  `getBoundingClientRect()` widths and heights, so "height === 0" fails on
  content that is genuinely hidden. Ask `el.checkVisibility()` instead, and
  check the group's own height equals its `summary`'s.
- **Exact-equality width checks fail on subpixel layout.** A paragraph filling
  its parent measures 551 against an inner box of 553. Allow ~3px.

Give a panel fed by a live worker a long poll, not a fixed sleep: a fresh
instance builds every tool component before the Tools frame answers, which is
minutes. Poll for the selector you need, up to a few hundred iterations.

Re-run at 1440 / 1200 / 1000 / 860 px. A gap of exactly ~10px between a bar's
bottom and the viewport is a horizontal scrollbar from something overflowing —
find it with `[...document.querySelectorAll('body *')].filter(n => n.getBoundingClientRect().right > innerWidth + 1)`.

## 3. Exercise the real protocol

A panel showing live numbers is only proven by numbers that move. Speak the
Thetis wire protocol on the same socket — plain JSON frames, no ids: `hello`
to get a session list, `open`, `send`, then poll your frame and print each
distinct tuple. A turn that fails on a missing API key still proves the
plumbing: the state went `working / turning=1` and back to `running`.

For a host frame alone, an `#[ignore]`d integration test under
`crates/thetis/tests/` is cheaper and stays in the repo as documentation —
see `ws_system.rs`. Run it with `THETIS_WS_URL=ws://127.0.0.1:7788/ws
cargo test -p thetis --test <name> -- --ignored --nocapture`.

## 4. Clean up

Kill by port, never by path: `pkill -f /tmp/uitest` also kills the Chrome whose
`--user-data-dir` lives there.

```
for p in $(ss -ltnp | grep 7788 | grep -oP 'pid=\K[0-9]+'); do kill -9 $p; done
pkill -f "remote-debugging-port=9222"
rm -rf /tmp/uitest /tmp/chromeprofile
```

Delete scratch scripts from the worktree — they would otherwise ride along into
the merge. Note that file tools refuse paths outside the project root, so a
helper script has to live in the worktree and be cleaned up after.

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| New `ui/` file 404s on the live port | Gateway serves committed trunk | Expected. Use a second gateway |
| Test gateway shows trunk's UI, not yours | Shared `THETIS_TARGET_DIR` / `THETIS_ARTIFACTS_DIR` | Override both; delete them and restart |
| `bootstrap build did not run` | `env -i` dropped cargo from `PATH` | Add `$HOME/.cargo/bin` |
| `Address already in use` after a restart | Previous instance still holds the port | Kill by pid from `ss -ltnp` |
| Bar reports "UI stale" about itself | Served artifact predates the branch head | It is right — restart the test gateway |
| Chrome dies when you kill the gateway | `pkill -f` matched its `--user-data-dir` | Keep the profile outside the test root |
| `HTTP 405` from `/json/new` | CDP wants PUT | `urllib.request.Request(..., method="PUT")` |
| Gateway and Chrome vanish together | A Thetis restart killed the shell that owned them | Launch both with `setsid`, detached |
| Probe never returns any output | Blocked in the foreground and cut off by a restart | Redirect to a file, detached; poll with short calls |
| Everything 404s and paths look wrong | The project root was renamed under you | Re-read `env | grep -i thetis` and relaunch |
