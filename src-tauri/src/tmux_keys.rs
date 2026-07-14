//! Pure encoding of shell input into tmux control-mode `send-keys` commands (DESIGN.md §12).
//! Port of src/main/tmuxKeys.js. No IO — data -> command strings.

/// Escape a text chunk for tmux's double-quoted `send-keys -l` argument.
pub fn escape_literal(part: &str) -> String {
    let mut s = String::new();
    for c in part.chars() {
        let code = c as u32;
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\t' => s.push_str("\\t"),
            '\x1b' => s.push_str("\\e"),
            _ if code < 0x20 => s.push_str(&format!("\\{:03o}", code)),
            _ => s.push(c),
        }
    }
    s
}

/// Encode `data` into the ordered `send-keys` command lines needed to reproduce it at `target`
/// (a window "@N" or pane "%N"). Text runs -> `-l "..."`; line breaks (\r\n, \r, \n) -> `Enter`.
pub fn encode_send_keys(data: &str, target: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = data.chars().peekable();

    let flush = |buf: &mut String, out: &mut Vec<String>| {
        if !buf.is_empty() {
            out.push(format!("send-keys -t {} -l \"{}\"", target, escape_literal(buf)));
            buf.clear();
        }
    };

    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                flush(&mut buf, &mut out);
                // consume a following '\n' so \r\n is a single Enter
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(format!("send-keys -t {} Enter", target));
            }
            '\n' => {
                flush(&mut buf, &mut out);
                out.push(format!("send-keys -t {} Enter", target));
            }
            _ => buf.push(c),
        }
    }
    flush(&mut buf, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_tk1_plain_text() {
        assert_eq!(encode_send_keys("ls", "@2"), vec!["send-keys -t @2 -l \"ls\""]);
    }

    #[test]
    fn tc_tk2_newline_is_enter() {
        assert_eq!(
            encode_send_keys("ls\n", "@2"),
            vec!["send-keys -t @2 -l \"ls\"", "send-keys -t @2 Enter"]
        );
    }

    #[test]
    fn tc_tk3_multiline() {
        assert_eq!(
            encode_send_keys("a\nb", "@0"),
            vec!["send-keys -t @0 -l \"a\"", "send-keys -t @0 Enter", "send-keys -t @0 -l \"b\""]
        );
    }

    #[test]
    fn tc_tk4_crlf_and_cr() {
        assert_eq!(
            encode_send_keys("x\r\n", "@1"),
            vec!["send-keys -t @1 -l \"x\"", "send-keys -t @1 Enter"]
        );
        assert_eq!(
            encode_send_keys("x\r", "@1"),
            vec!["send-keys -t @1 -l \"x\"", "send-keys -t @1 Enter"]
        );
    }

    #[test]
    fn tc_tk5_pane_target() {
        assert_eq!(encode_send_keys("hi", "%5"), vec!["send-keys -t %5 -l \"hi\""]);
    }

    #[test]
    fn tc_tk6_escaping() {
        assert_eq!(escape_literal("a\\b"), "a\\\\b");
        assert_eq!(escape_literal("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(escape_literal("a\tb"), "a\\tb");
        assert_eq!(escape_literal("\x1b[A"), "\\e[A");
        assert_eq!(escape_literal("\x01"), "\\001");
    }

    #[test]
    fn tc_tk7_empty() {
        assert!(encode_send_keys("", "@0").is_empty());
    }
}
