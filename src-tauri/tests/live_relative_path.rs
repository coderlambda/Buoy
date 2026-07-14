//! Live test for relative-path resolution against the pane cwd (§17), opt-in (#[ignore]).
//! Creates a tmux session, cd's into a dir containing a file, then reads the BARE filename and
//! asserts it resolves to <cwd>/<name> and the content loads. Also checks ~/ expansion and that
//! a non-file bare token errors (no tab).
//! Run: DT_LIVE_HOST=user@host DT_TMUX=/path cargo test --test live_relative_path -- --ignored --nocapture

use durable_terminal_lib::remote_file::{read_remote_file, TmuxCtx};

fn env(k: &str) -> Option<String> { std::env::var(k).ok().filter(|s| !s.is_empty()) }

#[test]
#[ignore]
fn live_relative_path_resolves_against_cwd() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };
    let tmux = env("DT_TMUX").unwrap_or_else(|| "tmux".into());
    let session = "rustrelpath";
    let socket = durable_terminal_lib::tmux_socket::socket_name("control", Some((3, 7)), session);

    let sh = |cmd: &str| { let _ = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", &host, cmd]).status(); };
    let kill = || sh(&format!("{} -L {} kill-session -t {} 2>/dev/null; true", tmux, socket, session));

    // Set up a work dir with a text file, and a session whose cwd is that dir.
    kill();
    sh(&format!(
        "mkdir -p /tmp/dt_rel_dir && printf 'RELPATH_CONTENT\\n' > /tmp/dt_rel_dir/hello.txt && \
         printf 'HOME_CONTENT\\n' > $HOME/dt_rel_home.txt && \
         {t} -L {s} new-session -d -s {n} -c /tmp/dt_rel_dir",
        t = tmux, s = socket, n = session));
    std::thread::sleep(std::time::Duration::from_millis(500));

    let ctx = TmuxCtx { tmux_path: tmux.clone(), socket: socket.clone(), session: session.into() };

    // 1) BARE filename -> resolves to the session's cwd (/tmp/dt_rel_dir/hello.txt)
    let bare = read_remote_file(&host, "hello.txt", 1_000_000, &ctx, &[]).expect("read bare relative");
    assert!(String::from_utf8_lossy(&bare.data).contains("RELPATH_CONTENT"),
        "bare filename resolved against pane cwd");

    // 2) ./hello.txt also resolves
    let dot = read_remote_file(&host, "./hello.txt", 1_000_000, &ctx, &[]).expect("read ./relative");
    assert!(String::from_utf8_lossy(&dot.data).contains("RELPATH_CONTENT"));

    // 3) ~/ expands to $HOME
    let home = read_remote_file(&host, "~/dt_rel_home.txt", 1_000_000, &ctx, &[]).expect("read ~ path");
    assert!(String::from_utf8_lossy(&home.data).contains("HOME_CONTENT"), "~/ expands to $HOME");

    // 4) absolute is unaffected by cwd
    let abs = read_remote_file(&host, "/tmp/dt_rel_dir/hello.txt", 1_000_000, &ctx, &[]).expect("read abs");
    assert!(String::from_utf8_lossy(&abs.data).contains("RELPATH_CONTENT"));

    // 5) a bare token that isn't a file in cwd -> error (renderer shows status, no tab)
    assert!(read_remote_file(&host, "nope.txt", 1000, &ctx, &[]).is_err(), "missing relative errors");

    // cleanup
    sh("rm -rf /tmp/dt_rel_dir $HOME/dt_rel_home.txt");
    kill();
    eprintln!("LIVE OK: relative path resolves against pane cwd ({} host)", host);
}
