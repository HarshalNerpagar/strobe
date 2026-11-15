// The page's fetch shim for Remotion's OffthreadVideo frame URLs (remotion.js), run in Node with a stub page.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const load = ({ request, config }) => {
  const fetched = [];
  const fellBack = [];
  const sandbox = {
    window: null,
    location: { origin: 'vrf://project' },
    MutationObserver: class { observe() {} },
    HTMLCanvasElement: class { getContext() {} },
    // A Node MessagePort with a listener keeps the process alive; this one runs the turn on setImmediate.
    MessageChannel: class {
      constructor() {
        this.port1 = {};
        this.port2 = { postMessage: () => setImmediate(() => this.port1.onmessage?.()) };
      }
    },
    fetch: async (input, init) => {
      fetched.push({ input: String(input), signal: init?.signal });
      return { status: 200, via: 'server' };
    },
    __vrf: { video: { request, fallback: () => null, refusal: () => 'refused', fellBack: (src, reason) => fellBack.push([src, reason]) } },
    __vrf_rm_config: config,
  };
  sandbox.window = sandbox;
  const context = require('node:vm').createContext(Object.assign(sandbox, {
    setTimeout, clearTimeout, setInterval, clearInterval, performance, URL, Promise, console, Symbol,
    addEventListener() {},
  }));
  require('node:vm').runInContext(fs.readFileSync(path.join(__dirname, '../../remotion.js'), 'utf8'), context);
  return { fetch: (url, init) => context.fetch(url, init), fetched, fellBack, context };
};

const frameUrl = (port, src = 'vrf://project/public/a.mp4', time = 1) =>
  `http://localhost:${port}/proxy?src=${encodeURIComponent(src)}&time=${time}&transparent=false&toneMapped=true`;

const withServer = { nativeVideo: true, proxyPort: 4321, serveUrl: 'http://localhost:4400', timeoutInMilliseconds: 3000 };

test('a native frame is answered by the native path and never reaches the network', async () => {
  const page = load({ request: async () => ({ status: 200, via: 'native' }), config: withServer });
  const res = await page.fetch(frameUrl(4321));
  assert.equal(res.via, 'native');
  assert.equal(page.fetched.length, 0);
});

test('a native request that does not answer in half the page timeout goes to the frame server; the next one is native again', async () => {
  let asked = 0;
  const page = load({ request: () => (asked++ ? Promise.resolve({ status: 200, via: 'native' }) : new Promise(() => {})), config: withServer });
  const started = Date.now();
  const res = await page.fetch(frameUrl(4321), { signal: new AbortController().signal });
  assert.equal(res.via, 'server');
  assert.ok(Date.now() - started >= 1400, 'waited half the page timeout');
  const url = new URL(page.fetched[0].input);
  assert.equal(url.port, '4321');
  assert.equal(url.searchParams.get('src'), 'http://localhost:4400/public/a.mp4');
  assert.ok(page.fetched[0].signal, 'the abort signal goes along');
  assert.match(page.fellBack[0][1], /no native frame within 1500 ms/);
  const next = await page.fetch(frameUrl(4321, 'vrf://project/public/a.mp4', 2));
  assert.equal(next.via, 'native');
  assert.equal(asked, 2);
  assert.equal(page.fetched.length, 1);
});

test('a native decode failure goes to the frame server when one runs', async () => {
  const page = load({
    request: async () => {
      throw new Error('video decode failed at frame 3');
    },
    config: withServer,
  });
  const res = await page.fetch(frameUrl(4321));
  assert.equal(res.via, 'server');
  assert.match(page.fellBack[0][1], /native request failed: video decode failed/);
});

test('an aborted request rejects and is not retried on the frame server', async () => {
  const controller = new AbortController();
  const page = load({
    request: (src, time, signal) => new Promise((_, reject) => signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')))),
    config: withServer,
  });
  const pending = page.fetch(frameUrl(4321), { signal: controller.signal });
  controller.abort();
  await assert.rejects(pending, { name: 'AbortError' });
  assert.equal(page.fetched.length, 0);
});

test('a refused clip goes to the frame server started with the page', async () => {
  const page = load({ request: async () => null, config: withServer });
  await page.fetch(frameUrl(4321, 'vrf://project/public/b.webm'));
  assert.equal(new URL(page.fetched[0].input).searchParams.get('src'), 'http://localhost:4400/public/b.webm');
  assert.equal(page.fellBack.length, 0);
});

test('without a frame server the native path waits as before and a refusal uses the lazily started one', async () => {
  const lazy = { nativeVideo: true, proxyPort: 0, serveUrl: null, timeoutInMilliseconds: 3000 };
  const page = load({ request: async () => null, config: lazy });
  page.context.__vrf.video.fallback = () => ({ port: 5555, serveUrl: 'http://localhost:5600' });
  await page.fetch(frameUrl(0));
  const url = new URL(page.fetched[0].input);
  assert.equal(url.port, '5555');
  assert.equal(url.searchParams.get('src'), 'http://localhost:5600/public/a.mp4');
});

test('a long page timeout still hands a slow native request to the frame server after 15 s at most', async () => {
  const page = load({ request: () => new Promise(() => {}), config: { ...withServer, timeoutInMilliseconds: 120000 } });
  const started = Date.now();
  const pending = page.fetch(frameUrl(4321));
  const outcome = await Promise.race([pending.then(() => 'answered'), new Promise((r) => setTimeout(() => r('waiting'), 16000))]);
  assert.equal(outcome, 'answered');
  assert.ok(Date.now() - started >= 14900);
  assert.match(page.fellBack[0][1], /no native frame within 15000 ms/);
});
