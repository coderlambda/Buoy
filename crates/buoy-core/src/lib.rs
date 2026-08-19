use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
}

/// Parse the renderer's `[user@]host[:port]` form without ever passing it through a shell.
/// Bracketed and bare IPv6 are accepted; a mobile connection requires an explicit user later.
pub fn parse_ssh_target(input: &str) -> Result<SshTarget, String> {
    if input.is_empty() || input.len() > 255 {
        return Err("host is empty or too long".into());
    }
    let (user, rest) = match input.find('@') {
        Some(at) => {
            let user = &input[..at];
            if !valid_user(user) {
                return Err("invalid SSH user".into());
            }
            (Some(user.to_string()), &input[at + 1..])
        }
        None => (None, input),
    };

    let (host, port, ipv6) = if let Some(stripped) = rest.strip_prefix('[') {
        let close = stripped.find(']').ok_or("unterminated IPv6 bracket")?;
        let host = &stripped[..close];
        let tail = &stripped[close + 1..];
        let port = if tail.is_empty() {
            22
        } else if let Some(value) = tail.strip_prefix(':') {
            parse_port(value)?
        } else {
            return Err("garbage after IPv6 bracket".into());
        };
        (host.to_string(), port, true)
    } else if rest.matches(':').count() >= 2 {
        (rest.to_string(), 22, true)
    } else if let Some(colon) = rest.find(':') {
        (
            rest[..colon].to_string(),
            parse_port(&rest[colon + 1..])?,
            false,
        )
    } else {
        (rest.to_string(), 22, false)
    };

    let valid_host = if ipv6 {
        !host.is_empty() && host.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
    } else {
        valid_dns_host(&host)
    };
    if !valid_host {
        return Err("invalid SSH host".into());
    }
    Ok(SshTarget { user, host, port })
}

pub fn validate_session_name(value: &str) -> Result<(), String> {
    let mut chars = value.chars();
    if value.is_empty()
        || value.len() > 64
        || !matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
    {
        return Err("invalid tmux session name".into());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("invalid tmux session name".into());
    }
    Ok(())
}

fn valid_user(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn valid_dns_host(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

fn parse_port(value: &str) -> Result<u16, String> {
    if value.is_empty() || value.len() > 5 || !value.chars().all(|c| c.is_ascii_digit()) {
        return Err("invalid SSH port".into());
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| "SSH port out of range".into())
}

/// Platform capabilities are part of the runtime contract. The frontend renders from these flags
/// instead of spreading target checks throughout product code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCapabilities {
    pub platform: &'static str,
    pub local_shell: bool,
    pub native_tabs: bool,
    pub port_forwarding: bool,
    pub background_connection: bool,
    pub file_download: bool,
    pub ssh_host_key_verification: bool,
}

impl RuntimeCapabilities {
    pub const fn desktop() -> Self {
        Self {
            platform: "desktop",
            local_shell: true,
            native_tabs: true,
            port_forwarding: true,
            background_connection: true,
            file_download: true,
            ssh_host_key_verification: true,
        }
    }

    pub const fn mobile() -> Self {
        Self {
            platform: "mobile",
            local_shell: false,
            native_tabs: true,
            port_forwarding: true,
            background_connection: false,
            file_download: true,
            ssh_host_key_verification: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_contract_is_remote_foreground_only() {
        let capabilities = RuntimeCapabilities::mobile();
        assert_eq!(capabilities.platform, "mobile");
        assert!(!capabilities.local_shell);
        assert!(!capabilities.background_connection);
        assert!(capabilities.native_tabs);
        assert!(capabilities.port_forwarding);
        assert!(capabilities.file_download);
        assert!(capabilities.ssh_host_key_verification);
    }

    #[test]
    fn parses_mobile_ssh_targets_without_shell_syntax() {
        assert_eq!(
            parse_ssh_target("alice@vpn-host:2202").unwrap(),
            SshTarget {
                user: Some("alice".into()),
                host: "vpn-host".into(),
                port: 2202
            },
        );
        assert_eq!(
            parse_ssh_target("alice@[fd00::1]:22").unwrap().host,
            "fd00::1"
        );
        assert!(parse_ssh_target("-oProxyCommand=bad").is_err());
        assert!(validate_session_name("dt-mobile_1").is_ok());
        assert!(validate_session_name("bad;command").is_err());
    }
}
