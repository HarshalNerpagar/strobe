const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

globalThis.__vrf_video_send = () => {};
(0, eval)(fs.readFileSync(path.join(__dirname, '../../video.js'), 'utf8'));
const { chooseFrame, swsRgb, swsConfig } = globalThis.__vrf.video;

const VFR = [0, 33, 66, 166, 183, 216, 249, 348, 366, 399, 432, 532, 549, 582];
const CFR = Array.from({ length: 120 }, (_, i) => i * 512);

test("the nearest frame wins, as in Remotion's frame server (probed on vfr1080.mp4)", () => {
  const remotion = [
    [0, 0], [32, 1], [33, 1], [65, 2], [66, 2], [100, 2], [165, 3], [166, 3], [200, 5], [215, 5], [216, 5],
    [248, 6], [249, 6], [250, 6], [300, 7], [333, 7], [348, 7], [349, 7], [400, 9],
  ];
  for (const [ms, index] of remotion) assert.equal(chooseFrame(VFR, ms), index, `${ms} ms`);
});

test("ties and unrounded targets follow Remotion on a constant frame rate (probed on cfr1080.mp4)", () => {
  const ts = 15360;
  const remotion = [
    [0.016, 0], [0.0166, 0], [0.016666666666666666, 1], [0.01667, 1], [0.0168, 1],
    [0.05, 2], [0.0499, 1], [0.050000001, 2], [0.083333333333333, 2], [0.1, 3],
  ];
  for (const [seconds, index] of remotion) assert.equal(chooseFrame(CFR, seconds * ts), index, `${seconds} s`);
});

test('targets are truncated to whole ticks and switch at the midpoint between two frames (probed on every gap near 4.5 s)', () => {
  const gap = [4425, 4458, 4558, 4575, 4608, 4641, 4741, 4758, 4791];
  for (const [tick, index] of [[4441, 0], [4442, 1], [4566, 2], [4567, 3], [4591, 3], [4592, 4], [4749, 6], [4750, 7]]) {
    assert.equal(chooseFrame(gap, tick), index, `${tick}`);
  }
  assert.equal(chooseFrame(gap, 4566.666666666666), 2);
  assert.equal(chooseFrame(gap, 4566.999), 2);
});

test('before the first frame the earliest is shown, after the last the last', () => {
  assert.equal(chooseFrame([23040, 23552, 24064], 0), 0);
  assert.equal(chooseFrame([23040, 23552, 24064], -5), 0);
  assert.equal(chooseFrame(VFR, 10_000), VFR.length - 1);
});

test('every output frame of a constant frame rate clip shows its own frame', () => {
  for (let frame = 0; frame < 120; frame++) assert.equal(chooseFrame(CFR, (frame / 30) * 15360), frame, `frame ${frame}`);
});

const { chooseFrameAtOrBefore, frameRule } = globalThis.__vrf.video;

test("Remotion 4.0.506 and later show the last frame at or before the time (probed on 25 and 29.97 fps clips)", () => {
  const remotion = {
    25: [5, 5, 6, 7, 8, 9, 10, 10, 11, 12, 13, 14, 15, 15, 16, 17, 18, 19, 20, 20, 21, 22, 23, 24, 25, 25, 26, 27, 28, 29],
    29.97: [5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34],
  };
  const tracks = { 25: [12800, 512], 29.97: [30000, 1001] };
  for (const [rate, shown] of Object.entries(remotion)) {
    const [timescale, step] = tracks[rate];
    const pts = Array.from({ length: 90 }, (_, i) => i * step);
    for (const [i, index] of shown.entries()) {
      const frame = i + 6;
      assert.equal(chooseFrameAtOrBefore(pts, (frame / 30) * timescale), index, `${rate} fps, output frame ${frame}`);
    }
  }
});

