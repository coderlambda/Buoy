'use strict';
// Unit tests for the file-viewer's PURE logic (§16): type classification, size caps, and the
// safe markdown renderer. DOM rendering itself is exercised live; this covers the bug-prone
// decision logic without a browser.
const { test } = require('node:test');
const assert = require('node:assert');
const { createFileViewerTab, classify, renderMarkdown, extOf, TEXT_RENDER_CAP, IMAGE_RENDER_CAP,
  HTML_RENDER_CAP } = require('../ui/fileViewerTab.js');

const bytesOf = (s) => new TextEncoder().encode(s);

// Tiny DOM stub — just enough for the viewer's createElement/appendChild/setAttribute usage. Used
// only by the HTML-sandbox test, whose whole point is the ATTRIBUTES on the generated <iframe>:
// those are the security boundary for untrusted file content, so they get asserted in CI rather
// than left to a live click-through.
// `fn` may be async, so this awaits it before restoring the globals (a plain try/finally would
// tear `document` down while the callback was still mid-await).
async function withFakeDom(fn) {
  const mk = (tag) => {
    const node = {
      tagName: String(tag).toLowerCase(), children: [], attrs: {}, style: { cssText: '' },
      className: '', textContent: '', _innerHTML: '',
      // Real DOM: assigning innerHTML replaces the subtree. renderInto() uses `= ''` to reset the
      // container before re-rendering, so the stub must actually drop children or a re-render would
      // still find the previous iframe.
      get innerHTML() { return this._innerHTML; },
      set innerHTML(v) { this._innerHTML = String(v); if (String(v) === '') this.children = []; },
      setAttribute(k, v) { this.attrs[k] = String(v); },
      getAttribute(k) { return Object.prototype.hasOwnProperty.call(this.attrs, k) ? this.attrs[k] : null; },
      appendChild(c) { this.children.push(c); return c; },
      querySelectorAll() { return []; },
      // depth-first search by tag, so a test can find the iframe wherever it's nested
      find(t) {
        if (this.tagName === t) return this;
        for (const c of this.children) { const hit = c.find && c.find(t); if (hit) return hit; }
        return null;
      },
      // depth-first search by className (buttons are nested inside the toolbar)
      findClass(cls) {
        if (this.className === cls) return this;
        for (const c of this.children) { const hit = c.findClass && c.findClass(cls); if (hit) return hit; }
        return null;
      },
    };
    return node;
  };
  const prevDoc = global.document, prevAtob = global.atob;
  global.document = { createElement: mk, createTextNode: (t) => ({ tagName: '#text', textContent: t, children: [], find: () => null }) };
  if (typeof global.atob !== 'function') {
    global.atob = (b64) => Buffer.from(b64, 'base64').toString('binary');
  }
  try { return await fn(mk); } finally { global.document = prevDoc; global.atob = prevAtob; }
}

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

// TC-FV7 GFM tables: header + separator + rows, alignment, escaping, and non-tables left alone
test('TC-FV7 markdown tables', () => {
  const html = renderMarkdown('| Name | Qty |\n|:-----|----:|\n| Apple | 3 |\n| Pear | 12 |');
  assert.match(html, /<table class="mdtable">/, 'table element');
  assert.match(html, /<th style="text-align:left">Name<\/th>/, 'left-aligned header');
  assert.match(html, /<th style="text-align:right">Qty<\/th>/, 'right-aligned header');
  assert.match(html, /<td style="text-align:right">12<\/td>/, 'aligned body cell');
  assert.ok(html.includes('<tbody>') && html.includes('</tbody>'), 'has tbody');
  // cell content still runs through inline (and is escaped)
  const esc = renderMarkdown('| a | b |\n|---|---|\n| <x> | **bold** |');
  assert.ok(esc.includes('&lt;x&gt;'), 'cell content escaped');
  assert.ok(esc.includes('<strong>bold</strong>'), 'inline markdown inside cells');
  // a lone pipe line WITHOUT a separator row is NOT a table (stays a paragraph)
  const notTable = renderMarkdown('a | b | c');
  assert.ok(!/mdtable/.test(notTable), 'no separator -> not a table');
});

// TC-FV8 self-contained HTML gets its own render mode, on the usual extensions
test('TC-FV8 classify html', () => {
  const page = bytesOf('<!doctype html><html><body>hi</body></html>');
  for (const p of ['/a/report.html', '/a/index.htm', '/a/doc.xhtml', '/A/REPORT.HTML']) {
    assert.equal(classify(p, page.length, page).mode, 'html', p);
  }
  // NOT html: a .txt that merely contains markup is still text (extension decides, as elsewhere)
  assert.equal(classify('/a/notes.txt', page.length, page).mode, 'text');
  // markdown still wins for .md even when the body is HTML-ish
  assert.equal(classify('/a/readme.md', page.length, page).mode, 'markdown');
});

