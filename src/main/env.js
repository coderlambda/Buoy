'use strict';
// GUI apps (Electron launched from Finder/dock) inherit a minimal PATH that usually lacks
// Homebrew (/opt/homebrew/bin on Apple Silicon, /usr/local/bin on Intel) and other common
// install dirs — so `mosh`/`et`/`tmux` appear "not found" even when installed. Augment PATH
// with the usual locations so backends and the preflight check can find them.
const path = require('path');

const EXTRA_PATHS = [
  '/opt/homebrew/bin',   // Apple Silicon Homebrew
  '/usr/local/bin',      // Intel Homebrew / common
  '/opt/local/bin',      // MacPorts
  path.join(process.env.HOME || '', '.local', 'bin'),
];

function augmentedPath() {
  const cur = (process.env.PATH || '').split(':').filter(Boolean);
  const merged = [...cur];
  for (const p of EXTRA_PATHS) {
    if (p && !merged.includes(p)) merged.push(p);
  }
  return merged.join(':');
}

// A process env with the augmented PATH — pass this to node-pty / execFile.
function spawnEnv(base = process.env) {
  return { ...base, PATH: augmentedPath() };
}

module.exports = { augmentedPath, spawnEnv, EXTRA_PATHS };
