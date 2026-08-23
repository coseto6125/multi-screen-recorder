const test = require('node:test');
const assert = require('node:assert');
const createTake = require('./take.js');

const blob = (bytes) => new Blob([Uint8Array.from(bytes)]);
const appends = (log) => log.filter((e) => e[0] === 'append_recording_chunk')
  .map((e) => Array.from(e[1]));

test('test_queue_chunk_appends_in_order', async () => {
  const log = [];
  const take = createTake({ invoke: async (cmd, data) => { log.push([cmd, data]); } });
  await take.start();
  assert.equal(take.queueChunk(blob([1, 2])), true);
  assert.equal(take.queueChunk(blob([3])), true);
  await take.drain();
  assert.deepEqual(appends(log), [[1, 2], [3]]);
});

test('test_queue_chunk_empty_is_ignored_not_queued', async () => {
  const log = [];
  const take = createTake({ invoke: async (cmd, data) => { log.push([cmd, data]); } });
  assert.equal(take.queueChunk(blob([])), true);
  await take.drain();
  assert.deepEqual(appends(log), []);
});

test('test_queue_chunk_backlog_over_bound_is_rejected', async () => {
  const log = [];
  let release;
  const hung = new Promise((resolve) => { release = resolve; });
  const take = createTake({
    invoke: async (cmd, data) => { log.push([cmd, data]); if (cmd === 'append_recording_chunk') return hung; },
    maxQueuedBytes: 1000,
  });
  assert.equal(take.queueChunk(blob(new Array(600).fill(1))), true);
  // The first append is still in flight, so the queue is non-empty and the
  // bound must reject a chunk that would push it past 1000 bytes.
  assert.equal(take.queueChunk(blob(new Array(600).fill(2))), false);
  release();
  await take.drain();
  assert.equal(appends(log).length, 1);
});

test('test_queue_chunk_first_oversized_chunk_still_taken', async () => {
  const log = [];
  const take = createTake({ invoke: async (cmd, data) => { log.push([cmd, data]); }, maxQueuedBytes: 10 });
  assert.equal(take.queueChunk(blob(new Array(50).fill(9))), true);
  await take.drain();
  assert.equal(appends(log).length, 1);
});

test('test_write_failure_notifies_on_fail_once_and_drain_rejects', async () => {
  const log = [];
  let failures = 0;
  const take = createTake({
    invoke: async (cmd, data) => {
      log.push([cmd, data]);
      if (appends(log).length === 1) throw new Error('disk full');
    },
    onFail: () => { failures += 1; },
  });
  take.queueChunk(blob([1]));
  await assert.rejects(() => take.drain(), /disk full/);
  assert.equal(take.failed, true);
  // Later chunks are dropped without reaching the backend, and onFail fired once.
  assert.equal(take.queueChunk(blob([2])), true);
  // A failed take keeps failing drain(): a caller must never read a clean one
  await assert.rejects(() => take.drain(), /disk full/);
  assert.deepEqual(appends(log), [[1]]);
  assert.equal(failures, 1);
});

test('test_fail_is_idempotent_first_error_wins', async () => {
  const take = createTake({ invoke: async () => {} });
  take.fail(new Error('first'));
  take.fail(new Error('second'));
  await assert.rejects(() => take.drain(), /first/);
});

test('test_mark_closing_drops_chunks', async () => {
  const log = [];
  const take = createTake({ invoke: async (cmd, data) => { log.push([cmd, data]); } });
  take.markClosing();
  assert.equal(take.queueChunk(blob([1])), true);
  await take.drain();
  assert.deepEqual(appends(log), []);
});

test('test_commit_resolves_saved_and_returns_path', async () => {
  const take = createTake({ invoke: async (cmd) => (cmd === 'finish_recording' ? '/r/a.webm' : undefined) });
  await take.start();
  const savedAssertion = assert.doesNotReject(() => take.saved);
  assert.equal(await take.commit(true), '/r/a.webm');
  await savedAssertion;
});

test('test_abort_returns_null_and_resolves_saved_when_invoke_rejects', async () => {
  const take = createTake({ invoke: async (cmd) => { if (cmd === 'abort_recording') throw new Error('no sink'); } });
  await take.start();
  const savedAssertion = assert.doesNotReject(() => take.saved);
  assert.equal(await take.abort(), null);
  await savedAssertion;
});

test('test_abort_passes_partial_path_through', async () => {
  const take = createTake({ invoke: async (cmd) => (cmd === 'abort_recording' ? '/r/a.partial.webm' : undefined) });
  assert.equal(await take.abort(), '/r/a.partial.webm');
});

test('test_start_twice_rejects', async () => {
  const take = createTake({ invoke: async () => {} });
  await take.start();
  await assert.rejects(() => take.start(), /already started/);
});

test('test_abort_gives_up_when_the_backend_never_answers', async () => {
  // A stalled disk holds the take lock, so abort_recording never returns. The
  // close handler awaits this call before destroying the window, so an
  // unbounded wait is what leaves the window unclosable.
  const take = createTake({ invoke: () => new Promise(() => {}) });
  assert.equal(await take.abort(20), null);
  await take.saved;
});

test('test_abort_without_a_timeout_waits_for_the_salvaged_path', async () => {
  // Every caller but the close handler wants the path, not a fast null: a slow
  // sync_all still ends with the bytes renamed, and the user is told where.
  const take = createTake({
    invoke: () => new Promise((resolve) => setTimeout(() => resolve('/r/a.partial.webm'), 30)),
  });
  assert.equal(await take.abort(), '/r/a.partial.webm');
  await take.saved;
});