test('a time on a frame shows that frame even when the product rounds just below it', () => {
  assert.equal(chooseFrameAtOrBefore([0, 512, 1024], 1023.9999999), 2);
  assert.equal(chooseFrameAtOrBefore([0, 512, 1024], 1023.9), 1);
  assert.equal(chooseFrameAtOrBefore([23040, 23552], 0), 0);
  assert.equal(chooseFrameAtOrBefore(CFR, 1e9), CFR.length - 1);
});

test('the frame rule follows the Remotion version, and an unknown version keeps the nearest frame', () => {
  assert.equal(frameRule('4.0.541'), chooseFrameAtOrBefore);
  assert.equal(frameRule('4.0.506'), chooseFrameAtOrBefore);
  assert.equal(frameRule('4.1.0'), chooseFrameAtOrBefore);
  assert.equal(frameRule('5.0.0-alpha.1'), chooseFrameAtOrBefore);
  assert.equal(frameRule('4.0.505'), chooseFrame);
  assert.equal(frameRule('3.3.100'), chooseFrame);
  assert.equal(frameRule(undefined), chooseFrame);
  assert.equal(frameRule('next'), chooseFrame);
});

const oracle = JSON.parse(fs.readFileSync(path.join(__dirname, 'swscale-oracle.json'), 'utf8'));
const matrixOf = (tag) => [tag.replace('f', ''), tag.endsWith('f')];

for (const [tag, samples] of Object.entries(oracle)) {
  test(`the integer model reproduces Remotion's libswscale for ${tag}`, () => {
    const [matrix, full] = matrixOf(tag);
    const config = swsConfig(matrix, full);
    for (const [y, u, v, r, g, b] of samples) assert.deepEqual(swsRgb(config, y, u, v), [r, g, b], `YUV ${y},${u},${v}`);
  });
}

const frameBuffer = (id, k) => {
  const buffer = new ArrayBuffer(32 + 4 * 2 + 2 * 1 * 2);
  const head = new DataView(buffer);
  head.setUint32(0, 1, true);
  head.setUint32(4, id, true);
  head.setUint32(8, k, true);
  head.setUint32(12, 4, true);
  head.setUint32(16, 2, true);
  return buffer;
};

const metaBuffer = (req, id, count) => {
  const json = new TextEncoder().encode(JSON.stringify({ req, eligible: true, width: 4, height: 2, timescale: 1000, matrix: '709', full: false }));
  const at = 32 + ((json.length + 7) & ~7);
  const buffer = new ArrayBuffer(at + count * 8);
  const head = new DataView(buffer);
  head.setUint32(0, 2, true);
  head.setUint32(4, id, true);
  head.setUint32(8, count, true);
  head.setUint32(12, json.length, true);
  new Uint8Array(buffer, 32, json.length).set(json);
  const pts = new Float64Array(buffer, at, count);
  for (let i = 0; i < count; i++) pts[i] = i * 1000;
  return buffer;
};

const fakeGl = new Proxy({}, { get: (_, key) => (/^[A-Z0-9_]+$/.test(key) ? 0 : () => true) });
const fakeCanvas = () => ({ getContext: () => fakeGl, remove() {}, style: {}, width: 1, height: 1 });

const withVideoPage = async (body) => {
  const saved = { send: globalThis.__vrf_video_send, document: globalThis.document, offscreen: globalThis.OffscreenCanvas };
  const images = [];
  globalThis.document = { images, createElement: fakeCanvas };
  globalThis.OffscreenCanvas = class {
    getContext() {}
    convertToBlob() {
      return Promise.resolve(new Blob([new Uint8Array([1, 2, 3])], { type: 'image/png' }));
    }
  };
  let nextClip = 7;
  globalThis.__vrf_video_send = (json) => {
    const message = JSON.parse(json);
    if (message.t === 'open') globalThis.__vrf_video_push(metaBuffer(message.req, nextClip++, 200));
    if (message.t === 'want' && !message.hit) globalThis.__vrf_video_push(frameBuffer(message.clip, message.k));
  };
  try {
    await body(images);
  } finally {
    globalThis.__vrf_video_send = saved.send;
    globalThis.document = saved.document;
    globalThis.OffscreenCanvas = saved.offscreen;
  }
};

