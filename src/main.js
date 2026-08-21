/**
 * Multi Screen Recorder - frontend
 * Screen capture via getDisplayMedia, composition via canvas, MediaRecorder,
 * save/convert through Tauri commands.
 */

(function () {
  const { invoke, convertFileSrc } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const appWindow = window.__TAURI__.window.getCurrentWindow();

  const MAX_SCREENS = 4;

  // --- Elements ---
  const screenGrid = document.getElementById('screen-grid');
  const screenEmpty = document.getElementById('screen-empty');
  const sourceHint = document.getElementById('source-hint');
  const sourceError = document.getElementById('source-error');
  const btnAddScreen = document.getElementById('btn-add-screen');
  const toggleMic = document.getElementById('toggle-mic');
  const toggleSystemAudio = document.getElementById('toggle-system-audio');
  const toggleWebcam = document.getElementById('toggle-webcam');
  const toggleConvertMp4 = document.getElementById('toggle-convert-mp4');
  const toggleStopTimer = document.getElementById('toggle-stop-timer');
  const timerMinutes = document.getElementById('timer-minutes');
  const timerSeconds = document.getElementById('timer-seconds');
  const timerCountdownEl = document.getElementById('timer-countdown');
  const bitrateChips = document.getElementById('bitrate-chips');
  const bitrateHint = document.getElementById('bitrate-hint');
  const codecSegmented = document.getElementById('codec-segmented');
  const statusEl = document.getElementById('status');
  const statusDot = document.getElementById('status-dot');
  const durationEl = document.getElementById('duration');
  const recIndicator = document.getElementById('rec-indicator');
  const btnRecord = document.getElementById('btn-record');
  const btnPreview = document.getElementById('btn-preview');
  const btnConvertFile = document.getElementById('btn-convert-file');
  const previewModal = document.getElementById('preview-modal');
  const previewVideo = document.getElementById('preview-video');
  const previewTitle = document.getElementById('preview-title');
  const btnClosePreview = document.getElementById('btn-close-preview');
  const recordingsPathEl = document.getElementById('recordings-path');
  const btnChangeRecordings = document.getElementById('btn-change-recordings');
  const btnOpenRecordings = document.getElementById('btn-open-recordings');
  const convertProgressEl = document.getElementById('convert-progress');
  const convertProgressLabel = document.getElementById('convert-progress-label');
  const convertProgressFill = document.getElementById('convert-progress-fill');
  const convertProgressPct = document.getElementById('convert-progress-pct');

  // --- State ---
  let screens = []; // { id, stream, label, tile }
  let screenSeq = 0;
  let selectedCodec = 'vp8';
  let selectedBitrate = 3000000;
  let maxRes = { w: 1920, h: 1080 };
  let phase = 'idle'; // idle | starting | recording | finalizing
  let mediaRecorder = null;
  let recordStream = null;
  let take = null; // per-recording sink (src/take.js); see startRecording
  let webcamStream = null;
  let micStream = null;
  let audioContext = null;
  let compositeCleanups = [];
  let durationInterval = null;
  let startTime = null;
  let stopTimeoutId = null;
  let countdownIntervalId = null;
  let lastSavedPath = null;

  // --- Titlebar ---
  document.getElementById('tl-close').addEventListener('click', () => appWindow.close());
  // Covers the titlebar button and Alt-F4 alike: the file gets closed and named before
  // the webview goes away, so a take in progress survives the close instead of vanishing.
  appWindow.onCloseRequested(async (event) => {
    if (phase === 'idle') return; // not prevented: the window closes as usual
    event.preventDefault();
    try {
      setStatus('Saving recording…', 'busy');
      const current = take;
      if (phase === 'finalizing' && current) {
        // onstop already owns the file; aborting here would race its own commit
        await withTimeout(current.saved, 30000);
      } else if (current) {
        current.markClosing(); // stop queueing chunks the file will never receive
        await withTimeout(current.drain().catch(() => {}), 10000);
        await current.abort();
      } else {
        await invoke('abort_recording').catch(() => null);
      }
    } finally {
      await appWindow.destroy();
    }
  });
  document.getElementById('tl-min').addEventListener('click', () => appWindow.minimize());
  document.getElementById('tl-max').addEventListener('click', () => appWindow.toggleMaximize());

  // --- Helpers ---
  function showError(msg) {
    sourceError.textContent = msg || '';
    sourceError.classList.toggle('hidden', !msg);
  }

  function setStatus(text, kind) {
    statusEl.textContent = text;
    statusDot.className = 'status-dot' + (kind ? ' ' + kind : '');
  }

  function baseName(p) {
    return p.split(/[/\\]/).pop();
  }

  function updateHint() {
    if (screens.length === 0) {
      sourceHint.textContent = 'Add one or more screens — they are combined into a grid in a single video.';
    } else {
      const layout = screens.length === 2 ? '2x1' : screens.length > 2 ? '2x2' : '';
      sourceHint.textContent = screens.length + ' screen' + (screens.length > 1 ? 's' : '') +
        ' added' + (layout ? ' — combined as a ' + layout + ' grid in a single video.' : '.');
    }
    btnAddScreen.disabled = screens.length >= MAX_SCREENS || phase !== 'idle';
    screenEmpty.classList.toggle('hidden', screens.length > 0);
  }

  // One place decides what the controls look like, so a take can never leave the UI
  // claiming it is idle while its file is still open.
  function setPhase(next) {
    phase = next;
    btnRecord.disabled = next === 'starting' || next === 'finalizing';
    btnRecord.classList.toggle('recording', next === 'recording');
    btnRecord.title = next === 'recording' ? 'Stop recording' : 'Start recording';
    recIndicator.classList.toggle('hidden', next !== 'recording');
    btnConvertFile.disabled = next !== 'idle';
    updateHint();
  }

  function showConvertProgress(fileName, verb) {
    convertProgressEl.classList.remove('hidden');
    convertProgressLabel.textContent = (verb || 'Converting') + ' ' + (fileName || '…');
    convertProgressFill.style.width = '0%';
    convertProgressPct.textContent = '0%';
  }

  function updateConvertProgress(percent, fileName, verb) {
    convertProgressFill.style.width = percent + '%';
    convertProgressPct.textContent = percent + '%';
    if (fileName) convertProgressLabel.textContent = (verb || 'Converting') + ' ' + fileName;
  }

  // A stalled disk or a wedged ffmpeg must not trap the user in a window that will
  // not close, so every wait on the way out is bounded.
  function withTimeout(promise, ms) {
    return Promise.race([
      promise,
      new Promise((resolve) => setTimeout(resolve, ms))
    ]);
  }

  function errText(err) {
    if (!err) return 'Unknown error';
    return String(err.message || err);
  }

  function hideConvertProgress() {
    convertProgressEl.classList.add('hidden');
  }

  // --- Screen management ---
  async function addScreen() {
    showError('');
    if (screens.length >= MAX_SCREENS) return;
    let stream;
    try {
      stream = await navigator.mediaDevices.getDisplayMedia({
        video: { frameRate: { ideal: 60 } },
        // Ask for system audio on the first screen; user can tick "share audio" in the picker
        audio: screens.length === 0
      });
    } catch (e) {
      if (e && (e.name === 'NotAllowedError' || e.name === 'AbortError')) return; // user canceled
      showError('Could not capture screen: ' + (e.message || e.name || 'unknown error'));
      return;
    }

    const id = ++screenSeq;
    const track = stream.getVideoTracks()[0];
    let label = (track && track.label) || '';
    if (!label || /^(screen|window|web-contents-media-stream):/i.test(label)) {
      const surface = track && track.getSettings ? track.getSettings().displaySurface : null;
      label = (surface === 'window' ? 'Window' : 'Screen') + ' ' + id;
    }

    const tile = document.createElement('div');
    tile.className = 'screen-tile';
    const video = document.createElement('video');
    video.autoplay = true;
    video.muted = true;
    video.srcObject = stream;
    const badge = document.createElement('span');
    badge.className = 'tile-badge';
    const labelEl = document.createElement('span');
    labelEl.className = 'tile-label';
    labelEl.textContent = label;
    labelEl.title = label;
    const removeBtn = document.createElement('button');
    removeBtn.className = 'tile-remove';
    removeBtn.innerHTML = '&times;';
    removeBtn.title = 'Remove this screen';
    removeBtn.addEventListener('click', () => removeScreen(id));
    tile.append(video, badge, labelEl, removeBtn);
    screenGrid.appendChild(tile);

    const entry = { id, stream, label, tile };
    screens.push(entry);

    // If user stops sharing from the OS/browser UI
    if (track) {
      track.addEventListener('ended', () => {
        stopRecording();
        removeScreen(id, true);
      });
    }

    renumberBadges();
    updateHint();
  }

  // `force` is for a source that has already ended: its tile must go even mid-take.
  function removeScreen(id, force) {
    const idx = screens.findIndex((s) => s.id === id);
    if (idx < 0) return;
    if (!force && phase !== 'idle') return;
    const [entry] = screens.splice(idx, 1);
    entry.stream.getTracks().forEach((t) => t.stop());
    entry.tile.remove();
    renumberBadges();
    updateHint();
  }

  function renumberBadges() {
    screens.forEach((s, i) => {
      s.tile.querySelector('.tile-badge').textContent = String(i + 1);
    });
  }

  // --- Audio merge ---
  function mergeAudioStreams(systemStream, mic) {
    audioContext = new AudioContext();
    const destination = audioContext.createMediaStreamDestination();
    if (systemStream && systemStream.getAudioTracks().length > 0) {
      audioContext.createMediaStreamSource(systemStream).connect(destination);
    }
    if (mic && mic.getAudioTracks().length > 0) {
      audioContext.createMediaStreamSource(mic).connect(destination);
    }
    return destination.stream;
  }

  // --- Canvas composition ---
  // Lays screens out in a grid (1 -> 1x1, 2 -> 2x1, 3-4 -> 2x2), each cell letterboxed,
  // then scales the whole canvas down (never up) to fit within maxW x maxH.
  // Uses rAF plus an interval fallback so drawing continues when the window is occluded.
  function startComposite(screenStreams, webcamTrack, maxW, maxH) {
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d');
    const videos = screenStreams.map((s) => {
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

    const n = videos.length;
    const cols = n <= 1 ? 1 : 2;
    const rows = Math.ceil(n / cols);

    function draw() {
      if (!videos.every((v) => v.videoWidth > 0)) return;
      // Uniform cells sized by the largest source, capped to the selected resolution
      const cellW = Math.max(...videos.map((v) => v.videoWidth));
      const cellH = Math.max(...videos.map((v) => v.videoHeight));
      const naturalW = cols * cellW;
      const naturalH = rows * cellH;
      const scale = Math.min(1, maxW / naturalW, maxH / naturalH);
      const outW = Math.max(2, Math.round((naturalW * scale) / 2) * 2);
      const outH = Math.max(2, Math.round((naturalH * scale) / 2) * 2);
      if (canvas.width !== outW || canvas.height !== outH) {
        canvas.width = outW;
        canvas.height = outH;
      }
      ctx.fillStyle = '#000';
      ctx.fillRect(0, 0, outW, outH);
      const cw = outW / cols;
      const ch = outH / rows;
      videos.forEach((v, i) => {
        const cx = (i % cols) * cw;
        const cy = Math.floor(i / cols) * ch;
        const s = Math.min(cw / v.videoWidth, ch / v.videoHeight);
        const dw = v.videoWidth * s;
        const dh = v.videoHeight * s;
        ctx.drawImage(v, cx + (cw - dw) / 2, cy + (ch - dh) / 2, dw, dh);
      });
      if (webcamVideo && webcamVideo.videoWidth) {
        const padding = Math.max(8, Math.round(outW * 0.01));
        const w = Math.floor(outW * 0.2);
        const h = Math.floor(w * (webcamVideo.videoHeight / webcamVideo.videoWidth));
        ctx.drawImage(webcamVideo, outW - w - padding, outH - h - padding, w, h);
      }
    }

    let animId;
    function loop() {
      draw();
      animId = requestAnimationFrame(loop);
    }
    animId = requestAnimationFrame(loop);
    const intervalId = setInterval(draw, 33); // keeps drawing if rAF is throttled

    compositeCleanups.push(() => {
      cancelAnimationFrame(animId);
      clearInterval(intervalId);
      videos.forEach((v) => { v.srcObject = null; });
      if (webcamVideo) webcamVideo.srcObject = null;
    });

    return canvas.captureStream(60);
  }

  function stopComposite() {
    compositeCleanups.forEach((cb) => cb());
    compositeCleanups = [];
  }

  // --- Recording ---
  async function startRecording() {
    if (phase !== 'idle') return; // permissions and IPC below await; a second entry would race
    showError('');
    if (screens.length === 0) {
      showError('Please add at least one screen first.');
      return;
    }
    setPhase('starting');

    const useMic = toggleMic.checked;
    const useSystemAudio = toggleSystemAudio.checked;
    const useWebcam = toggleWebcam.checked;

    micStream = null;
    webcamStream = null;

    if (useMic) {
      try {
        micStream = await navigator.mediaDevices.getUserMedia({ audio: true });
      } catch (e) {
        showError('Microphone not available: ' + (e.message || 'permission denied'));
        setPhase('idle');
        return;
      }
    }

    if (useWebcam) {
      try {
        webcamStream = await navigator.mediaDevices.getUserMedia({ video: true });
      } catch (e) {
        showError('Webcam not available: ' + (e.message || 'permission denied'));
        if (micStream) micStream.getTracks().forEach((t) => t.stop());
        micStream = null;
        setPhase('idle');
        return;
      }
    }

    // Video track: single screen within the resolution cap and no webcam records
    // directly; otherwise composite through the canvas (grid + scaling)
    let videoTrack;
    let canvasStream = null;
    const track0 = screens[0].stream.getVideoTracks()[0];
    const settings0 = track0 && track0.getSettings ? track0.getSettings() : {};
    const exceedsCap =
      (settings0.width || 0) > maxRes.w || (settings0.height || 0) > maxRes.h;
    const needComposite = screens.length > 1 || (useWebcam && webcamStream) || exceedsCap;
    if (needComposite) {
      canvasStream = startComposite(
        screens.map((s) => s.stream),
        useWebcam && webcamStream ? webcamStream.getVideoTracks()[0] : null,
        maxRes.w,
        maxRes.h
      );
      videoTrack = canvasStream.getVideoTracks()[0];
    } else {
      videoTrack = track0;
    }

    // Audio: system audio comes from the first screen's display stream (if shared)
    const tracks = [videoTrack];
    if (useMic || useSystemAudio) {
      const merged = mergeAudioStreams(useSystemAudio ? screens[0].stream : null, useMic ? micStream : null);
      if (merged.getAudioTracks().length > 0) tracks.push(merged.getAudioTracks()[0]);
    }
    recordStream = new MediaStream(tracks);

    const videoBitrate = selectedBitrate;
    let mimeType = 'video/webm';
    if (selectedCodec === 'vp9' && MediaRecorder.isTypeSupported('video/webm;codecs=vp9')) {
      mimeType = 'video/webm;codecs=vp9';
    } else if (MediaRecorder.isTypeSupported('video/webm;codecs=vp8')) {
      mimeType = 'video/webm;codecs=vp8';
    }
    try {
      mediaRecorder = new MediaRecorder(recordStream, {
        mimeType,
        videoBitsPerSecond: videoBitrate,
        audioBitsPerSecond: 128000
      });
    } catch (e) {
      mediaRecorder = new MediaRecorder(recordStream);
    }

    // Open the output file first: a disk error must surface now, not after an hour
    // of recording. From here on every chunk goes straight to that file.
    // The take owns the ordered write queue, the backpressure bound, error
    // latching and the saved promise the close handler waits on (src/take.js).
    const current = createTake({
      invoke,
      // A write that fails must stop the take. Left running, the recorder would
      // keep producing chunks that are dropped for as long as the user records.
      onFail: () => stopRecording(),
    });
    take = current;
    try {
      await current.start();
    } catch (e) {
      // No sink was opened, but every exit still goes through abort(): it
      // releases the saved promise the close handler may already be waiting on.
      await current.abort();
      take = null;
      cleanupAfterRecording();
      setPhase('idle');
      showError('Cannot start recording: ' + errText(e));
      return;
    }

    // Stream each chunk to disk as it arrives; the ordering, backpressure and
    // failure rules live inside the take (src/take.js).
    mediaRecorder.ondataavailable = (e) => {
      if (!current.queueChunk(e.data)) {
        current.fail(new Error('Disk cannot keep up with the recording'));
      }
    };

    mediaRecorder.onerror = (e) => {
      current.fail((e && e.error) || new Error('Recorder failed'));
    };

    mediaRecorder.onstop = async () => {
      clearAutoStop();
      stopDurationTimer();
      setStatus('Finalizing…', 'busy');

      const doConvert = toggleConvertMp4.checked;
      cleanupAfterRecording();

      let savedPath;
      try {
        await current.drain(); // drain the appends still in flight; rejects on a latched write error
        // A conversion rewrites the timestamps anyway, so only a kept WebM needs the fix
        savedPath = await current.commit(!doConvert);
      } catch (err) {
        // Whatever reached disk before the failure is kept under a .partial.webm name
        const partial = await current.abort();
        hideConvertProgress();
        setStatus('Error', 'error');
        showError(errText(err) + (partial ? ' — incomplete recording kept: ' + partial : ''));
        endTake(current);
        return;
      }
      hideConvertProgress();
      lastSavedPath = savedPath;
      btnPreview.disabled = false;
      setStatus('Saved: ' + baseName(savedPath));
      if (!doConvert) {
        endTake(current);
        return;
      }

      // The WebM is already safe on disk; a failed conversion must not lose it
      try {
        setStatus('Converting to MP4…', 'busy');
        showConvertProgress(baseName(savedPath));
        const mp4Path = await invoke('convert_to_mp4', { webmPath: savedPath });
        lastSavedPath = mp4Path;
        setStatus('Saved: ' + baseName(mp4Path));
      } catch (err) {
        setStatus('Error', 'error');
        showError('MP4 conversion failed, WebM kept: ' + errText(err));
      }
      hideConvertProgress();
      endTake(current);
    };

    // timeslice 1000 ms: hand a chunk over every second, so at most one second of
    // recording is ever in flight rather than sitting in the webview
    try {
      mediaRecorder.start(1000);
    } catch (e) {
      await current.abort();
      take = null;
      cleanupAfterRecording();
      setPhase('idle');
      showError('Cannot start recording: ' + errText(e));
      return;
    }
    startTime = Date.now();
    startDurationTimer();
    setStatus('Recording…', 'recording');
    setPhase('recording');

    // Auto-stop timer
    if (toggleStopTimer.checked) {
      const min = Math.max(0, parseInt(timerMinutes.value, 10) || 0);
      const sec = Math.max(0, Math.min(59, parseInt(timerSeconds.value, 10) || 0));
      const totalMs = (min * 60 + sec) * 1000;
      if (totalMs > 0) {
        stopTimeoutId = setTimeout(() => stopRecording(), totalMs);
        timerCountdownEl.classList.remove('hidden');
        let remaining = min * 60 + sec;
        const tick = () => {
          if (remaining < 0) return;
          const m = Math.floor(remaining / 60);
          const s = remaining % 60;
          timerCountdownEl.textContent = 'Stops in ' + String(m).padStart(2, '0') + ':' + String(s).padStart(2, '0');
          remaining--;
        };
        tick();
        countdownIntervalId = setInterval(tick, 1000);
      }
    }
  }

  function endTake(t) {
    if (take === t) take = null;
    setPhase('idle');
  }

  function clearAutoStop() {
    if (stopTimeoutId) { clearTimeout(stopTimeoutId); stopTimeoutId = null; }
    if (countdownIntervalId) { clearInterval(countdownIntervalId); countdownIntervalId = null; }
    timerCountdownEl.classList.add('hidden');
  }

  function cleanupAfterRecording() {
    // Screens keep streaming for the next take; only stop recording-specific resources
    stopComposite();
    if (recordStream) {
      // Only stop tracks we created (canvas/merged audio), not the live screen tracks
      recordStream.getTracks().forEach((t) => {
        const isScreenTrack = screens.some((s) => s.stream.getTracks().includes(t));
        if (!isScreenTrack) t.stop();
      });
      recordStream = null;
    }
    if (webcamStream) {
      webcamStream.getTracks().forEach((t) => t.stop());
      webcamStream = null;
    }
    if (micStream) {
      micStream.getTracks().forEach((t) => t.stop());
      micStream = null;
    }
    if (audioContext) {
      audioContext.close().catch(() => {});
      audioContext = null;
    }
    mediaRecorder = null;
  }

  // Only onstop returns the phase to idle, so the record button stays disabled until
  // the file this take opened is closed.
  function stopRecording() {
    if (phase !== 'recording') return;
    setPhase('finalizing');
    clearAutoStop();
    // The recorder stops itself once every track has ended, and stop() then throws;
    // its own onstop still runs and closes the take, so there is nothing to do here.
    if (!mediaRecorder || mediaRecorder.state === 'inactive') return;
    mediaRecorder.stop();
  }

  function startDurationTimer() {
    durationInterval = setInterval(() => {
      if (!startTime) return;
      const sec = Math.floor((Date.now() - startTime) / 1000);
      const m = Math.floor(sec / 60);
      const s = sec % 60;
      durationEl.textContent = String(m).padStart(2, '0') + ':' + String(s).padStart(2, '0');
    }, 500);
  }

  function stopDurationTimer() {
    if (durationInterval) { clearInterval(durationInterval); durationInterval = null; }
    durationEl.textContent = '00:00';
    startTime = null;
  }

  // --- Preview ---
  function showPreview() {
    if (!lastSavedPath) return;
    previewVideo.src = convertFileSrc(lastSavedPath);
    previewTitle.textContent = baseName(lastSavedPath);
    previewModal.classList.remove('hidden');
  }

  function closePreview() {
    previewVideo.pause();
    previewVideo.src = '';
    previewModal.classList.add('hidden');
  }

  // --- Convert existing file ---
  async function convertFile() {
    showError('');
    setStatus('Choose a file…', 'busy');
    try {
      const result = await invoke('convert_file_to_mp4');
      hideConvertProgress();
      if (result.canceled) { setStatus('Ready'); return; }
      if (result.error) {
        setStatus('Error', 'error');
        showError(result.error);
        return;
      }
      lastSavedPath = result.path;
      btnPreview.disabled = false;
      setStatus('Converted: ' + baseName(result.path));
    } catch (err) {
      hideConvertProgress();
      setStatus('Error', 'error');
      showError(String(err));
    }
  }

  // --- Events from backend ---
  listen('convert-progress', (event) => {
    const { fileName, percent, stage } = event.payload;
    const verb = stage === 'finalize' ? 'Finalizing' : 'Converting';
    if (convertProgressEl.classList.contains('hidden')) showConvertProgress(fileName, verb);
    updateConvertProgress(percent, fileName, verb);
  });

  // --- UI wiring ---
  btnAddScreen.addEventListener('click', addScreen);
  btnRecord.addEventListener('click', () => {
    if (phase === 'recording') {
      stopRecording();
      return;
    }
    // Nothing awaits startRecording, so an unexpected throw would otherwise strand the
    // phase at 'starting' and leave the record button disabled for the rest of the session
    startRecording().catch((e) => {
      showError('Cannot start recording: ' + errText(e));
      if (phase !== 'starting') return;
      cleanupAfterRecording();
      if (take) take.abort();
      take = null;
      setPhase('idle');
    });
  });
  btnPreview.addEventListener('click', showPreview);
  btnConvertFile.addEventListener('click', convertFile);
  btnClosePreview.addEventListener('click', closePreview);
  previewModal.addEventListener('click', (e) => {
    if (e.target === previewModal) closePreview();
  });

  bitrateChips.querySelectorAll('.chip').forEach((chip) => {
    chip.addEventListener('click', () => {
      selectedBitrate = parseInt(chip.dataset.rate, 10) || 3000000;
      bitrateChips.querySelectorAll('.chip').forEach((c) => c.classList.toggle('active', c === chip));
      bitrateHint.textContent = chip.dataset.hint || '';
    });
  });

  const resChips = document.getElementById('res-chips');
  const resHint = document.getElementById('res-hint');
  resChips.querySelectorAll('.chip').forEach((chip) => {
    chip.addEventListener('click', () => {
      maxRes = {
        w: parseInt(chip.dataset.w, 10) || 1920,
        h: parseInt(chip.dataset.h, 10) || 1080
      };
      resChips.querySelectorAll('.chip').forEach((c) => c.classList.toggle('active', c === chip));
      resHint.textContent = chip.dataset.hint || '';
    });
  });

  codecSegmented.querySelectorAll('.seg-btn').forEach((btn) => {
    btn.addEventListener('click', () => {
      selectedCodec = btn.dataset.codec;
      codecSegmented.querySelectorAll('.seg-btn').forEach((b) => b.classList.toggle('active', b === btn));
    });
  });

  function updateTimerInputs() {
    const disabled = !toggleStopTimer.checked;
    timerMinutes.disabled = disabled;
    timerSeconds.disabled = disabled;
  }
  toggleStopTimer.addEventListener('change', updateTimerInputs);
  updateTimerInputs();

  btnChangeRecordings.addEventListener('click', async () => {
    try {
      const result = await invoke('change_recordings_path');
      if (result && !result.canceled && result.path) {
        recordingsPathEl.textContent = result.path;
        recordingsPathEl.title = result.path;
      }
    } catch (e) {
      showError(String(e));
    }
  });

  btnOpenRecordings.addEventListener('click', () => {
    invoke('open_recordings_folder').catch((e) => showError(String(e)));
  });

  // --- Init ---
  invoke('get_recordings_path')
    .then((p) => {
      recordingsPathEl.textContent = p || '—';
      recordingsPathEl.title = p || '';
    })
    .catch(() => {});
  setPhase('idle');
  setStatus('Ready');
})();
