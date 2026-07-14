//! Live integration test for remote file fetch (§16), opt-in (#[ignore]).
//! Prereqs on the remote (create before running):
//!   printf 'hello from remote\nline2\n' > /tmp/dt_view_test.txt
//!   printf '# Title\n\n- a\n- b\n' > /tmp/dt_view_test.md
//!   head -c 200000 /dev/urandom > /tmp/dt_view_big.bin
//! Run: DT_LIVE_HOST=user@host cargo test --test live_remote_file -- --ignored --nocapture

use durable_terminal_lib::remote_file::{read_remote_file, TmuxCtx};

fn env(k: &str) -> Option<String> { std::env::var(k).ok().filter(|s| !s.is_empty()) }

#[test]
#[ignore]
fn live_fetch_text_and_truncation_and_binary() {
    let host = match env("DT_LIVE_HOST") { Some(h) => h, None => { eprintln!("SKIP: set DT_LIVE_HOST"); return; } };

    // text file, full read
    let f = read_remote_file(&host, "/tmp/dt_view_test.txt", 1_000_000, &TmuxCtx::default(), &[]).expect("read text");
    let s = String::from_utf8_lossy(&f.data);
    assert!(s.contains("hello from remote") && s.contains("line2"), "text content round-trips: {:?}", s);
    assert!(!f.truncated);

    // truncation: cap below the file size
    let t = read_remote_file(&host, "/tmp/dt_view_test.txt", 5, &TmuxCtx::default(), &[]).expect("read capped");
    assert_eq!(t.data.len(), 5, "capped to 5 bytes");
    assert!(t.truncated, "must report truncated when file exceeds cap");
    assert_eq!(&t.data, b"hello");

    // markdown file
    let m = read_remote_file(&host, "/tmp/dt_view_test.md", 1_000_000, &TmuxCtx::default(), &[]).expect("read md");
    assert!(String::from_utf8_lossy(&m.data).contains("# Title"));

    // binary file: bytes survive base64 transport intact (no corruption)
    let b = read_remote_file(&host, "/tmp/dt_view_big.bin", 1_000_000, &TmuxCtx::default(), &[]).expect("read bin");
    assert_eq!(b.data.len(), 200_000, "full binary size preserved");
    assert!(!b.truncated);

    // a path that isn't a file -> error, not a panic
    assert!(read_remote_file(&host, "/tmp", 1000, &TmuxCtx::default(), &[]).is_err(), "directory rejected");
    assert!(read_remote_file(&host, "/no/such/path/xyz", 1000, &TmuxCtx::default(), &[]).is_err(), "missing rejected");

    eprintln!("LIVE OK: remote file fetch (text/md/binary/truncation/errors) against {}", host);
}
