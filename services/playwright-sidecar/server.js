// Headless Playwright driver for the web-browser-* tool family.
//
// Why this process exists: Thetis tools are wasm32-wasip2 components and cannot
// spawn processes, so none of them can drive Playwright directly. They *can*
// make outbound HTTP calls, so the kernel runs this sidecar on loopback and the
// tools speak JSON to it. One browser is shared by every conversation; each
// session id gets its own BrowserContext, which is Playwright's isolation unit
// (separate cookies, storage and cache).
//
// The wire is deliberately dumb: POST /op with {op, session, ...args} and get
// back {ok, ...} or {ok:false, error}. All formatting for the model is done
// here rather than in the tools, so the whole family stays consistent and a
// change to output shape does not mean recompiling twelve wasm components.

'use strict';

const http = require('http');
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

const PORT = Number(process.env.THETIS_PW_PORT || 39412);
const HOST = '127.0.0.1';
// A page that never settles must not wedge a tool call forever.
const DEFAULT_TIMEOUT = Number(process.env.THETIS_PW_TIMEOUT_MS || 15000);
// Contexts are cheap but not free; drop the ones nobody is using.
const IDLE_MS = Number(process.env.THETIS_PW_IDLE_MS || 900000);
// The kernel passes the token it generated; loopback plus a token means another
// local process cannot quietly drive the browser.
const TOKEN = process.env.THETIS_PW_TOKEN || '';

let browser = null;
/** session id -> { ctx, pages: Page[], active: number, consoles, requests, dialogs, lastUsed } */
const sessions = new Map();

async function getBrowser() {
  if (browser && browser.isConnected()) return browser;
  // headless is not configurable on purpose: there is no display on the host
  // this runs on, and a headed browser would hang waiting for one.
  browser = await chromium.launch({
    headless: true,
    args: ['--no-sandbox', '--disable-dev-shm-usage'],
  });
  return browser;
}

async function getSession(id) {
  const key = id || 'default';
  let s = sessions.get(key);
  if (s && s.ctx) {
    s.lastUsed = Date.now();
    return s;
  }
  const b = await getBrowser();
  const ctx = await b.newContext({
    viewport: { width: 1280, height: 800 },
    ignoreHTTPSErrors: true,
  });
  ctx.setDefaultTimeout(DEFAULT_TIMEOUT);
  ctx.setDefaultNavigationTimeout(DEFAULT_TIMEOUT);

  s = {
    ctx,
    pages: [],
    active: 0,
    consoles: [],
    requests: [],
    dialogs: [],
    lastUsed: Date.now(),
  };

  // Console and network history are captured as they happen: a model asking
  // "why did that fail" after the fact cannot rewind the page.
  //
  // This fires for pages the *site* opens (window.open, target=_blank) as well
  // as ones we create, so it is the only place that needs to attach. Calling
  // attachPage again for a page we opened ourselves would register every
  // listener twice and duplicate each console message.
  ctx.on('page', (page) => attachPage(s, page));
  sessions.set(key, s);
  await ctx.newPage();
  return s;
}

function attachPage(s, page) {
  // Idempotent: the 'page' event and an explicit call can both reach here for
  // the same page, and double listeners mean double history.
  if (s.pages.includes(page)) return;
  s.pages.push(page);

  page.on('console', (msg) => {
    s.consoles.push({
      type: msg.type(),
      text: msg.text(),
      ts: Date.now(),
    });
    if (s.consoles.length > 500) s.consoles.shift();
  });
  page.on('pageerror', (err) => {
    s.consoles.push({ type: 'pageerror', text: String(err), ts: Date.now() });
    if (s.consoles.length > 500) s.consoles.shift();
  });
  page.on('requestfinished', async (req) => {
    let status = null;
    try {
      const r = await req.response();
      status = r ? r.status() : null;
    } catch { /* response may be gone */ }
    s.requests.push({
      method: req.method(),
      url: req.url(),
      status,
      type: req.resourceType(),
      ts: Date.now(),
    });
    if (s.requests.length > 500) s.requests.shift();
  });
  page.on('requestfailed', (req) => {
    s.requests.push({
      method: req.method(),
      url: req.url(),
      status: 'failed',
      failure: req.failure() ? req.failure().errorText : '',
      type: req.resourceType(),
      ts: Date.now(),
    });
    if (s.requests.length > 500) s.requests.shift();
  });
  // An unhandled dialog blocks the page forever, so dismiss by default and
  // record it. browser_handle_dialog's behaviour is available via op=dialog.
  page.on('dialog', async (d) => {
    s.dialogs.push({ type: d.type(), message: d.message(), ts: Date.now() });
    if (!s.dialogHandler) {
      try { await d.dismiss(); } catch { /* already gone */ }
    }
  });
  page.on('close', () => {
    const i = s.pages.indexOf(page);
    if (i >= 0) s.pages.splice(i, 1);
    if (s.active >= s.pages.length) s.active = Math.max(0, s.pages.length - 1);
  });
}