const fetchUrl = async (src, time) => {
  const blob = await (await globalThis.__vrf.video.request(src, time)).blob();
  return URL.createObjectURL(blob);
};

const fetchUrlFromBytes = async (src, time) => {
  const res = await globalThis.__vrf.video.request(src, time);
  const blob = new Blob([await res.arrayBuffer()], { type: res.headers.get('Content-Type') ?? '' });
  return URL.createObjectURL(blob);
};

const showImage = (images, src) => {
  const img = { src, className: '', style: { cssText: '' }, parentNode: { insertBefore() {} } };
  images.splice(0, images.length, img);
};

test('frames shown once and replaced are not kept for the life of the page', async () => {
  await withVideoPage(async (images) => {
    const video = globalThis.__vrf.video;
    for (let t = 0; t < 40; t++) {
      showImage(images, await fetchUrl('clip-a.mp4', t));
      assert.equal(video.present(), 1);
    }
    assert.equal(video.sources.size, 1);
  });
});

test('a requested frame stays resolvable until it has been presented, even if its URL is revoked first', async () => {
  await withVideoPage(async (images) => {
    const video = globalThis.__vrf.video;
    const url = await fetchUrl('clip-b.mp4', 3);
    URL.revokeObjectURL(url);
    assert.equal(video.sources.has(url), true);
    showImage(images, url);
    assert.equal(video.present(), 1);
    assert.equal(video.sources.has(url), true);
    images.length = 0;
    video.present();
    assert.equal(video.sources.has(url), false);
  });
});

test('revoking the URL of a frame that was presented and left the page forgets it', async () => {
  await withVideoPage(async (images) => {
    const video = globalThis.__vrf.video;
    const url = await fetchUrl('clip-c.mp4', 5);
    showImage(images, url);
    video.present();
    images.length = 0;
    URL.revokeObjectURL(url);
    assert.equal(video.sources.has(url), false);
  });
});

test('frames requested but never shown are dropped after a few presents', async () => {
  await withVideoPage(async () => {
    const video = globalThis.__vrf.video;
    const url = await fetchUrl('clip-d.mp4', 9);
    for (let i = 0; i < 12; i++) video.present();
    assert.equal(video.sources.has(url), false);
  });
});

test('the decoded frame cache of a clip stays bounded when frames are requested one after another', async () => {
  await withVideoPage(async () => {
    const video = globalThis.__vrf.video;
    for (let t = 0; t < 60; t++) await video.request('clip-e.mp4', t);
    const clip = [...video.clips.values()].at(-1);
    assert.ok(clip.frames.size <= 15, `${clip.frames.size}`);
  });
});

test('a frame read as bytes and wrapped in a new Blob, as Remotion 4.0.540 and later do, is shown', async () => {
  await withVideoPage(async (images) => {
    const video = globalThis.__vrf.video;
    const url = await fetchUrlFromBytes('clip-f.mp4', 4);
    assert.equal(video.sources.get(url)?.k, 4);
    showImage(images, url);
    assert.equal(video.present(), 1);
  });
});

test('each request read as bytes keeps its own frame', async () => {
  await withVideoPage(async () => {
    const video = globalThis.__vrf.video;
    const first = await fetchUrlFromBytes('clip-g.mp4', 2);
    const second = await fetchUrlFromBytes('clip-g.mp4', 6);
    assert.equal(video.sources.get(first)?.k, 2);
    assert.equal(video.sources.get(second)?.k, 6);
  });
});

test('the frame response says it carries a PNG', async () => {
  await withVideoPage(async () => {
    const res = await globalThis.__vrf.video.request('clip-h.mp4', 1);
    assert.equal(res.headers.get('Content-Type'), 'image/png');
  });
});

