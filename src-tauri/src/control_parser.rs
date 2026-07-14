//! Pure parser for the tmux control-mode (`tmux -CC`) protocol (DESIGN.md §12).
//! Port of src/shared/controlModeParser.js. Feed it stream chunks via `write`; it returns
//! structured events. No IO, no state beyond line buffering + %begin/%end correlation.

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    Output { pane: String, data: String },
    Begin { cmd: String },
    Reply { cmd: String, ok: bool, body: Vec<String> },
    WindowAdd { window: String },
    WindowClose { window: String },
    WindowRenamed { window: String, name: String },
    WindowPaneChanged { window: String, pane: String },
    SessionChanged { session: String, name: String },
    SessionWindowChanged { session: String, window: String },
    LayoutChange { window: String, layout: String },
    SessionsChanged,
    Exit { reason: String },
    Unknown { line: String },
}

struct Reply {
    cmd: String,
    lines: Vec<String>,
}

pub struct ControlModeParser {
    buf: String,
    in_reply: Option<Reply>,
}

impl Default for ControlModeParser {
    fn default() -> Self { Self::new() }
}

impl ControlModeParser {
    pub fn new() -> Self {
        ControlModeParser { buf: String::new(), in_reply: None }
    }

    /// Feed a chunk of stream bytes (as UTF-8 text); returns any complete events parsed.
    pub fn write(&mut self, chunk: &str) -> Vec<ControlEvent> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.find('\n') {
            let mut line: String = self.buf[..nl].to_string();
            self.buf.drain(..nl + 1);
            if line.ends_with('\r') {
                line.pop();
            }
            self.line(&line, &mut out);
        }
        out
    }

    fn line(&mut self, raw: &str, out: &mut Vec<ControlEvent>) {
        let line = strip_markers(raw);
        if line.is_empty() {
            return;
        }

        // Inside a reply block: everything until %end/%error is verbatim body (even lines that
        // start with '%', e.g. a display-message reply "%34").
        if let Some(reply) = self.in_reply.as_mut() {
            if line == "%end" || line.starts_with("%end ") || line == "%error" || line.starts_with("%error ") {
                let ok = line.starts_with("%end");
                let r = self.in_reply.take().unwrap();
                out.push(ControlEvent::Reply { cmd: r.cmd, ok, body: r.lines });
                return;
            }
            reply.lines.push(line.to_string());
            return;
        }

        if !line.starts_with('%') {
            return;
        }

        let (kw, rest) = match line.find(' ') {
            Some(sp) => (&line[..sp], &line[sp + 1..]),
            None => (line.as_str(), ""),
        };

        match kw {
            "%output" => {
                // "%<pane> <data...>" — pane is %N, data runs to end of line (octal-escaped).
                if let Some(sp) = rest.find(' ') {
                    let pane = &rest[..sp];
                    let data = &rest[sp + 1..];
                    if is_pane_id(pane) {
                        out.push(ControlEvent::Output { pane: pane.to_string(), data: unescape_output(data) });
                    }
                }
            }
            "%begin" => {
                // "<ts> <cmd#> <flags>"
                let cmd = rest.split(' ').nth(1).unwrap_or("").to_string();
                self.in_reply = Some(Reply { cmd: cmd.clone(), lines: Vec::new() });
                out.push(ControlEvent::Begin { cmd });
            }
            "%end" | "%error" => {
                if let Some(r) = self.in_reply.take() {
                    out.push(ControlEvent::Reply { cmd: r.cmd, ok: kw == "%end", body: r.lines });
                }
            }
            "%window-add" => out.push(ControlEvent::WindowAdd { window: rest.trim().to_string() }),
            "%window-close" | "%unlinked-window-close" => {
                out.push(ControlEvent::WindowClose { window: rest.trim().to_string() })
            }
            "%window-renamed" => {
                if let Some(i) = rest.find(' ') {
                    out.push(ControlEvent::WindowRenamed {
                        window: rest[..i].to_string(),
                        name: rest[i + 1..].to_string(),
                    });
                }
            }
            "%window-pane-changed" => {
                let mut it = rest.split(' ');
                if let (Some(w), Some(p)) = (it.next(), it.next()) {
                    out.push(ControlEvent::WindowPaneChanged { window: w.to_string(), pane: p.to_string() });
                }
            }
            "%session-changed" => {
                if let Some(i) = rest.find(' ') {
                    out.push(ControlEvent::SessionChanged {
                        session: rest[..i].to_string(),
                        name: rest[i + 1..].to_string(),
                    });
                } else {
                    out.push(ControlEvent::SessionChanged { session: rest.to_string(), name: String::new() });
                }
            }
            "%session-window-changed" => {
                let mut it = rest.split(' ');
                if let (Some(s), Some(w)) = (it.next(), it.next()) {
                    out.push(ControlEvent::SessionWindowChanged { session: s.to_string(), window: w.to_string() });
                }
            }
            "%layout-change" => {
                if let Some(i) = rest.find(' ') {
                    out.push(ControlEvent::LayoutChange {
                        window: rest[..i].to_string(),
                        layout: rest[i + 1..].to_string(),
                    });
                }
            }
            "%sessions-changed" => out.push(ControlEvent::SessionsChanged),
            "%exit" => out.push(ControlEvent::Exit { reason: rest.trim().to_string() }),
            _ => out.push(ControlEvent::Unknown { line: line.to_string() }),
        }
    }
}

