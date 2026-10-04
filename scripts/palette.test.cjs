const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');

// Exercise the public extractor with decoded, uniformly coloured covers.
function extractor(dark) {
  let cover;
  const colours = { orange: [220, 100, 30], green: [40, 180, 70] };
  const context = {
    window: {},
    matchMedia: () => ({ matches: dark }),
    document: {
      createElement: () => ({
        getContext: () => ({
          clearRect() {},
          drawImage(img) { cover = img.src; },
          getImageData: () => ({
            data: Uint8ClampedArray.from(
              Array.from({ length: 42 * 42 }, () => [...colours[cover], 255]).flat(),
            ),
          }),
        }),
      }),
    },
    Image: class { decode() { return Promise.resolve(); } },
  };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../src/palette.js'), 'utf8'), context);
  return context.window.palette.extract;
}

for (const dark of [false, true]) {
  test(`Cover tint off removes artwork colours (${dark ? 'dark' : 'light'})`, async () => {
    const extract = extractor(dark);
    const orange = await extract('orange', 0);
    const green = await extract('green', 0);
    assert.equal(orange.accent, green.accent, 'controls must not change colour with the cover');
    assert.equal(orange.accentSoft, green.accentSoft);
    assert.equal(orange.accent, '#d7788b', 'disabled tint uses the fixed muted rose accent');
    assert.equal(orange.accentSoft, 'rgba(215, 120, 139, 0.23)');
    for (const key of ['wash1', 'wash2', 'wash3', 'washBase']) {
      assert.match(orange[key], /, 0\)$/);
      assert.match(green[key], /, 0\)$/);
    }
    const enabled = await extract('green', 1);
    assert.notEqual(enabled.accent, green.accent, 're-enabling restores cached artwork colour');
    const next = await extract('orange', 0.5);
    assert.notEqual(next.accent, enabled.accent, 'enabled tint still follows the cover');
  });
}