async function activePage(s) {
  if (!s.pages.length) {
    // Every tab was closed; open a fresh one so the session stays usable.
    // The context's 'page' event does the attaching.
    const p = await s.ctx.newPage();
    s.active = 0;
    return p;
  }
  if (s.active >= s.pages.length) s.active = s.pages.length - 1;
  return s.pages[s.active];
}

// --- snapshot ---------------------------------------------------------------

// The accessibility snapshot with [ref=eN] handles. This is how a model is
// meant to address elements: refs come from here and go back as `target`.
async function snapshot(page) {
  try {
    return await page.ariaSnapshot({ mode: 'ai' });
  } catch (e) {
    // A navigation mid-snapshot is the common cause; one retry is enough.
    try {
      return await page.ariaSnapshot({ mode: 'ai' });
    } catch {
      return `<snapshot unavailable: ${e.message.split('\n')[0]}>`;
    }
  }
}

// A tool result is capped at 32 KB by the host, and a snapshot of a real
// application page can exceed that on its own. Trimming here rather than
// letting the host cut mid-character keeps the output valid and, more
// importantly, tells the caller what to do about it: filter, or page through.
const SNAPSHOT_BUDGET = Number(process.env.THETIS_PW_SNAPSHOT_CHARS || 12000);

function trimSnapshot(text, budget = SNAPSHOT_BUDGET) {
  if (!text || text.length <= budget) return { snapshot: text };
  const lines = text.split('\n');
  const kept = [];
  let used = 0;
  for (const line of lines) {
    if (used + line.length + 1 > budget) break;
    kept.push(line);
    used += line.length + 1;
  }
  return {
    snapshot: kept.join('\n'),
    snapshotTruncated: true,
    snapshotNote: `showing ${kept.length} of ${lines.length} nodes (${used} of ${text.length} chars). `
      + 'Narrow it with the `text` or `regex` argument to web-browser-snapshot rather than reading it all.',
  };
}