// TC-FV9 html uses its own (higher) cap, not the shared text cap — self-contained files inline
// their images, so they are legitimately bigger than hand-written text.
test('TC-FV9 html size cap is independent of the text cap', () => {
  const b = bytesOf('<html>x</html>');
  assert.ok(HTML_RENDER_CAP > TEXT_RENDER_CAP, 'html cap is the looser one');
  // between the two caps: a .txt is refused, the same size as .html still renders
  const between = TEXT_RENDER_CAP + 1;
  assert.equal(classify('/a.txt', between, b).mode, 'toobig');
  assert.equal(classify('/a.html', between, b).mode, 'html');
  // boundary: at the cap renders, one past it does not
  assert.equal(classify('/a.html', HTML_RENDER_CAP, b).mode, 'html');
  assert.equal(classify('/a.html', HTML_RENDER_CAP + 1, b).mode, 'toobig');
});

// TC-FV10 a binary file named .html is still refused (the sniff runs before the html branch, so a
// mislabeled blob can't be fed to the parser)
test('TC-FV10 binary .html is not previewed', () => {
  const nul = new Uint8Array([0x3c, 0x00, 0x68]);        // '<' NUL 'h'
  assert.equal(classify('/a/evil.html', 3, nul).mode, 'binary');
  const badUtf8 = new Uint8Array([0xff, 0xfe, 0xfd]);
  assert.equal(classify('/a/evil.htm', 3, badUtf8).mode, 'binary');
});

// TC-FV11 THE SECURITY TEST for html preview. Hostile file content is handed to a real browser
// parser, so the iframe's isolation attributes are the boundary. Assert them exactly:
//   - sandbox is present and EMPTY (any allow-scripts => the file could run JS; any
//     allow-same-origin => it shares our origin and can reach window.__TAURI__ / the invoke bridge)
//   - the markup goes in via srcdoc, never through innerHTML on an app-origin element
test('TC-FV11 html preview is sandboxed and never touches app-origin innerHTML', async () => {
  const hostile = '<html><body><script>alert(1)</script><img src=x onerror=alert(2)></body></html>';
  const data_b64 = Buffer.from(hostile, 'utf8').toString('base64');
  await withFakeDom(async () => {
    const root = global.document.createElement('div');
    const api = { readRemoteFile: async () => ({ data_b64, size: hostile.length, truncated: false }) };
    const tab = createFileViewerTab({ id: 's1', path: '/tmp/eek.html', api }, { setStatus() {} });
    await tab.mount(root);

    const frame = root.find('iframe');
    assert.ok(frame, 'html mode renders an <iframe>');
    const sandbox = frame.getAttribute('sandbox');
    assert.equal(sandbox, '', 'sandbox must be present and empty');
    assert.ok(!/allow-scripts/.test(sandbox), 'never allow-scripts: file content must not execute');
    assert.ok(!/allow-same-origin/.test(sandbox), 'never allow-same-origin: no reach into our origin');
    // the untrusted markup is srcdoc, and is NOT assigned to any app-origin innerHTML
    assert.equal(frame.srcdoc, hostile, 'markup delivered via srcdoc');
    const noInnerHtml = (n) => {
      assert.ok(!String(n.innerHTML || '').includes('<script>'),
        `hostile markup must not reach innerHTML (${n.tagName})`);
      n.children.forEach(noInnerHtml);
    };
    noInnerHtml(root);
  });
});

// Helper: mount an html viewer and return { root, tab, calls } for the opt-in tests.
async function mountHtml(api, path = '/tmp/page.html') {
  const src = '<html><body><script>ran()</script></body></html>';
  const data_b64 = Buffer.from(src, 'utf8').toString('base64');
  const calls = [];
  const full = Object.assign({
    readRemoteFile: async () => ({ data_b64, size: src.length, truncated: false }),
  }, api);
  const wrapped = new Proxy(full, {
    get(t, k) {
      const v = t[k];
      if (typeof v === 'function' && k !== 'readRemoteFile') {
        return (...a) => { calls.push(k); return v(...a); };
      }
      return v;
    },
  });
  const root = global.document.createElement('div');
  const tab = createFileViewerTab({ id: 's1', path, api: wrapped }, { setStatus() {} });
  await tab.mount(root);
  return { root, tab, calls, src };
}

