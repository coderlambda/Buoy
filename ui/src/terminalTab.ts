
// Built-in 'terminal' tab-kind (§14/§15). Wraps an xterm.js terminal as a polymorphic
// TabContent so the project/tab machinery can host it generically alongside future tab kinds
// (markdown, browser, ...). Only terminal tabs bind to the tmux backend; other kinds ignore
// onData/resize. This module is the reference implementation of the TabContent interface.
/* global Terminal, FitAddon, CanvasAddon */

// spec: { id, meta, linkProvider, linkHandler }  ctx: { input(bytes), ack(id,bytes), onReady?, ... }
// Returns a TabContent: { kind, mount(el), onData(d), resize(c,r), focus(), dispose(),
//                         readBuffer() (test hook), term (raw xterm) }.
export interface TerminalTabSpec {
  linkHandler?: unknown;
  linkProvider?: XtermLinkProvider;
  /** Reconcile iOS dictation's evolving full-sentence insertText snapshots. */
  mobileTextInput?: boolean;
}

export interface TerminalTabContext {
  input(data: string): void;
  ack?(bytes: number): void;
  copyText?(text: string): void;
  setStatus?(message: string): void;
  log?(message: string): void;
  onBell?(): void;
  onInteract?(): void;
}

let repaintCount = 0;
let inputLatencyStarted: number | null = null;
let inputLatencyResult: number | null = null;

function codePoints(value: string): string[] { return Array.from(value); }

function commonPrefixLength(left: string[], right: string[]): number {
  const end = Math.min(left.length, right.length);
  let index = 0;
  while (index < end && left[index] === right[index]) index += 1;
  return index;
}

export interface MobileDomEdit {
  output: string;
  deleteCount: number;
  insertCount: number;
  prefixLength: number;
}

/**
 * Turn the actual edit WebKit made to xterm's helper textarea into terminal bytes. iOS dictation
 * does not expose a composition lifecycle, but it does keep replacing its previous hypothesis in
 * the textarea. Keeping that text storage and applying its replacement is the strategy used by
 * native iOS terminals such as SwiftTerm; relying on InputEvent.data loses the replacement range.
 */
export class MobileDomInputReconciler {
  reconcile(before: string, after: string): MobileDomEdit {
    const oldPoints = codePoints(before);
    const newPoints = codePoints(after);
    const prefixLength = commonPrefixLength(oldPoints, newPoints);
    const deleteCount = oldPoints.length - prefixLength;
    const inserted = newPoints.slice(prefixLength);
    return {
      output: '\x7f'.repeat(deleteCount) + inserted.join(''),
      deleteCount,
      insertCount: inserted.length,
      prefixLength,
    };
  }
}

// Diagnostic used by the native webview suite. Incrementing only after refresh succeeds makes the
// counter detect both a missed call site and a renderer whose refresh primitive stopped working.
export function getTerminalRepaintCount(): number { return repaintCount; }
export function armTerminalInputLatency(): void {
  inputLatencyStarted = performance.now();
  inputLatencyResult = null;
}
export function getTerminalInputLatency(): number | null { return inputLatencyResult; }

