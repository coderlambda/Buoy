
import type { LinkContext, LinkModifiers, LinkPlugin } from './plugins.js';
import type {
  OscNotificationParser,
  TuiActivityOptions,
  TuiActivityTracker,
} from './types.js';

// Built-in link plugins: URL and filesystem path. They use the SAME registry API a
// third-party plugin would (see plugins.ts), so they are examples as much as
// features. Register them, then register your own for tickets/PRs/etc.

// URL: http(s)/ftp/file, bare www., AND bare loopback host:port (localhost:3000, 127.0.0.1:8080
// — with optional scheme + path). The bare-loopback form makes dev-server output clickable so it
// can be port-forwarded (§18). Trailing punctuation trimmed by the char classes.
export const URL_RE = /\b(?:https?|ftp|file):\/\/[^\s"'<>`)]+|(?:\bwww\.[^\s"'<>`)]+)|(?:\b(?:localhost|127\.0\.0\.1):\d{1,5}(?:\/[^\s"'<>`)]*)?)/g;

// Path matcher. Four kinds, tried left-to-right in one alternation:
//   1. slash paths: absolute (/a/b), home (~/a/b), or slash-containing relative (./x, ../x, src/a)
//   2. bare filename WITH an extension: README.md, notes.txt, pic.png (>=1 non-space, a dot, then
//      1-8 ext chars) — this makes `ls` output clickable (§17). Relatives are resolved against the
//      pane cwd in the backend.
//   3. known extension-less names: Makefile, Dockerfile, LICENSE, README (word-bounded).
// A plain word with no slash, no extension, and not on the known list stays unmatched (avoids
// underlining every token). Trailing shell punctuation is excluded from the char classes.
export const KNOWN_NAMES = ['Makefile', 'Dockerfile', 'LICENSE', 'README', 'Gemfile', 'Rakefile'] as const;
// 1. leading-anchored slash paths: absolute, home, or ./ ../ relative. The char class excludes
//    ')' (and '(') so a path wrapped in a tool call — e.g. Update(/a/b/x.md) — doesn't swallow the
//    trailing paren (matches URL_RE's exclusions). Paths literally containing parens are rare and
//    not worth mis-capturing every wrapped path for.
const SLASH_PATH = String.raw`(?:~\/|\.{1,2}\/|\/)[^\s"'<>\`:()]+`;
// 2. relative path with an interior slash and a filename WITH an extension: src/main.rs, a/b/c.txt.
//    (requires an extension on the last segment so bare dir words like "src/foo" without a dot
//    still match here only when the final segment has an ext — keeps noise down.)
const REL_SLASH = String.raw`[\w.\-]+(?:\/[\w.\-]+)*\/[\w.\-]*\w\.[A-Za-z0-9]{1,8}`;
// 3. bare filename WITH an extension: README.md, notes.txt.
const BARE_WITH_EXT = String.raw`[\w.\-]*\w\.[A-Za-z0-9]{1,8}`;
// 4. known extension-less names.
const KNOWN_RE = String.raw`\b(?:${KNOWN_NAMES.join('|')})\b`;
export const PATH_RE = new RegExp(`${SLASH_PATH}|${REL_SLASH}|${BARE_WITH_EXT}|${KNOWN_RE}`, 'g');

// Build the two default plugins. Handlers are thin — they call into ctx, which the host
// (renderer) supplies with openExternal / copyText / setStatus / meta.
// Smart URL open, shared by the regex 'url' link plugin AND the OSC 8 hyperlink handler (§21) so
// both behave identically. mods = { shift, meta, alt } (§18): plain = smart (loopback -> ssh -L
// tunnel + local browser; else default browser); Shift(+Cmd) = chooser (where to open). Terminal
// text is untrusted, so only safe schemes are opened.
export function openUrlSmart(text: string, ctx: LinkContext, mods?: LinkModifiers): void {
  if (mods && mods.shift && ctx.chooseOpen) { ctx.chooseOpen(text); return; }
  if (ctx.isLoopback && ctx.isLoopback(text) && ctx.openForwardedUrl) { ctx.openForwardedUrl(text); return; }
  let url = text;
  if (/^www\./i.test(url)) url = 'https://' + url;
  if (!/^(https?|ftp|file):\/\//i.test(url)) { ctx.setStatus('refused to open: ' + text); return; }
  ctx.openExternal(url);
  ctx.setStatus('opened ' + url);
}

// §21: parse a file:// URI into an absolute remote path, or null if it isn't a file URI. Handles
// `file:///abs`, `file://host/abs` (host ignored — CC uses an empty host), percent-encoding, and a
// trailing `:line[:col]` suffix some tools append (stripped so the path resolves). Returns null for
// anything that isn't file://, so callers fall through to URL handling.
export function parseFileUri(uri: unknown): string | null {
  const m = /^file:\/\/[^/]*(\/[^\s]*)$/i.exec(String(uri).trim());
  if (!m) return null;
  let path = m[1];
  if (!path) return null;
  try { path = decodeURIComponent(path); } catch (_) { /* keep raw on bad encoding */ }
  // strip a trailing :line or :line:col (but NOT a lone drive-like ":" mid-path)
  path = path.replace(/:\d+(?::\d+)?$/, '');
  return path;
}

// §21: extract OSC 8 file:// hyperlinks from a raw output chunk as [{shown, path}] pairs (path is
// the absolute path from the file:// URI). Format: ESC ] 8 ; params ; uri (ST|BEL) text ESC ] 8 ; ; (ST|BEL).
// Non-file links and malformed sequences are skipped. Pure/testable; the renderer builds a lookup
// map from these so a clicked relative filename resolves to the agent's authoritative absolute path.
const OSC8_LINK_RE = /\x1b\]8;[^;]*;([^\x07\x1b]*)(?:\x07|\x1b\\)([^\x1b]*)\x1b\]8;;(?:\x07|\x1b\\)/g;
export interface Osc8FileLink { shown: string; path: string }

export function extractOsc8FileLinks(data: string): Osc8FileLink[] {
  const out: Osc8FileLink[] = [];
  if (!data || data.indexOf('\x1b]8;') === -1) return out;
  OSC8_LINK_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = OSC8_LINK_RE.exec(data)) !== null) {
    const shown = (m[2] || '').trim();
    const path = parseFileUri(m[1] ?? '');
    if (shown && path) out.push({ shown, path });
  }
  return out;
}