// TC-FV12 scripts are OPT-IN: a fresh html preview must never auto-enable them. This is the
// property that keeps merely CLICKING a path from executing a remote file's code.
test('TC-FV12 scripts are never enabled without an explicit click', async () => {
  await withFakeDom(async () => {
    const { root, calls } = await mountHtml({ enableHtmlScripts: async () => ({ url: 'buoyhtml://localhost/tok' }) });
    assert.ok(!calls.includes('enableHtmlScripts'), 'mount must not opt in by itself');
    assert.equal(root.find('iframe').getAttribute('sandbox'), '', 'still the static sandbox');
    // the opt-in affordance is offered
    const btn = root.findClass('fv-scripts');
    assert.ok(btn, 'an Enable-scripts button is present for html');
  });
});

// TC-FV13 after the explicit click: the frame switches to the buoyhtml: URL with allow-scripts, and
// still WITHOUT allow-same-origin (that combination is what keeps the origin opaque, so the
// scripted document cannot reach window.__TAURI__ / the invoke bridge).
test('TC-FV13 enabling scripts uses a separate origin and stays cross-origin', async () => {
  await withFakeDom(async () => {
    const URL_ = 'buoyhtml://localhost/abc123';
    const { root, calls } = await mountHtml({ enableHtmlScripts: async () => ({ url: URL_ }) });
    const btn = root.findClass('fv-scripts');
    await btn.onclick();

    assert.ok(calls.includes('enableHtmlScripts'), 'the click drives the opt-in command');
    const frame = root.find('iframe');
    const sandbox = frame.getAttribute('sandbox');
    assert.equal(sandbox, 'allow-scripts', 'scripts allowed, and nothing else');
    assert.ok(!/allow-same-origin/.test(sandbox),
      'NEVER allow-same-origin: opaque origin is what blocks IPC access');
    assert.ok(!/allow-popups|allow-top-navigation|allow-forms|allow-modals/.test(sandbox),
      'no popups/top-nav/forms/modals');
    // served from the separate origin, NOT inlined as srcdoc (srcdoc would inherit the app CSP)
    assert.equal(frame.src, URL_, 'loaded from the buoyhtml: origin');
    assert.ok(!frame.srcdoc, 'not srcdoc once scripted');
  });
});

// TC-FV14 the scripted state is per-tab and not persisted: a NEW viewer for the same path starts
// static again, so one opt-in can't silently apply to later files.
test('TC-FV14 script opt-in does not leak to a new tab', async () => {
  await withFakeDom(async () => {
    const api = { enableHtmlScripts: async () => ({ url: 'buoyhtml://localhost/t1' }) };
    const first = await mountHtml(api);
    const btn = first.root.findClass('fv-scripts');
    await btn.onclick();
    assert.equal(first.root.find('iframe').getAttribute('sandbox'), 'allow-scripts');

    const second = await mountHtml(api);            // same path, fresh tab
    assert.equal(second.root.find('iframe').getAttribute('sandbox'), '', 'new tab starts static');
    assert.ok(!second.calls.includes('enableHtmlScripts'), 'and does not auto-opt-in');
  });
});

// TC-FV15 non-html modes never offer the scripts opt-in (the button would be meaningless, and for
// text/markdown the content is escaped by us rather than parsed).
test('TC-FV15 no scripts opt-in for non-html modes', async () => {
  await withFakeDom(async () => {
    for (const p of ['/a/readme.md', '/a/notes.txt']) {
      const { root } = await mountHtml({ enableHtmlScripts: async () => ({ url: 'x' }) }, p);
      const btn = root.findClass('fv-scripts');
      assert.ok(!btn, 'no Enable-scripts button for ' + p);
    }
  });
});

// TC-FV16 the viewer's root must be free to be a flex column. renderer.js reveals a tab by CLEARING
// the inline display ('') rather than setting 'block' — an inline display:block outranks the
// stylesheet's `.fv-root { display:flex }`, which breaks `.fv-body { flex:1 }` and collapses the
// preview iframe to the CSS default 150px regardless of the tab's real height (measured: 150px in a
// 618px tab). This guards the contract from the viewer's side: the tab element must carry no inline
// display of its own, so whatever the stylesheet says wins.
test('TC-FV16 viewer root sets no inline display (flex column must survive reveal)', async () => {
  await withFakeDom(async () => {
    const { root, tab } = await mountHtml({});
    const el = tab.element();
    assert.ok(el, 'tab exposes its element');
    assert.equal(el.className, 'fv-root', 'the tab element IS the flex root');
    // the viewer must not hard-code a display on its own root...
    assert.ok(!/display\s*:/.test(el.style.cssText || ''),
      'no inline display on the viewer root: ' + el.style.cssText);
    // ...nor on the body/iframe whose flex sizing does the stretching
    for (const cls of ['fv-body', 'fv-html']) {
      const n = root.findClass(cls);
      if (n) {
        assert.ok(!/display\s*:/.test(n.style.cssText || ''),
          `no inline display on .${cls}: ` + n.style.cssText);
      }
    }
  });
});