test('blobs the page builds itself are untouched and still Blobs', async () => {
  const blob = new Blob([new Uint8Array([9, 8, 7]).buffer], { type: 'text/plain' });
  assert.ok(blob instanceof Blob);
  assert.equal(blob.size, 3);
  assert.equal(await blob.text(), '\t\b\u0007');
  class Named extends Blob {}
  assert.ok(new Named(['x']) instanceof Named);
  const url = URL.createObjectURL(blob);
  assert.equal(globalThis.__vrf.video.sources.has(url), false);
  URL.revokeObjectURL(url);
});

// Remotion's OffthreadVideo aborts its fetch when the video unmounts or its frame changes, and clears its delayRender() handle only
// when that fetch rejects with an AbortError (or once the image is set). A native request that ignored the abort left the handle
// open: "A delayRender() "Fetching http://localhost:0/proxy?..." was called but not cleared" (Nvidia Short #1 v6.2, 2026-11-15).
const withHeldFrames = async (body) => {
  const held = [];
  await withVideoPage(async (images) => {
    const send = globalThis.__vrf_video_send;
    globalThis.__vrf_video_send = (json) => {
      const message = JSON.parse(json);
      if (message.t === 'want') held.push(message);
      else send(json);
    };
    const release = () => {
      for (const message of held.splice(0)) if (!message.hit) globalThis.__vrf_video_push(frameBuffer(message.clip, message.k));
    };
    await body({ images, release, held });
  });
};

test('a request aborted while its frame is being decoded rejects with an AbortError', async () => {
  await withHeldFrames(async ({ release }) => {
    const controller = new AbortController();
    const pending = globalThis.__vrf.video.request('abort-a.mp4', 2, controller.signal);
    await new Promise((r) => setTimeout(r, 5));
    assert.match(globalThis.__vrf.video.diagnose(), /1 request\(s\) in flight \[abort-a\.mp4 t=2 frame 2/);
    controller.abort();
    await assert.rejects(pending, (error) => error.name === 'AbortError' && /aborted/.test(error.message));
    release();
    assert.match(globalThis.__vrf.video.diagnose(), /0 request\(s\) in flight, \d+ aborted/);
  });
});

test('a request whose signal is already aborted rejects at once', async () => {
  await withHeldFrames(async () => {
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(globalThis.__vrf.video.request('abort-b.mp4', 1, controller.signal), { name: 'AbortError' });
  });
});

test('reading the body of an aborted response rejects, as a fetch body does', async () => {
  await withVideoPage(async () => {
    const controller = new AbortController();
    const res = await globalThis.__vrf.video.request('abort-c.mp4', 3, controller.signal);
    controller.abort();
    await assert.rejects(res.arrayBuffer(), { name: 'AbortError' });
    await assert.rejects(res.blob(), { name: 'AbortError' });
  });
});

test('a request that was not aborted resolves and shows its frame, and an abort after it is harmless', async () => {
  await withHeldFrames(async ({ images, release }) => {
    const controller = new AbortController();
    const pending = globalThis.__vrf.video.request('abort-d.mp4', 4, controller.signal);
    await new Promise((r) => setTimeout(r, 5));
    release();
    const res = await pending;
    const blob = new Blob([await res.arrayBuffer()], { type: 'image/png' });
    controller.abort();
    const url = URL.createObjectURL(blob);
    assert.equal(globalThis.__vrf.video.sources.get(url)?.k, 4);
    showImage(images, url);
    assert.equal(globalThis.__vrf.video.present(), 1);
  });
});

test('an aborted request does not stop a later request for the same frame', async () => {
  await withHeldFrames(async ({ release }) => {
    const first = new AbortController();
    const aborted = globalThis.__vrf.video.request('abort-e.mp4', 5, first.signal);
    await new Promise((r) => setTimeout(r, 5));
    first.abort();
    await assert.rejects(aborted, { name: 'AbortError' });
    const again = globalThis.__vrf.video.request('abort-e.mp4', 5, new AbortController().signal);
    await new Promise((r) => setTimeout(r, 5));
    release();
    const res = await again;
    assert.equal(res.status, 200);
  });
});
