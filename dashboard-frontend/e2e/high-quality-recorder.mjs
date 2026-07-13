import { spawn, spawnSync } from 'node:child_process';
import { stat } from 'node:fs/promises';

const FFMPEG = '/usr/local/bin/ffmpeg';
const FPS = 25;

function childFailure(name, status, stderr = '') {
  const detail = stderr.trim() ? `: ${stderr.trim().slice(-2_000)}` : '';
  return new Error(`${name} exited with status ${status}${detail}`);
}

/**
 * High-fidelity replacement for Playwright's built-in 1 Mbps VP8 recorder.
 *
 * Playwright still owns and drives Chromium. A CDP session requests quality-100 JPEG compositor
 * frames, which are timestamp-expanded to a 25 fps constant-rate stream exactly like Playwright's
 * own recorder. The intermediate uses lossless H.264, so the later delivery encode starts from the
 * screencast frames rather than from Playwright's hard-coded low-bitrate WebM.
 */
export class HighQualityRecorder {
  static async start(page, outputPath, options = {}) {
    const recorder = new HighQualityRecorder(page, outputPath, options);
    await recorder.start();
    return recorder;
  }

  constructor(page, outputPath, { width = 1920, height = 1080, quality = 100 } = {}) {
    this.page = page;
    this.outputPath = outputPath;
    this.width = width & ~1;
    this.height = height & ~1;
    this.quality = quality;
    this.session = null;
    this.process = null;
    this.closePromise = null;
    this.pending = Promise.resolve();
    this.firstTimestamp = null;
    this.lastFrame = null;
    this.lastFrameWallMs = 0;
    this.error = null;
    this.stopped = false;
    this.stderr = '';
    this.onScreencastFrame = (event) => {
      this.pending = this.pending
        .then(() => this.writeTimestampedFrame(event))
        .catch((error) => { this.error ??= error; })
        .finally(() => this.session?.send('Page.screencastFrameAck', { sessionId: event.sessionId }).catch(() => {}));
    };
  }

  async start() {
    this.session = await this.page.context().newCDPSession(this.page);
    const filter = `pad=${this.width}:${this.height}:0:0:gray,crop=${this.width}:${this.height}:0:0,format=yuv420p`;
    this.process = spawn(FFMPEG, [
      '-loglevel', 'error',
      '-f', 'image2pipe',
      '-avioflags', 'direct',
      '-fpsprobesize', '0',
      '-probesize', '32',
      '-analyzeduration', '0',
      '-c:v', 'mjpeg',
      '-i', 'pipe:0',
      '-y', '-an', '-r', String(FPS),
      '-vf', filter,
      '-c:v', 'libx264', '-preset', 'ultrafast', '-qp', '0',
      this.outputPath,
    ], { stdio: ['pipe', 'ignore', 'pipe'] });

    this.process.stderr.setEncoding('utf8');
    this.process.stderr.on('data', (chunk) => { this.stderr = `${this.stderr}${chunk}`.slice(-8_000); });
    this.closePromise = new Promise((resolve, reject) => {
      this.process.once('error', reject);
      this.process.once('close', (status) => {
        if (status === 0) resolve();
        else reject(childFailure('lossless recorder ffmpeg', status, this.stderr));
      });
    });

    this.session.on('Page.screencastFrame', this.onScreencastFrame);
    await this.session.send('Page.startScreencast', {
      format: 'jpeg',
      quality: this.quality,
      maxWidth: this.width,
      maxHeight: this.height,
      everyNthFrame: 1,
    });
  }

  async writeTimestampedFrame(event) {
    if (this.stopped) return;
    const timestamp = event.metadata?.timestamp || Date.now() / 1_000;
    if (this.firstTimestamp === null) this.firstTimestamp = timestamp;
    const frameNumber = Math.floor((timestamp - this.firstTimestamp) * FPS);
    if (this.lastFrame) {
      const repeatCount = Math.max(0, frameNumber - this.lastFrame.frameNumber);
      for (let index = 0; index < repeatCount; index += 1) await this.writeFrame(this.lastFrame.buffer);
    }
    this.lastFrame = { buffer: Buffer.from(event.data, 'base64'), frameNumber };
    this.lastFrameWallMs = performance.now();
  }

  async writeFrame(buffer) {
    if (!this.process?.stdin.writable) throw new Error('lossless recorder stdin closed unexpectedly');
    await new Promise((resolve, reject) => {
      this.process.stdin.write(buffer, (error) => error ? reject(error) : resolve());
    });
  }

  async stop() {
    if (this.stopped) return;
    this.stopped = true;
    await this.session?.send('Page.stopScreencast').catch(() => {});
    await this.pending;

    if (this.lastFrame) {
      const tailSeconds = Math.max((performance.now() - this.lastFrameWallMs) / 1_000, 1);
      const repeatCount = Math.ceil(tailSeconds * FPS);
      for (let index = 0; index < repeatCount; index += 1) await this.writeFrame(this.lastFrame.buffer);
    }

    this.process?.stdin.end();
    await this.closePromise;
    this.session?.off('Page.screencastFrame', this.onScreencastFrame);
    await this.session?.detach().catch(() => {});
    if (!this.lastFrame) throw new Error('CDP screencast produced no frames');
    if (this.error) throw this.error;
  }
}

function runFfmpeg(name, args) {
  const result = spawnSync(FFMPEG, args, { stdio: 'inherit' });
  if (result.status !== 0) throw childFailure(name, result.status);
}

/**
 * Encode from the lossless source at a strict target bitrate. `nal-hrd=cbr` is intentional:
 * dashboard footage has long low-entropy stretches where CRF and ordinary ABR both undershoot by
 * 5x even at near-lossless settings. CBR retains ample bandwidth for scrolling/text transitions
 * and uses legal filler NAL units during static stretches, yielding a predictable shareable size.
 */
export async function encodeDeliveryMp4({
  sourcePath,
  outputPath,
  preset = 'slow',
  bitrateKbps = 2_150,
}) {
  runFfmpeg(`strict ${bitrateKbps} kbps delivery encode`, [
    '-y', '-loglevel', 'warning',
    '-i', sourcePath,
    '-vf', 'scale=in_range=pc:out_range=tv,format=yuv420p',
    '-c:v', 'libx264', '-preset', preset,
    '-b:v', `${bitrateKbps}k`,
    '-minrate', `${bitrateKbps}k`,
    '-maxrate', `${bitrateKbps}k`,
    '-bufsize', `${bitrateKbps * 2}k`,
    '-x264-params', 'nal-hrd=cbr:force-cfr=1',
    '-color_range', 'tv', '-pix_fmt', 'yuv420p', '-movflags', '+faststart',
    '-an', outputPath,
  ]);
  const sizeBytes = (await stat(outputPath)).size;
  return { bitrateKbps, sizeBytes, sizeMb: sizeBytes / 1_000_000 };
}