// tmux `capture-pane -e` preserves OSC 8 hyperlinks. Replaying those snapshots into xterm marks
// every captured link cell with xterm's persistent dashed/dotted underline, even though Buoy's
// regex link provider can already rediscover and activate the visible path. Strip only the OSC 8
// wrappers from reconnect snapshots after extractOsc8FileLinks has harvested their targets. Live
// output must keep OSC 8 intact, and all other SGR styling in the snapshot remains untouched.
const OSC8_SEQUENCE_RE = /\x1b\]8;[^\x07\x1b]*(?:\x07|\x1b\\)/g;

export function stripOsc8Sequences(data: string): string {
  if (!data || data.indexOf('\x1b]8;') === -1) return data;
  return data.replace(OSC8_SEQUENCE_RE, '');
}

// Notification OSCs are a STREAM protocol: the ESC introducer, payload, and terminator can arrive
// in separate PTY chunks. Keep the small amount of unfinished protocol state here instead of
// regexing each renderer chunk independently (which silently misses split notifications).
//
// Supported protocols:
//   OSC 9   — iTerm2 legacy:       ESC ] 9 ; message BEL/ST
//   OSC 777 — rxvt/simple:         ESC ] 777 ; notify ; title ; body BEL/ST
//   OSC 99  — Kitty notification:  ESC ] 99 ; metadata ; payload ST
//
// `write()` returns how many COMPLETE, user-visible notification requests ended in this chunk.
// The renderer only needs the count (the product intentionally shows a dot, not notification text).
export function createOscNotificationParser(): OscNotificationParser {
  const MAX_PENDING = 16 * 1024;   // protocol payloads are small; bound hostile/incomplete OSCs
  let pending = '';

  function nextOscStart(s: string, from: number): number {
    const esc = s.indexOf('\x1b]', from);
    const c1 = s.indexOf('\x9d', from);   // 8-bit OSC introducer
    if (esc < 0) return c1;
    if (c1 < 0) return esc;
    return Math.min(esc, c1);
  }

  function terminator(s: string, from: number): { at: number; len: number } | null {
    const bel = s.indexOf('\x07', from);
    const st7 = s.indexOf('\x1b\\', from);
    const st8 = s.indexOf('\x9c', from);  // 8-bit ST
    let at = -1; let len = 0;
    for (const [i, n] of [[bel, 1], [st7, 2], [st8, 1]] as const) {
      if (i >= 0 && (at < 0 || i < at)) { at = i; len = n; }
    }
    return at < 0 ? null : { at, len };
  }

  return {
    write(data: unknown): number {
      if (data == null || data === '') return 0;
      pending += String(data);
      let notifications = 0;

      while (pending) {
        const start = nextOscStart(pending, 0);
        if (start < 0) {
          // Preserve a trailing ESC because the next chunk may begin with `]`.
          pending = pending.endsWith('\x1b') ? '\x1b' : '';
          break;
        }
        if (start > 0) pending = pending.slice(start);

        const introLen = pending.startsWith('\x1b]') ? 2 : 1;
        const end = terminator(pending, introLen);
        const nested = nextOscStart(pending, introLen);
        // ESC is not legal inside an OSC payload. If another OSC begins before this one terminates,
        // discard the malformed prefix and recover at the newer sequence.
        if (nested >= 0 && (!end || nested < end.at)) {
          pending = pending.slice(nested);
          continue;
        }
        if (!end) {
          if (pending.length > MAX_PENDING) {
            // Drop an unterminated/hostile sequence without retaining its payload forever.
            pending = pending.endsWith('\x1b') ? '\x1b' : '';
          }
          break;
        }

        const payload = pending.slice(introLen, end.at);
        if (isOscNotification(payload)) notifications++;
        pending = pending.slice(end.at + end.len);
      }
      return notifications;
    },
    reset() { pending = ''; },
  };
}

