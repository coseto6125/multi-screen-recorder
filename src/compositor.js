// Capture-stream composition for recording: the screen grid (canvas), the
// webcam picture-in-picture, and system/mic audio merging. The layout math is
// pure and unit-tested; the composite and audio handles own their lifecycles,
// so UI code never touches cleanup arrays or a shared audio context.
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.gridCompositor = factory();
})(typeof self !== 'undefined' ? self : this, function () {
  'use strict';

  // Pure: frame layout for sources laid out `cols` per row. Cells are uniform,
  // sized by the largest source; the whole frame scales down (never up) to fit
  // within maxW x maxH at even dimensions; every cell letterboxes its source.
  // sizes: [{ w, h }] with w, h > 0.
  function planGrid(sizes, cols, maxW, maxH) {
    const rows = Math.ceil(sizes.length / cols);
    const cellW = Math.max(...sizes.map((s) => s.w));
    const cellH = Math.max(...sizes.map((s) => s.h));
    const naturalW = cols * cellW;
    const naturalH = rows * cellH;
    const scale = Math.min(1, maxW / naturalW, maxH / naturalH);
    const outW = Math.max(2, Math.round((naturalW * scale) / 2) * 2);
    const outH = Math.max(2, Math.round((naturalH * scale) / 2) * 2);
    const cw = outW / cols;
    const ch = outH / rows;
    const cells = sizes.map((s, i) => {
      const x = (i % cols) * cw;
      const y = Math.floor(i / cols) * ch;
      const fit = Math.min(cw / s.w, ch / s.h);
      return {
        dx: x + (cw - s.w * fit) / 2,
        dy: y + (ch - s.h * fit) / 2,
        dw: s.w * fit,
        dh: s.h * fit,
      };
    });
    return { outW, outH, cells };
  }

  // Pure: bottom-right picture-in-picture rect for the webcam overlay.
  function pipRect(outW, outH, srcW, srcH) {
    const padding = Math.max(8, Math.round(outW * 0.01));
    const w = Math.floor(outW * 0.2);
    const h = Math.floor(w * (srcH / srcW));
    return { dx: outW - w - padding, dy: outH - h - padding, dw: w, dh: h };
  }

  // Composite handle: lays screens out in a grid (1 -> 1x1, 2 -> 2x1, 3-4 ->
  // 2x2) plus an optional webcam overlay, drawing every frame with rAF and an
  // interval fallback so drawing continues when the window is occluded.
  // stop() releases the videos and timers exactly once.
  function createGridComposite({ streams, webcamTrack, maxW, maxH }) {
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d');
    const videos = streams.map((s) => {
      const v = document.createElement('video');
      v.autoplay = true;
      v.muted = true;
      v.srcObject = s;
      v.play().catch(() => {});
      return v;
    });

    let webcamVideo = null;
    if (webcamTrack) {
      webcamVideo = document.createElement('video');
      webcamVideo.autoplay = true;
      webcamVideo.muted = true;
      webcamVideo.srcObject = new MediaStream([webcamTrack]);
      webcamVideo.play().catch(() => {});
    }

    const cols = videos.length <= 1 ? 1 : 2;

    function draw() {
      if (!videos.every((v) => v.videoWidth > 0)) return;
      const plan = planGrid(
        videos.map((v) => ({ w: v.videoWidth, h: v.videoHeight })),
        cols,
        maxW,
        maxH
      );
      if (canvas.width !== plan.outW || canvas.height !== plan.outH) {
        canvas.width = plan.outW;
        canvas.height = plan.outH;
      }
      ctx.fillStyle = '#000';
      ctx.fillRect(0, 0, plan.outW, plan.outH);
      plan.cells.forEach((c, i) => {
        ctx.drawImage(videos[i], c.dx, c.dy, c.dw, c.dh);
      });
      if (webcamVideo && webcamVideo.videoWidth) {
        const pip = pipRect(plan.outW, plan.outH, webcamVideo.videoWidth, webcamVideo.videoHeight);
        ctx.drawImage(webcamVideo, pip.dx, pip.dy, pip.dw, pip.dh);
      }
    }

    let animId;
    function loop() {
      draw();
      animId = requestAnimationFrame(loop);
    }
    animId = requestAnimationFrame(loop);
    const intervalId = setInterval(draw, 33); // keeps drawing if rAF is throttled

    let stopped = false;
    return {
      stream: canvas.captureStream(60),
      stop() {
        if (stopped) return;
        stopped = true;
        cancelAnimationFrame(animId);
        clearInterval(intervalId);
        videos.forEach((v) => { v.srcObject = null; });
        if (webcamVideo) webcamVideo.srcObject = null;
      },
    };
  }

  // Audio merge handle: mixes system audio and mic into one stream; stop()
  // closes the AudioContext. A stream without audio tracks contributes nothing.
  function mergeAudio(systemStream, mic) {
    const context = new AudioContext();
    const destination = context.createMediaStreamDestination();
    for (const source of [systemStream, mic]) {
      if (source && source.getAudioTracks().length > 0) {
        context.createMediaStreamSource(source).connect(destination);
      }
    }
    let stopped = false;
    return {
      stream: destination.stream,
      stop() {
        if (stopped) return;
        stopped = true;
        context.close().catch(() => {});
      },
    };
  }

  return { planGrid, pipRect, createGridComposite, mergeAudio };
});