fn is_pane_id(s: &str) -> bool {
    s.starts_with('%') && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit())
}

// Strip leading DCS control-mode marker (ESC P 1000 p; ESC optional) and trailing ST (ESC \).
fn strip_markers(line: &str) -> String {
    let mut s = line;
    // MARKER_RE: /^\x1b?P1000p/
    if let Some(stripped) = s.strip_prefix('\x1b') {
        if let Some(rest) = stripped.strip_prefix("P1000p") {
            s = rest;
        }
    } else if let Some(rest) = s.strip_prefix("P1000p") {
        s = rest;
    }
    // ST_RE: /\x1b\\$/
    if let Some(rest) = s.strip_suffix("\x1b\\") {
        s = rest;
    }
    s.to_string()
}

/// Un-escape a tmux %output payload: octal escapes like \033 \015 \012 -> raw bytes, plus \\ -> \.
pub fn unescape_output(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            if n == '\\' {
                out.push('\\');
                i += 2;
                continue;
            }
            // 3-digit octal after the backslash?
            if i + 3 < bytes.len() + 1 && i + 4 <= bytes.len() {
                let oct: String = bytes[i + 1..i + 4].iter().collect();
                if oct.len() == 3 && oct.chars().all(|c| ('0'..='7').contains(&c)) {
                    if let Ok(code) = u32::from_str_radix(&oct, 8) {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                            i += 4;
                            continue;
                        }
                    }
                }
            }
            out.push('\\');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&str]) -> Vec<ControlEvent> {
        let mut p = ControlModeParser::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.write(c));
        }
        out
    }

    #[test]
    fn tc_cm1_unescape_octal() {
        assert_eq!(unescape_output("\\033[1m"), "\x1b[1m");
        assert_eq!(unescape_output("a\\015\\012b"), "a\r\nb");
        assert_eq!(unescape_output("plain text"), "plain text");
        assert_eq!(unescape_output("back\\\\slash"), "back\\slash");
    }

    #[test]
    fn tc_cm2_output_event() {
        let ev = collect(&["%output %0 hello\\015\\012world\r\n"]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0], ControlEvent::Output { pane: "%0".into(), data: "hello\r\nworld".into() });
    }

    #[test]
    fn tc_cm_reply_body_verbatim() {
        // a display-message reply whose body line starts with '%' must be body, not a control line
        let ev = collect(&["%begin 1 42 0\r\n", "%34\r\n", "%end 1 42 0\r\n"]);
        let reply = ev.iter().find(|e| matches!(e, ControlEvent::Reply { .. })).unwrap();
        assert_eq!(reply, &ControlEvent::Reply { cmd: "42".into(), ok: true, body: vec!["%34".into()] });
    }

    #[test]
    fn tc_cm_window_events() {
        let ev = collect(&["%window-add @1\r\n", "%window-renamed @1 zsh\r\n", "%session-window-changed $0 @1\r\n"]);
        assert!(ev.contains(&ControlEvent::WindowAdd { window: "@1".into() }));
        assert!(ev.contains(&ControlEvent::WindowRenamed { window: "@1".into(), name: "zsh".into() }));
        assert!(ev.contains(&ControlEvent::SessionWindowChanged { session: "$0".into(), window: "@1".into() }));
    }

    #[test]
    fn tc_cm_error_reply() {
        let ev = collect(&["%begin 1 5 0\r\n", "boom\r\n", "%error 1 5 0\r\n"]);
        let reply = ev.iter().find(|e| matches!(e, ControlEvent::Reply { .. })).unwrap();
        assert_eq!(reply, &ControlEvent::Reply { cmd: "5".into(), ok: false, body: vec!["boom".into()] });
    }

    #[test]
    fn tc_cm_chunk_boundaries() {
        // a line split across two writes must still parse once the newline arrives
        let mut p = ControlModeParser::new();
        assert!(p.write("%window-a").is_empty());
        assert!(p.write("dd @2").is_empty());
        let ev = p.write("\r\n");
        assert_eq!(ev, vec![ControlEvent::WindowAdd { window: "@2".into() }]);
    }
}
