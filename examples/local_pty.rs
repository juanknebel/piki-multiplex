//! Minimal local-PTY example: spawn `sh`, send a command, print the screen.
//!
//! Run with: `cargo run --example local_pty`

use std::time::Duration;

use piki_multiplex::pty::{PtyOutputSignal, PtySession, SpawnOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let signal = PtyOutputSignal::new();
    let mut session = PtySession::spawn_with(
        SpawnOptions::new(&std::env::current_dir()?, "sh").with_output_signal(signal.clone()),
    )
    .await?;

    session.write(b"echo hello-from-piki\r")?;

    // Wait for output, then poll the emulated screen until the echo lands.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::time::timeout_at(deadline, signal.notified())
            .await
            .is_err()
        {
            anyhow::bail!("timed out waiting for PTY output");
        }
        signal.take();
        let screen = session.parser().lock().screen().contents();
        if screen.contains("hello-from-piki") {
            println!("{screen}");
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the echo; screen was:\n{screen}");
        }
    }

    session.kill()?;
    Ok(())
}