// A ref is only valid for the snapshot that produced it, and Playwright
// reassigns them when the DOM changes. Accepting a selector too means a caller
// who knows the page does not have to snapshot first.
function locate(page, target) {
  if (!target || typeof target !== 'string') {
    throw new Error('a `target` is required: a ref such as "e12" from a snapshot, or a selector');
  }
  const t = target.trim();
  if (/^e\d+$/.test(t)) return page.locator(`aria-ref=${t}`);
  if (t.startsWith('aria-ref=')) return page.locator(t);
  if (t.startsWith('text=') || t.startsWith('css=') || t.startsWith('xpath=')
      || t.startsWith('//') || t.startsWith('#') || t.startsWith('.')
      || /^[a-zA-Z][\w-]*(\[|\.|#|:|\s|$)/.test(t)) {
    return page.locator(t);
  }
  return page.locator(t);
}

async function pageState(page, s, extra = {}) {
  let title = '';
  try { title = await page.title(); } catch { /* navigating */ }
  return {
    ok: true,
    url: page.url(),
    title,
    tabs: s.pages.length,
    activeTab: s.active,
    ...extra,
  };
}

// --- operations -------------------------------------------------------------

const ops = {
  async status() {
    const b = browser && browser.isConnected() ? browser : null;
    return {
      ok: true,
      running: !!b,
      version: b ? b.version() : null,
      headless: true,
      playwright: require('playwright/package.json').version,
      sessions: [...sessions.keys()],
    };
  },

  async navigate(s, a) {
    const page = await activePage(s);
    if (a.action === 'back') await page.goBack({ waitUntil: a.waitUntil || 'load' });
    else if (a.action === 'forward') await page.goForward({ waitUntil: a.waitUntil || 'load' });
    else if (a.action === 'reload') await page.reload({ waitUntil: a.waitUntil || 'load' });
    else {
      if (!a.url) throw new Error('navigate needs a `url`');
      // Clear per-navigation history so console/network reflect this page.
      s.consoles = [];
      s.requests = [];
      await page.goto(a.url, { waitUntil: a.waitUntil || 'load' });
    }
    return pageState(page, s, trimSnapshot(await snapshot(page)));
  },

  async snapshot(s, a) {
    const page = await activePage(s);
    const full = await snapshot(page);
    if (a.text || a.regex) {
      const lines = full.split('\n');
      let re;
      if (a.regex) {
        const m = /^\/(.*)\/([gimsuy]*)$/.exec(a.regex);
        re = m ? new RegExp(m[1], m[2]) : new RegExp(a.regex);
      } else {
        re = new RegExp(a.text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'i');
      }
      const ctxLines = Number.isFinite(a.context) ? a.context : 2;
      const keep = new Set();
      lines.forEach((l, i) => {
        if (re.test(l)) {
          for (let j = Math.max(0, i - ctxLines); j <= Math.min(lines.length - 1, i + ctxLines); j++) keep.add(j);
        }
      });
      const picked = [...keep].sort((x, y) => x - y);
      const out = [];
      let prev = -1;
      for (const i of picked) {
        if (prev >= 0 && i > prev + 1) out.push('  ...');
        out.push(lines[i]);
        prev = i;
      }
      const hits = lines.filter((l) => re.test(l)).length;
      if (!out.length) {
        return pageState(page, s, {
          matches: 0,
          snapshot: '<no matching nodes>',
          hint: 'Nothing in the accessibility tree matched. The text may be an image, '
            + 'may not have loaded yet, or may differ in wording — call web-browser-snapshot '
            + 'with no filter to see what is actually there.',
        });
      }
      return pageState(page, s, {
        matches: hits,
        ...trimSnapshot(out.join('\n')),
      });
    }
    return pageState(page, s, trimSnapshot(full));
  },

  async click(s, a) {
    const page = await activePage(s);
    const opts = {
      button: a.button || 'left',
      clickCount: a.doubleClick ? 2 : 1,
      modifiers: a.modifiers || undefined,
    };
    if (a.x !== undefined && a.y !== undefined) {
      await page.mouse.click(a.x, a.y, { button: opts.button, clickCount: opts.clickCount });
    } else {
      const el = locate(page, a.target);
      if (a.doubleClick) await el.dblclick({ button: opts.button, modifiers: opts.modifiers });
      else await el.click(opts);
    }
    await settle(page);
    return pageState(page, s, trimSnapshot(await snapshot(page)));
  },

  async hover(s, a) {
    const page = await activePage(s);
    if (a.x !== undefined && a.y !== undefined) await page.mouse.move(a.x, a.y);
    else await locate(page, a.target).hover();
    await settle(page);
    return pageState(page, s, trimSnapshot(await snapshot(page)));
  },

  async type(s, a) {
    const page = await activePage(s);
    if (a.action === 'press_key') {
      if (!a.key) throw new Error('press_key needs a `key`');
      await page.keyboard.press(a.key);
    } else if (a.action === 'fill_form' && Array.isArray(a.fields)) {
      for (const f of a.fields) {
        const el = locate(page, f.target);
        if (f.type === 'checkbox' || f.type === 'radio') await el.setChecked(!!f.value);
        else if (f.type === 'select') await el.selectOption(String(f.value));
        else await el.fill(String(f.value ?? ''));
      }
    } else if (a.action === 'select_option') {
      const values = Array.isArray(a.values) ? a.values.map(String)
        : [String(a.value ?? a.text ?? '')];
      await locate(page, a.target).selectOption(values);
    } else {
      const el = locate(page, a.target);
      const text = String(a.text ?? '');
      if (a.slowly) await el.pressSequentially(text, { delay: 30 });
      else await el.fill(text);
      if (a.submit) await el.press('Enter');
    }
    await settle(page);
    return pageState(page, s, trimSnapshot(await snapshot(page)));
  },

  async evaluate(s, a) {
    const page = await activePage(s);
    if (!a.function) throw new Error('evaluate needs a `function`, e.g. "() => document.title"');
    const target = toFunction(a.function);
    let result;
    if (a.target) result = await locate(page, a.target).evaluate(target);
    else result = await page.evaluate(target);
    let rendered;
    try { rendered = JSON.stringify(result, null, 2); } catch { rendered = String(result); }
    if (rendered === undefined) rendered = 'undefined';
    return pageState(page, s, { result: rendered });
  },

  async screenshot(s, a) {
    const page = await activePage(s);

    // Binary never goes back inline. A tool result is a 32 KB string, and a
    // base64 screenshot of an ordinary page is several times that — so the
    // image is written where both the agent's file tools and the user's browser
    // can reach it, and only the path comes back.
    if (a.action === 'pdf') {
      const buf = await page.pdf({ format: a.format || 'Letter' });
      const saved = await saveArtifact(s, buf, a.filename, 'pdf');
      return pageState(page, s, { ...saved, mime: 'application/pdf' });
    }
    // An explicit `type` wins; otherwise a .png filename means the caller
    // wanted a PNG, and jpeg is the default because it is far smaller.
    const wantsPng = a.type === 'png'
      || (!a.type && /\.png$/i.test(String(a.filename || '')));
    const type = wantsPng ? 'png' : 'jpeg';
    const opts = { type, fullPage: !!a.fullPage };
    if (type === 'jpeg') opts.quality = Number.isFinite(a.quality) ? a.quality : 60;
    const buf = a.target
      ? await locate(page, a.target).screenshot(opts)
      : await page.screenshot(opts);
    const saved = await saveArtifact(s, buf, a.filename, type === 'png' ? 'png' : 'jpg');
    return pageState(page, s, {
      ...saved,
      mime: type === 'png' ? 'image/png' : 'image/jpeg',
    });
  },

  async wait(s, a) {
    const page = await activePage(s);
    const timeout = Number.isFinite(a.timeout) ? a.timeout : DEFAULT_TIMEOUT;
    if (a.text) {
      await page.getByText(a.text).first().waitFor({ state: 'visible', timeout });
    } else if (a.textGone) {
      await page.getByText(a.textGone).first().waitFor({ state: 'hidden', timeout });
    } else if (a.target) {
      await locate(page, a.target).waitFor({ state: a.state || 'visible', timeout });
    } else if (a.loadState) {
      await page.waitForLoadState(a.loadState, { timeout });
    } else if (Number.isFinite(a.time)) {
      await page.waitForTimeout(Math.min(a.time * 1000, timeout));
    } else {
      await page.waitForLoadState('load', { timeout });
    }
    return pageState(page, s, trimSnapshot(await snapshot(page)));
  },

  async console(s, a) {
    const page = await activePage(s);
    const order = { debug: 0, log: 1, info: 1, warning: 2, warn: 2, error: 3, pageerror: 3 };
    const min = order[a.level || 'info'] ?? 1;
    const items = s.consoles.filter((m) => (order[m.type] ?? 1) >= min);
    return pageState(page, s, {
      total: s.consoles.length,
      shown: items.length,
      messages: items.map((m) => `[${m.type}] ${m.text}`),
    });
  },

  async network(s, a) {
    const page = await activePage(s);
    if (Number.isFinite(a.index)) {
      const r = s.requests[a.index - 1];
      if (!r) throw new Error(`no request #${a.index}; there are ${s.requests.length}`);
      return pageState(page, s, { request: r });
    }
    let items = s.requests;
    if (a.filter) {
      const re = new RegExp(a.filter, 'i');
      items = items.filter((r) => re.test(r.url) || re.test(String(r.status)));
    }
    if (a.failedOnly) items = items.filter((r) => r.status === 'failed' || (typeof r.status === 'number' && r.status >= 400));
    return pageState(page, s, {
      total: s.requests.length,
      shown: items.length,
      requests: items.map((r, i) => `${i + 1}. ${r.method} ${r.status ?? '-'} ${r.url}`),
    });
  },

  async tabs(s, a) {
    if (a.action === 'new') {
      // The context's 'page' event attaches it; we only need its index.
      const p = await s.ctx.newPage();
      s.active = Math.max(0, s.pages.indexOf(p));
      if (a.url) await p.goto(a.url, { waitUntil: 'load' });
    } else if (a.action === 'select') {
      if (!Number.isFinite(a.index) || a.index < 0 || a.index >= s.pages.length) {
        throw new Error(`no tab #${a.index}; there are ${s.pages.length}`);
      }
      s.active = a.index;
      await s.pages[a.index].bringToFront();
    } else if (a.action === 'close') {
      const i = Number.isFinite(a.index) ? a.index : s.active;
      const p = s.pages[i];
      if (!p) throw new Error(`no tab #${i}`);
      await p.close();
    }
    const page = await activePage(s);
    const list = [];
    for (let i = 0; i < s.pages.length; i++) {
      let t = '';
      try { t = await s.pages[i].title(); } catch { /* navigating */ }
      list.push(`${i}${i === s.active ? ' *' : ''}: ${t} — ${s.pages[i].url()}`);
    }
    return pageState(page, s, { tabList: list });
  },

  async state(s, a) {
    const page = await activePage(s);
    const kind = a.kind || 'cookies';
    const action = a.action || 'list';

    if (kind === 'cookies') {
      if (action === 'list' || action === 'get') {
        const all = await s.ctx.cookies();
        const items = a.name ? all.filter((c) => c.name === a.name) : all;
        return pageState(page, s, { cookies: items });
      }
      if (action === 'set') {
        if (!a.name) throw new Error('setting a cookie needs a `name`');
        const u = new URL(page.url());
        await s.ctx.addCookies([{
          name: a.name,
          value: String(a.value ?? ''),
          domain: a.domain || u.hostname,
          path: a.path || '/',
        }]);
        return pageState(page, s, { set: a.name });
      }
      if (action === 'clear') {
        await s.ctx.clearCookies();
        return pageState(page, s, { cleared: 'cookies' });
      }
      if (action === 'delete') {
        if (!a.name) throw new Error('deleting a cookie needs a `name`');
        const keep = (await s.ctx.cookies()).filter((c) => c.name !== a.name);
        await s.ctx.clearCookies();
        if (keep.length) await s.ctx.addCookies(keep);
        return pageState(page, s, { deleted: a.name });
      }
    }

    if (kind === 'localStorage' || kind === 'sessionStorage') {
      const store = kind;
      const run = (fn, arg) => page.evaluate(fn, { store, arg });
      if (action === 'list') {
        const items = await run(({ store: st }) => {
          const s2 = window[st]; const o = {};
          for (let i = 0; i < s2.length; i++) { const k = s2.key(i); o[k] = s2.getItem(k); }
          return o;
        });
        return pageState(page, s, { [store]: items });
      }
      if (action === 'get') {
        const v = await run(({ store: st, arg }) => window[st].getItem(arg), a.name);
        return pageState(page, s, { key: a.name, value: v });
      }
      if (action === 'set') {
        await page.evaluate(({ store: st, k, v }) => window[st].setItem(k, v),
          { store, k: a.name, v: String(a.value ?? '') });
        return pageState(page, s, { set: a.name });
      }
      if (action === 'delete') {
        await page.evaluate(({ store: st, k }) => window[st].removeItem(k), { store, k: a.name });
        return pageState(page, s, { deleted: a.name });
      }
      if (action === 'clear') {
        await page.evaluate(({ store: st }) => window[st].clear(), { store });
        return pageState(page, s, { cleared: store });
      }
    }

    if (kind === 'storageState') {
      const st = await s.ctx.storageState();
      return pageState(page, s, { storageState: st });
    }

    if (kind === 'dialog') {
      if (action === 'accept' || action === 'dismiss') {
        // Arm a one-shot handler for the next dialog.
        s.dialogHandler = true;
        const page2 = await activePage(s);
        page2.once('dialog', async (d) => {
          try {
            if (action === 'accept') await d.accept(a.promptText || undefined);
            else await d.dismiss();
          } catch { /* gone */ }
          s.dialogHandler = false;
        });
        return pageState(page, s, { armed: action });
      }
      return pageState(page, s, { dialogs: s.dialogs });
    }

    if (kind === 'viewport') {
      if (!Number.isFinite(a.width) || !Number.isFinite(a.height)) {
        throw new Error('resizing needs `width` and `height`');
      }
      await page.setViewportSize({ width: a.width, height: a.height });
      return pageState(page, s, { viewport: { width: a.width, height: a.height } });
    }

    throw new Error(`unknown state kind '${kind}'`);
  },

  async close(s, a) {
    const key = a.session || 'default';
    if (a.all) {
      for (const [k, v] of sessions) {
        try { await v.ctx.close(); } catch { /* already gone */ }
        sessions.delete(k);
      }
      return { ok: true, closed: 'all sessions' };
    }
    const found = sessions.get(key);
    if (found) {
      try { await found.ctx.close(); } catch { /* already gone */ }
      sessions.delete(key);
    }
    return { ok: true, closed: key };
  },
};

// Where screenshots and PDFs land. `workspace/` is the directory the wasm
// guests get as a preopen, so a file written here is readable by the agent's
// own file tools and by every other tool — which is what makes "the screenshot
// is at <path>" a useful answer rather than a dead end.
const ARTIFACT_DIR = process.env.THETIS_PW_ARTIFACTS
  || path.join(process.cwd(), 'workspace', 'browser');

async function saveArtifact(s, buf, requested, ext) {
  await fs.promises.mkdir(ARTIFACT_DIR, { recursive: true });
  let name = (requested || '').trim();
  if (name) {
    // Never let a caller escape the artifact directory.
    name = path.basename(name);
  } else {
    const stamp = new Date().toISOString().replace(/[:.]/g, '-');
    name = `${stamp}.${ext}`;
  }
  if (!path.extname(name)) name += `.${ext}`;
  const full = path.join(ARTIFACT_DIR, name);
  await fs.promises.writeFile(full, buf);
  // Relative to the project root, because that is how the agent's file tools
  // address things.
  const rel = path.relative(process.cwd(), full);
  return { path: rel.startsWith('..') ? full : rel, bytes: buf.length };
}

// Playwright treats a *string* passed to evaluate() as an expression, so the
// source "() => document.title" evaluates to a function object and comes back
// as undefined rather than the title. Turning the source into a real function
// here is what makes the natural arrow-function form work — and it is the form
// every Playwright example, and therefore every model, reaches for first.
//
// Anything that does not look like a function is left as an expression, so
// "document.title" keeps working too.
function toFunction(src) {
  const looksLikeFn = /^\s*(async\s*)?(\(|function\b)/.test(src)
    || /^\s*(async\s*)?[A-Za-z_$][\w$]*\s*=>/.test(src);
  if (!looksLikeFn) return src;
  try {
    // eslint-disable-next-line no-eval
    const fn = eval(`(${src})`);
    return typeof fn === 'function' ? fn : src;
  } catch {
    // Let Playwright report the syntax error against the original source.
    return src;
  }
}

// After a click the page may navigate or re-render; a short settle makes the
// snapshot that follows describe the result rather than the page mid-flight.
async function settle(page) {
  try {
    await page.waitForLoadState('domcontentloaded', { timeout: 2000 });
  } catch { /* no navigation happened, which is fine */ }
}

// Ops that do not need a session context.
const SESSIONLESS = new Set(['status']);

async function dispatch(body) {
  const op = body.op;
  if (!op) throw new Error('missing `op`');
  const fn = ops[op];
  if (!fn) throw new Error(`unknown op '${op}'`);
  if (SESSIONLESS.has(op)) return fn(body);
  if (op === 'close') return fn(null, body);
  const s = await getSession(body.session);
  return fn(s, body);
}

const server = http.createServer((req, res) => {
  let raw = '';
  req.on('data', (c) => { raw += c; if (raw.length > 8 << 20) req.destroy(); });
  req.on('end', async () => {
    const send = (code, obj) => {
      const out = JSON.stringify(obj);
      res.writeHead(code, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(out) });
      res.end(out);
    };
    if (req.method === 'GET' && req.url === '/health') {
      return send(200, { ok: true, headless: true });
    }
    if (req.method !== 'POST') return send(405, { ok: false, error: 'POST /op' });
    if (TOKEN) {
      const got = req.headers['x-thetis-token'];
      if (got !== TOKEN) return send(403, { ok: false, error: 'bad or missing token' });
    }
    let body;
    try { body = raw ? JSON.parse(raw) : {}; }
    catch (e) { return send(400, { ok: false, error: `body was not JSON: ${e.message}` }); }
    try {
      const out = await dispatch(body);
      send(200, out);
    } catch (e) {
      // Playwright errors are multi-line with a long "call log"; the first
      // lines carry the cause and the rest floods a model's context.
      const msg = String(e && e.message ? e.message : e).split('\n').slice(0, 4).join('\n');
      send(200, { ok: false, error: msg });
    }
  });
});

setInterval(async () => {
  const now = Date.now();
  for (const [k, v] of sessions) {
    if (now - v.lastUsed > IDLE_MS) {
      try { await v.ctx.close(); } catch { /* already gone */ }
      sessions.delete(k);
    }
  }
  // With nothing open, drop the browser too and let it start again on demand.
  if (!sessions.size && browser && browser.isConnected()) {
    try { await browser.close(); } catch { /* already gone */ }
    browser = null;
  }
}, 60000).unref();

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, async () => {
    try { if (browser) await browser.close(); } catch { /* already gone */ }
    process.exit(0);
  });
}

server.listen(PORT, HOST, () => {
  console.log(`playwright sidecar listening on http://${HOST}:${PORT} (headless, playwright ${require('playwright/package.json').version})`);
});
