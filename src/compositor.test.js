const test = require('node:test');
const assert = require('node:assert');
const { planGrid, pipRect } = require('./compositor.js');

const approx = (actual, expected, msg) =>
  assert.ok(Math.abs(actual - expected) < 1e-6, `${msg}: ${actual} != ${expected}`);

test('test_plan_grid_single_source_within_cap_keeps_natural_size', () => {
  const plan = planGrid([{ w: 1920, h: 1080 }], 1, 3840, 2160);
  assert.equal(plan.outW, 1920);
  assert.equal(plan.outH, 1080);
  approx(plan.cells[0].dx, 0, 'dx');
  approx(plan.cells[0].dy, 0, 'dy');
  approx(plan.cells[0].dw, 1920, 'dw');
});

test('test_plan_grid_two_sources_center_in_uniform_cells', () => {
  // Landscape beside portrait: cells size by the larger dimension of each axis
  const plan = planGrid([{ w: 1920, h: 1080 }, { w: 1080, h: 1920 }], 2, 7680, 4320);
  assert.equal(plan.outW, 3840); // 2 * 1920, no upscale beyond cap
  assert.equal(plan.outH, 1920);
  // 2 sources in 2 columns = a single row of 1920x1920 cells
  const [landscape, portrait] = plan.cells;
  approx(portrait.dy, 0, 'portrait fills its cell height');
  approx(portrait.dh, 1920, 'portrait cell height');
  approx(portrait.dx, 1920 + (1920 - portrait.dw) / 2, 'portrait centers in its cell');
  approx(landscape.dw, 1920, 'landscape fills its cell width');
  approx(landscape.dy, (1920 - 1080) / 2, 'landscape letterboxes vertically');
  approx(landscape.dx, 0, 'landscape starts at its cell edge');
});

test('test_plan_grid_three_sources_fill_a_2x2_grid_with_an_empty_cell', () => {
  const sizes = [{ w: 1280, h: 720 }, { w: 1280, h: 720 }, { w: 1280, h: 720 }];
  const plan = planGrid(sizes, 2, 1920, 1080);
  assert.equal(plan.outW, 1920);
  assert.equal(plan.outH, 1080);
  assert.equal(plan.cells.length, 3);
  // Third tile sits at column 0 of row 1
  approx(plan.cells[2].dy, 540, 'third tile on the second row');
  approx(plan.cells[2].dx, 0, 'third tile on the left column');
});

test('test_plan_grid_downscale_caps_and_keeps_even_dimensions', () => {
  const plan = planGrid([{ w: 3000, h: 700 }], 1, 1000, 1000);
  assert.equal(plan.outW, 1000);
  assert.equal(plan.outH, 234);
  assert.equal(plan.outH % 2, 0);
});

test('test_plan_grid_never_upscales_small_sources', () => {
  const plan = planGrid([{ w: 640, h: 360 }], 1, 1920, 1080);
  assert.equal(plan.outW, 640);
  assert.equal(plan.outH, 360);
});

test('test_pip_rect_anchors_bottom_right_and_preserves_aspect', () => {
  const r = pipRect(2000, 1000, 640, 480);
  assert.deepEqual(r, { dx: 1580, dy: 680, dw: 400, dh: 300 });
});

test('test_pip_rect_padding_floors_at_eight_pixels', () => {
  const r = pipRect(500, 500, 640, 480);
  assert.deepEqual(r, { dx: 392, dy: 417, dw: 100, dh: 75 }); // padding = max(8, 5) = 8
});