// Is one COMPLETE OSC payload a request to present a notification? OSC 99 also carries protocol
// replies/control messages (close, alive, capability query, buttons/icon chunks); those must not
// light an unread dot. Multipart notifications light it only on the final `d=1` chunk (`d` defaults
// to 1), preventing title/body fragments from looking like multiple new notifications.
export function isOscNotification(payload: string): boolean {
  const semi = payload.indexOf(';');
  if (semi < 0) return false;
  const code = payload.slice(0, semi);
  const rest = payload.slice(semi + 1);

  if (code === '9') return rest.length > 0;
  if (code === '777') return rest === 'notify' || rest.startsWith('notify;');
  if (code !== '99') return false;

  const payloadSep = rest.indexOf(';');
  if (payloadSep < 0) return false;   // OSC 99 requires both semicolons
  const metadata = rest.slice(0, payloadSep);
  const fields: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const part of metadata.split(':')) {
    const eq = part.indexOf('=');
    if (eq > 0) fields[part.slice(0, eq)] = part.slice(eq + 1);
  }
  if (fields.d === '0') return false;
  const part = fields.p || 'title';
  return part === 'title' || part === 'body';
}

// A TUI proves it is repainting previous rows through explicit frame markers, two-argument cursor
// positioning/margins, or vertical movement over at least two rows. Deliberately do NOT match CHA
// (`CSI n G`): Claude emits it heavily, but so do ordinary prompts and carriage-return progress
// bars. `CSI 1 A` is excluded for the same reason—single-line readline redraws are normal shell use.
const TUI_SEQ_SOURCE = String.raw`\x1b\[(?:\?2026h|\d+;\d+[Hr]|(?:[2-9]|\d{2,})A)`;
const TUI_SEQ = new RegExp(TUI_SEQ_SOURCE);
const TUI_SEQ_GLOBAL = new RegExp(TUI_SEQ_SOURCE, 'g');
const TUI_CARRY_CHARS = 16;

export function containsTuiRepaint(data: string): boolean {
  return TUI_SEQ.test(data);
}

export function createTuiActivityTracker(options?: TuiActivityOptions): TuiActivityTracker {
  const { decayMs = 10_000, now: clock = Date.now } = options ?? {};
  const decay = Math.max(0, decayMs);
  let carry = '';
  let activeUntil = Number.NEGATIVE_INFINITY;

  return {
    write(data: string, now = clock()): boolean {
      if (data) {
        const combined = carry + data;
        // Rescan the overlap so split CSI sequences are detected, but ignore a match wholly inside
        // the old carry: re-counting it on every small chunk would incorrectly extend the decay.
        TUI_SEQ_GLOBAL.lastIndex = 0;
        let match: RegExpExecArray | null;
        while ((match = TUI_SEQ_GLOBAL.exec(combined)) !== null) {
          if (match.index + match[0].length > carry.length) {
            activeUntil = now + decay;
            break;
          }
        }
        carry = combined.slice(-TUI_CARRY_CHARS);
      }
      return now < activeUntil;
    },
    active(now = clock()): boolean { return now < activeUntil; },
    reset(): void {
      carry = '';
      activeUntil = Number.NEGATIVE_INFINITY;
    },
  };
}

export function builtinLinkPlugins(): LinkPlugin[] {
  return [
    {
      name: 'url',
      priority: 10,   // URLs win over paths when they'd overlap (e.g. file:///a/b)
      regex: URL_RE,
      onClick(text: string, ctx: LinkContext, mods?: LinkModifiers) { openUrlSmart(text, ctx, mods); },
    },
    {
      name: 'path',
      priority: 0,
      regex: PATH_RE,
      onClick(text: string, ctx: LinkContext) {
        // Open the path in an in-app file-viewer tab (§16): the host fetches the file's bytes
        // (remote over ssh, or local) and previews text/markdown/image with a Download button.
        // Falls back to copy+status if the host can't provide a viewer (older/plain builds).
        if (ctx.openViewer) { ctx.openViewer(text); return; }
        ctx.copyText?.(text);
        const where = ctx.meta && ctx.meta.host ? `remote (${ctx.meta.host})` : 'local';
        ctx.setStatus(`copied ${where} path: ${text}`);
      },
    },
  ];
}
