'use strict';
// Unit tests for the file-viewer's PURE logic (§16): type classification, size caps, and the
// safe markdown renderer. DOM rendering itself is exercised live; this covers the bug-prone
// decision logic without a browser.
const { test } = require('node:test');
const assert = require('node:assert');
const { classify, renderMarkdown, extOf, TEXT_RENDER_CAP, IMAGE_RENDER_CAP } = require('../ui/fileViewerTab.js');

const bytesOf = (s) => new TextEncoder().encode(s);

// TC-FV1 extension detection
test('TC-FV1 extOf', () => {
  assert.equal(extOf('/a/b/readme.md'), 'md');
  assert.equal(extOf('/x/pic.PNG'), 'png');
  assert.equal(extOf('/x/Makefile'), '');
  assert.equal(extOf('/x/.bashrc'), '');   // leading-dot dotfile => no ext
});

// TC-FV2 classify: text / markdown / image by extension
test('TC-FV2 classify by type', () => {
  assert.equal(classify('/a.txt', 10, bytesOf('hi')).mode, 'text');
  assert.equal(classify('/a.md', 10, bytesOf('# hi')).mode, 'markdown');
  const img = classify('/a.png', 10, new Uint8Array([1, 2, 3]));
  assert.equal(img.mode, 'image');
  assert.equal(img.mime, 'image/png');
});

// TC-FV3 classify: binary content (non-image) -> download-only
test('TC-FV3 classify binary', () => {
  const nul = new Uint8Array([0x48, 0x00, 0x49]);   // contains NUL
  assert.equal(classify('/a.bin', 3, nul).mode, 'binary');
  // invalid UTF-8 also counts as binary
  const badUtf8 = new Uint8Array([0xff, 0xfe, 0xfd]);
  assert.equal(classify('/a.dat', 3, badUtf8).mode, 'binary');
});

// TC-FV4 size caps: text over 1MB and image over 5MB -> 'toobig'; under -> render
test('TC-FV4 tiered size caps', () => {
  assert.equal(classify('/a.txt', TEXT_RENDER_CAP + 1, bytesOf('x')).mode, 'toobig');
  assert.equal(classify('/a.txt', TEXT_RENDER_CAP, bytesOf('x')).mode, 'text');
  assert.equal(classify('/a.png', IMAGE_RENDER_CAP + 1, new Uint8Array([1])).mode, 'toobig');
  assert.equal(classify('/a.png', IMAGE_RENDER_CAP, new Uint8Array([1])).mode, 'image');
  // an image between the text cap and image cap still renders (image cap is higher)
  assert.equal(classify('/a.png', 3 * 1024 * 1024, new Uint8Array([1])).mode, 'image');
});

// TC-FV5 markdown renderer escapes content and never passes raw HTML (untrusted file, strict CSP)
test('TC-FV5 markdown is XSS-safe', () => {
  const html = renderMarkdown('# Hi <script>alert(1)</script>\n\n- **b** & <img>\n\n```\nraw <b>\n```');
  assert.ok(!html.includes('<script>'), 'no raw <script>');
  assert.ok(html.includes('&lt;script&gt;'), 'script escaped in heading');
  assert.ok(html.includes('raw &lt;b&gt;'), 'code fence content escaped');
  assert.ok(html.includes('<strong>b</strong>'), 'bold rendered');
  assert.ok(html.includes('&amp;'), 'ampersand escaped');
});

// TC-FV6 markdown links are routed through data-url (opened via app openExternal), only safe schemes
test('TC-FV6 markdown links use data-url and safe schemes', () => {
  const ok = renderMarkdown('[site](https://example.com)');
  assert.match(ok, /data-url="https:\/\/example\.com"/);
  assert.match(ok, /class="mdlink"/);
  // a javascript: link is NOT turned into a link (pattern only matches http/https/ftp/mailto)
  const bad = renderMarkdown('[x](javascript:alert(1))');
  assert.ok(!/mdlink/.test(bad), 'javascript: not linkified');
});
