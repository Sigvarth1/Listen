//! macOS: checks the visualizer's process tap. Plays a quiet 440 Hz tone with mpv for 4 s, taps it
//! like Listen does and prints how many samples arrived and their level (silence means the
//! "System Audio Recording" permission is missing).
//!
//!     cargo run -p listen-visualizer --example macos_tap_check

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    use std::time::{Duration, Instant};
    let mut mpv = std::process::Command::new("mpv")
        .args([
            "--no-config",
            "--no-video",
            "--volume=10",
            "--media-controls=no",
            "av://lavfi:sine=frequency=440:duration=4",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("mpv must be installed (brew install mpv)");
    listen_core::platform::track_child(Some(mpv.id()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let capture = tokio::spawn(listen_visualizer::capture::pipewire_many(&["listen-visualizer"], tx));
    let (mut count, mut sum) = (0usize, 0f64);
    let end = Instant::now() + Duration::from_secs(3);
    while let Ok(Some((_, samples))) = tokio::time::timeout_at(end.into(), rx.recv()).await {
        count += samples.len();
        sum += samples.iter().map(|s: &f32| f64::from(*s) * f64::from(*s)).sum::<f64>();
    }
    capture.abort();
    if let Ok(Ok(Err(e))) = tokio::time::timeout(Duration::from_millis(100), capture).await {
        println!("capture failed: {e}");
    }
    let _ = mpv.kill();
    let rms = if count > 0 { (sum / count as f64).sqrt() } else { 0.0 };
    println!("samples: {count} (~{:.1} s at 48 kHz stereo), rms: {rms:.4}", count as f64 / 96_000.0);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}
