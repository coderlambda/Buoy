'use strict';
// Version-tagged tmux socket names (DESIGN.md §12). The tmux server/control protocol changes
// across MINOR releases, so a 3.5 server and a 3.7 client on the SAME socket silently fail (the
// "connected but no output" bug after a tmux upgrade). Tagging each socket by MAJOR-MINOR keeps
// incompatible versions from ever sharing a server. `control` mode and `plain` mode also use
// distinct socket prefixes so a session's two views never collide.
//
// This is the single source of the naming rule — main, sshTmuxBackend, and controlModeBackend
// all derive their socket here so the invariant can't drift between them.

// Returns e.g. 'dtcc3-7' (control) or 'dtapp3-7' (plain). `tmuxVersion` is [major, minor] from the
// probe; when unknown the tag is empty (best-effort, matches the pre-versioning behavior).
function socketName(mode, tmuxVersion) {
  const v = Array.isArray(tmuxVersion) ? `${tmuxVersion[0]}-${tmuxVersion[1]}` : '';
  return (mode === 'control' ? 'dtcc' : 'dtapp') + v;
}

module.exports = { socketName };
