use std::time::{Duration, Instant};

use piki_multiplex::pty::PtySession;

#[tokio::test]
async fn write_does_not_block_on_large_input() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = PtySession::spawn(
        dir.path(),
        24,
        80,
        "cat",
        &[],
        &[],
        &[],
        false,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let big = vec![b'x'; 1_000_000];
    let start = Instant::now();
    session.write(&big).unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "write blocked for {:?}",
        start.elapsed()
    );

    let _ = session.kill();
}
