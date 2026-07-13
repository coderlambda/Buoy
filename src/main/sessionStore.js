'use strict';
// Disk-backed session list (DESIGN.md §5.2). Persisted file is UNTRUSTED input:
// re-validate host/session on load, drop invalid entries rather than using them.
const fs = require('fs');
const path = require('path');
const { validateSession, parseHost } = require('../shared/validation');

class SessionStore {
  constructor(filePath) {
    this.filePath = filePath;
  }

  load() {
    let raw;
    try {
      raw = fs.readFileSync(this.filePath, 'utf8');
    } catch (_) {
      return []; // missing file => empty list (TC-P3)
    }
    let parsed;
    try {
      parsed = JSON.parse(raw);
    } catch (_) {
      return []; // corrupt file => empty list, no throw (TC-P3)
    }
    if (!Array.isArray(parsed)) return [];
    const out = [];
    for (const e of parsed) {
      if (!e || typeof e !== 'object') continue;
      try {
        validateSession(e.session);   // re-validate untrusted input (TC-P2)
        parseHost(e.host);
      } catch (_) {
        continue; // drop invalid entry
      }
      // tmuxPath is untrusted-on-load: only keep it if it matches the safe path charset
      // (same as buildSshArgs validates); otherwise drop it and re-probe on next connect.
      const tmuxPath = (typeof e.tmuxPath === 'string' && /^[A-Za-z0-9._/-]+$/.test(e.tmuxPath))
        ? e.tmuxPath : null;
      const tmuxVersion = (Array.isArray(e.tmuxVersion) && e.tmuxVersion.length === 2
        && e.tmuxVersion.every((n) => Number.isInteger(n))) ? e.tmuxVersion : null;
      out.push({
        id: String(e.id),
        host: e.host,
        session: e.session,
        transport: ['ssh','mosh','et'].includes(e.transport) ? e.transport : 'ssh',   // default/whitelist
        mode: e.mode === 'control' ? 'control' : 'plain',
        tmuxPath,
        tmuxVersion,
        title: typeof e.title === 'string' ? e.title : e.session,
        order: Number.isFinite(e.order) ? e.order : out.length,
      });
    }
    out.sort((a, b) => a.order - b.order);
    return out;
  }

  save(sessions) {
    const dir = path.dirname(this.filePath);
    fs.mkdirSync(dir, { recursive: true });
    const clean = sessions.map((s, i) => ({
      id: String(s.id), host: s.host, session: s.session,
      transport: ['ssh','mosh','et'].includes(s.transport) ? s.transport : 'ssh',
      mode: s.mode === 'control' ? 'control' : 'plain',
      tmuxPath: (typeof s.tmuxPath === 'string' && /^[A-Za-z0-9._/-]+$/.test(s.tmuxPath)) ? s.tmuxPath : null,
      tmuxVersion: (Array.isArray(s.tmuxVersion) && s.tmuxVersion.length === 2 && s.tmuxVersion.every((n) => Number.isInteger(n))) ? s.tmuxVersion : null,
      title: s.title || s.session, order: Number.isFinite(s.order) ? s.order : i,
    }));
    // atomic-ish write
    const tmp = this.filePath + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(clean, null, 2));
    fs.renameSync(tmp, this.filePath);
  }
}

module.exports = { SessionStore };
