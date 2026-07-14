//! Version-tagged tmux socket names (DESIGN.md §12). Port of src/shared/tmuxSocket.js.
//! Tag by MAJOR-MINOR so a 3.5 server and a 3.7 client never share a socket (the "connected but
//! no output" bug after a tmux upgrade). Distinct prefixes for control vs plain mode.

/// `mode` is "control" or "plain"; `version` is (major, minor) if known.
pub fn socket_name(mode: &str, version: Option<(u32, u32)>) -> String {
    let tag = match version {
        Some((maj, min)) => format!("{}-{}", maj, min),
        None => String::new(),
    };
    let prefix = if mode == "control" { "dtcc" } else { "dtapp" };
    format!("{}{}", prefix, tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_ts1_prefixes() {
        assert_eq!(socket_name("control", Some((3, 7))), "dtcc3-7");
        assert_eq!(socket_name("plain", Some((3, 7))), "dtapp3-7");
    }

    #[test]
    fn tc_ts2_minor_bump_changes_socket() {
        assert_ne!(socket_name("control", Some((3, 5))), socket_name("control", Some((3, 7))));
    }

    #[test]
    fn tc_ts3_unknown_version() {
        assert_eq!(socket_name("control", None), "dtcc");
        assert_eq!(socket_name("plain", None), "dtapp");
    }
}
