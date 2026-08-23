// Per-take frontend sink for the streaming recorder.
//
// Owns everything about getting MediaRecorder chunks onto disk in order: the
// one-append-at-a-time chain, the backpressure bound, error latching, and the
// saved promise the window close handler waits on. UI code only ever starts,
// queues to, drains, commits or aborts a take — it never touches the queue's
// internals, so every ordering or backpressure fix lands in this one file.
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.createTake = factory();
})(typeof self !== 'undefined' ? self : this, function () {
  'use strict';

  // Stop the take rather than let the queue grow without bound when the disk
  // falls behind the encoder: an unbounded backlog is the memory failure the
  // streaming fix removes.
  const DEFAULT_MAX_QUEUED_BYTES = 32 * 1024 * 1024;

  // A stalled disk holds the backend's take lock for as long as the write
  // blocks, and abort_recording needs that same lock. Waiting on it without a
  // bound is what leaves the window unclosable, so give up and let the caller
  // proceed: the bytes already on disk keep their .part name either way.
  const ABORT_TIMEOUT_MS = 5000;

  function createTake({ invoke, onFail, maxQueuedBytes } = {}) {
    const limit = maxQueuedBytes || DEFAULT_MAX_QUEUED_BYTES;
    let chain = Promise.resolve();
    let queued = 0;
    let error = null;
    let closing = false;
    let started = false;
    let resolveSaved;
    // Resolves once the file is closed and named, whether the take succeeded
    // or not. The close handler waits on this instead of racing onstop for
    // the same sink.
    const saved = new Promise((resolve) => { resolveSaved = resolve; });

    // Latches the first failure and stops the recorder exactly once. A write
    // that fails must stop the take: left running, the recorder would keep
    // producing chunks that are dropped for as long as the user records.
    function fail(err) {
      if (error) return;
      error = err;
      if (onFail) onFail(err);
    }

    return {
      saved,

      get failed() { return error !== null; },

      async start() {
        if (started) throw new Error('Take already started');
        started = true;
        await invoke('start_recording');
      },

      // Hands a chunk to the queue. Empty chunks are ignored, chunks after a
      // failure or a close request are dropped, and false means the backlog
      // bound rejected the chunk so the caller must fail the take.
      queueChunk(chunk) {
        if (chunk.size === 0 || error || closing) return true;
        // An empty queue always takes the chunk: the bound is there to catch
        // a backlog, not to reject one oversized chunk the disk could absorb.
        if (queued > 0 && queued + chunk.size > limit) return false;
        queued += chunk.size;
        chain = chain
          .then(async () => {
            if (error) return;
            const buffer = await chunk.arrayBuffer();
            await invoke('append_recording_chunk', new Uint8Array(buffer));
          })
          .catch(fail)
          .then(() => { queued -= chunk.size; });
        return true;
      },

      fail,

      // Stops accepting chunks the file will never receive (window closing).
      markClosing() {
        closing = true;
      },

      // Resolves once every queued append has settled. Rejects with the
      // latched write error so callers cannot mistake a failed drain for a
      // clean one.
      async drain() {
        await chain;
        if (error) throw error;
      },

      // Closes and names the file. Resolves saved on success.
      async commit(remux) {
        const savedPath = await invoke('finish_recording', { remux });
        resolveSaved();
        return savedPath;
      },

      // Salvages whatever reached disk under a partial name, or null when
      // nothing usable exists. Resolves saved even when the command fails:
      // every exit path must release a close handler waiting on this take.
      async abort(timeoutMs = ABORT_TIMEOUT_MS) {
        let timer;
        try {
          return await Promise.race([
            invoke('abort_recording'),
            new Promise((resolve) => { timer = setTimeout(() => resolve(null), timeoutMs); }),
          ]);
        } catch (err) {
          return null;
        } finally {
          // Race leaves the loser running; an uncleared timer keeps the take
          // object alive for the full timeout after the abort already returned.
          clearTimeout(timer);
          resolveSaved();
        }
      },
    };
  }

  return createTake;
});