export function createTerminalTab(spec: TerminalTabSpec, ctx: TerminalTabContext) {
  const options: XtermTerminalOptions = {
    // SF Mono covers terminal box drawing on iOS; the later fallbacks cover CJK, symbols and emoji.
    // xterm still owns wcwidth/cell placement, while Canvas can select a fallback glyph per cell.
    fontFamily: spec.mobileTextInput
      ? '"SFMono-Regular", "SF Mono", Menlo, Monaco, "PingFang SC", "Apple Symbols", "Apple Color Emoji", monospace'
      : 'Menlo, Consolas, monospace',
    fontSize: spec.mobileTextInput ? 12 : 13,
    lineHeight: spec.mobileTextInput ? 1.08 : 1,
    rescaleOverlappingGlyphs: true,
    theme: spec.mobileTextInput
      ? { background: '#090c12', foreground: '#eef3fa' }
      : { background: '#1e1e2e', foreground: '#cdd6f4' },
    scrollback: 5000,
    // §21: handle OSC 8 hyperlinks (embedded-URI links). Without this xterm renders them
    // underlined but the click is a no-op in the Tauri webview.
  };
  if (spec.linkHandler) options.linkHandler = spec.linkHandler;
  const term = new Terminal(options);
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  // §13 regex-based URL/path links (our plugin engine).
  if (spec.linkProvider) term.registerLinkProvider(spec.linkProvider);

  // Clipboard support. Two independent paths, both landing in the system clipboard via ctx.copyText:
  //   1. OSC 52 (remote-driven copy): a program on the remote (Claude Code, tmux, vim) selects text
  //      and emits ESC ] 52 ; c ; <base64> ST to set the clipboard. xterm.js ignores OSC 52 by
  //      default (security: any program could overwrite the clipboard), so WE opt in here — decode
  //      the base64 and write it. Without this the remote prints "Sent N chars via OSC 52" but the
  //      text never reaches the Mac clipboard.
  //   2. Local Cmd/Ctrl+C and right-click (user-driven copy): when the remote app has mouse
  //      reporting ON, a drag is sent to the remote instead of making a native xterm selection; but
  //      when it's OFF the user CAN select in xterm, and expects Cmd+C / a context menu to copy. We
  //      wire both to term.getSelection(). Keyboard handling is registered in mount() (after open).
  const copySelection = () => {
    const sel = (() => { try { return term.getSelection(); } catch (_) { return ''; } })();
    if (!sel) return false;
    if (ctx.copyText) ctx.copyText(sel);
    if (ctx.setStatus) ctx.setStatus('copied ' + sel.length + ' chars');
    return true;
  };
  if (term.parser && term.parser.registerOscHandler) {
    term.parser.registerOscHandler(52, (payload) => {
      const text = decodeOsc52(payload);
      if (text && ctx.copyText) { ctx.copyText(text); if (ctx.setStatus) ctx.setStatus('copied ' + text.length + ' chars'); }
      return true;   // handled — suppress xterm's default (which would do nothing anyway)
    });
  }

  let mounted = false;
  let el: HTMLDivElement | null = null;
  let rendererKind: 'dom' | 'canvas' = 'dom';
  const preOpen: string[] = [];   // bytes buffered until the xterm is opened
  const mobileDomInput = new MobileDomInputReconciler();
  let mobileBeforeInput: {
    target: HTMLTextAreaElement;
    value: string;
    selectionStart: number | null;
    selectionEnd: number | null;
    inputType: string;
  } | null = null;
  let mobileTrackedValue = '';
  let mobileComposing = false;
  let keyboardDispatch = false;
  let lastKeyboardData: { data: string; at: number } | null = null;

  // input up (gating is applied by the caller via ctx.input, which may buffer)
  term.onData((data) => {
    if (keyboardDispatch) lastKeyboardData = { data, at: performance.now() };
    ctx.input(data);
  });
  // xterm consumes a standalone BEL byte and surfaces it through onBell instead of leaving it in
  // the rendered data stream. Codex's default notification method falls back to BEL for terminals
  // it does not recognize, so forward that standard attention signal to the project/tab layer.
  if (term.onBell) term.onBell(() => { if (ctx.onBell) ctx.onBell(); });

  // Local copy shortcuts. Cmd+C (mac) / Ctrl+Shift+C (elsewhere) copy the xterm selection when
  // there IS one; otherwise fall through so the key reaches the shell (Ctrl+C = SIGINT). Attached
  // via attachCustomKeyEventHandler so it runs before onData forwards the byte to the backend.
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== 'keydown') return true;
    const isCopy = (e.key === 'c' || e.key === 'C') &&
      ((e.metaKey && !e.ctrlKey && !e.altKey) || (e.ctrlKey && e.shiftKey && !e.metaKey && !e.altKey));
    if (isCopy && term.hasSelection && term.hasSelection()) { copySelection(); return false; }
    return true;
  });

  return {
    kind: 'terminal',
    term,                      // raw handle (link provider, tests)
    get mounted() { return mounted; },

    mount(container: HTMLElement) {
      if (mounted && el) { container.appendChild(el); return; }
      el = document.createElement('div');
      el.style.width = '100%'; el.style.height = '100%';
      container.appendChild(el);
      term.open(el);
      // Canvas draws the grid with 2D canvas calls instead of per-cell DOM nodes and holds no
      // scarce WebGL context. Construction/load failure deliberately leaves xterm's DOM renderer
      // active: the pane remains correct, only slower.
      try {
        term.loadAddon(new CanvasAddon.CanvasAddon());
        rendererKind = 'canvas';
        // A freshly attached renderer has no dirty rows. Without this mandatory repaint an idle
        // terminal can remain blank until the user types or resizes it.
        this.repaintAllRows();
      } catch (_) { /* DOM renderer fallback */ }
      mounted = true;
      // Observe real DOM gestures instead of term.onData: xterm uses onData for both user input and
      // automatic terminal protocol replies, and a reply must not acknowledge unread work.
      const acknowledge = () => { if (ctx.onInteract) ctx.onInteract(); };
      if (spec.mobileTextInput) {
        // WebKit has an open bug where iOS dictation emits no composition events. Capture the real
        // before/after edit on xterm's textarea and stop the target event before xterm forwards
        // InputEvent.data as an append. This preserves WebKit's replacement semantics instead.
        el.addEventListener('beforeinput', (event) => {
          const input = event as InputEvent;
          if (mobileComposing || input.inputType === 'insertFromPaste') return;
          if (!(event.target instanceof HTMLTextAreaElement)) return;
          mobileBeforeInput = {
            target: event.target,
            value: event.target.value,
            selectionStart: event.target.selectionStart,
            selectionEnd: event.target.selectionEnd,
            inputType: input.inputType,
          };
          mobileTrackedValue = event.target.value;
        }, true);
        el.addEventListener('input', (event) => {
          const input = event as InputEvent;
          if (!(event.target instanceof HTMLTextAreaElement)) return;
          const textarea = event.target;
          const beforeState = mobileBeforeInput?.target === textarea ? mobileBeforeInput : null;
          const before = beforeState
            ? beforeState.value
            : mobileTrackedValue;
          const after = textarea.value;
          mobileTrackedValue = after;
          mobileBeforeInput = null;

          if (mobileComposing || input.inputType === 'insertFromPaste') return;
          const keyboardData = lastKeyboardData;
          const keyboardAlreadySent = keyboardData != null
            && performance.now() - keyboardData.at < 250
            && (keyboardData.data === input.data
              || (input.inputType.startsWith('delete') && keyboardData.data === '\x7f'));
          if (keyboardAlreadySent) return;

          // This input had no keyboard/keypress delivery (the signature of iOS dictation and some
          // soft keyboards). Prevent xterm's target listener from appending InputEvent.data.
          event.stopPropagation();
          const edit = mobileDomInput.reconcile(before, after);
          if (ctx.log) {
            ctx.log(`mobile input edit ${JSON.stringify({
              inputType: input.inputType,
              dataLength: codePoints(input.data ?? '').length,
              beforeLength: codePoints(before).length,
              afterLength: codePoints(after).length,
              selectionStart: beforeState?.selectionStart ?? null,
              selectionEnd: beforeState?.selectionEnd ?? null,
              prefixLength: edit.prefixLength,
              deleteCount: edit.deleteCount,
              insertCount: edit.insertCount,
            })}`);
          }
          if (edit.output) ctx.input(edit.output);
        }, true);
        el.addEventListener('keydown', () => {
          keyboardDispatch = true;
          setTimeout(() => { keyboardDispatch = false; }, 0);
        }, true);
        el.addEventListener('keyup', () => { keyboardDispatch = false; }, true);
        el.addEventListener('compositionstart', () => { mobileComposing = true; }, true);
        el.addEventListener('compositionend', (event) => {
          mobileComposing = false;
          if (event.target instanceof HTMLTextAreaElement) mobileTrackedValue = event.target.value;
        }, true);
        el.addEventListener('paste', () => { mobileBeforeInput = null; }, true);
        el.addEventListener('blur', (event) => {
          mobileBeforeInput = null;
          mobileTrackedValue = event.target instanceof HTMLTextAreaElement ? event.target.value : '';
        }, true);
      }
      el.addEventListener('pointerdown', acknowledge, true);
      el.addEventListener('keydown', acknowledge, true);
      el.addEventListener('paste', acknowledge, true);
      el.addEventListener('beforeinput', acknowledge, true);  // IME/composition input
      // Right-click: copy the selection if there is one (otherwise let the default menu through).
      el.addEventListener('contextmenu', (e) => {
        if (term.hasSelection && term.hasSelection()) { e.preventDefault(); copySelection(); }
      });
      if (preOpen.length) { const b = preOpen.splice(0); b.forEach((d) => term.write(d)); }
      this.fit();
    },
    element() { return el; },

    onData(data: string) {
      if (!mounted) { preOpen.push(data); return; }
      term.write(data, () => {
        if (ctx.ack) ctx.ack(byteLen(data));
        const started = inputLatencyStarted;
        if (started != null) {
          inputLatencyStarted = null;
          requestAnimationFrame(() => requestAnimationFrame(() => {
            inputLatencyResult = performance.now() - started;
          }));
        }
      });
    },

    fit() { try { fit.fit(); } catch (_) {} return { cols: term.cols, rows: term.rows }; },
    resize(cols: number, rows: number) { try { term.resize(cols, rows); } catch (_) {} },
    focus() { try { term.focus(); } catch (_) {} },
    // Force xterm to re-render every row of the current buffer. This does not resize the grid,
    // touch the PTY, or alter scrollback, so it is safe on reveal/focus/wake recovery paths.
    repaintAllRows() {
      try {
        term.refresh(0, Math.max(0, term.rows - 1));
        repaintCount += 1;
      } catch (_) { /* renderer not ready */ }
    },
    rendererKind() { return rendererKind; },

    readBuffer() {   // test hook
      const buf = term.buffer.active; let out = '';
      for (let i = 0; i < buf.length; i++) { const l = buf.getLine(i); if (l) out += l.translateToString(true) + '\n'; }
      return out;
    },

    dispose() { try { term.dispose(); } catch (_) {} if (el && el.parentNode) el.parentNode.removeChild(el); },
  };
}

