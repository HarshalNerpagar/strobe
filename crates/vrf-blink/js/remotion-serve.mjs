import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const [projectDir, bundleDir] = process.argv.slice(2, 4).map((p) => resolve(p));
// Decode threads of Remotion's compositor (`offthreadVideoThreads`; prepareServer ignores `concurrency`). One thread served
// every view and fell behind when the views' slow native requests were handed to it under CPU load (s01 v6.6, 2026-11-15).
const threads = Math.max(1, Number(process.argv[4]) || 2);
process.chdir(projectDir);
const { RenderInternals } = createRequire(`${projectDir}/package.json`)('@remotion/renderer');
const server = await RenderInternals.prepareServer({
  webpackConfigOrServeUrl: bundleDir,
  port: null,
  remotionRoot: projectDir,
  concurrency: 1,
  offthreadVideoThreads: threads,
  sampleRate: 48000,
  logLevel: 'error',
  indent: false,
  offthreadVideoCacheSizeInBytes: null,
  binariesDirectory: null,
  forceIPv4: true,
});
process.stdout.write(`${JSON.stringify({ serveUrl: server.serveUrl, offthreadPort: server.offthreadPort })}\n`);
process.stdin.resume();
process.stdin.on('end', async () => {
  await server.closeServer(true);
  process.exit(0);
});