function byteLen(s: string): number { return new TextEncoder().encode(s).length; }

// Decode an OSC 52 clipboard-SET payload into UTF-8 text, or '' if it isn't one we act on.
// payload = "<selection>;<base64>" where selection is c/p/q/s/0-7 (which clipboard). We treat all
// selections the same (write to the system clipboard). A "?" data field is a clipboard READ
// request — we refuse it (returning '') so a remote program can't exfiltrate the clipboard.
export function decodeOsc52(payload: unknown): string {
  const s = String(payload == null ? '' : payload);
  const semi = s.indexOf(';');
  const b64 = semi >= 0 ? s.slice(semi + 1) : s;
  // Refuse any read request ("?" data field), not just an exact match: a read would let a remote
  // program exfiltrate the local clipboard.
  if (!b64 || b64.trim() === '' || b64.indexOf('?') >= 0) return '';
  // Decode base64 -> BYTES, then strict UTF-8. The old decodeURIComponent(escape(..)) path threw on
  // any payload that wasn't wholly valid UTF-8 and fell back to the raw binary string, which silently
  // wrote mojibake to the system clipboard (e.g. valid-UTF-8 "café" mixed with one latin-1 byte came
  // out as "cafÃ© é"). Strict decoding means we either produce the right text or nothing.
  let bytes: Uint8Array;
  try {
    const bin = atob(b64);
    bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i) & 0xFF;
  } catch (_) { return ''; }
  if (!bytes.length) return '';
  try { return new TextDecoder('utf-8', { fatal: true }).decode(bytes); }
  catch (_) { return ''; }   // not valid UTF-8 -> refuse rather than paste garbage
}
