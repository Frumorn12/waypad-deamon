//! Captures a few seconds of desktop audio and reports what came out.
//!
//! Play something on the machine before running it: loopback of a silent
//! desktop still produces packets, but they are digital silence and prove far
//! less than a stream with real content in it.

#[cfg(windows)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use waypad_core::{audio::AudioStreamOptions, backend::AudioBackend};
    use waypad_windows::audio::WindowsAudioBackend;

    let backend = WindowsAudioBackend::new();
    let capability = backend.probe().await;
    println!(
        "probe: supported={} device={:?}",
        capability.supported, capability.monitor_source
    );
    println!("reason: {}", capability.reason.unwrap_or_default());
    anyhow::ensure!(capability.supported, "no default output device to capture");

    let options = AudioStreamOptions::new(Some(96), Some(20));
    let mut producer = backend.open(options).await?;
    println!("format: {:?}", producer.format());
    println!("source: {:?}", producer.source_label());

    let started = std::time::Instant::now();
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut silent = 0u64;
    while started.elapsed().as_secs() < 5 {
        match producer.next_packet().await? {
            Some(packet) => {
                packets += 1;
                bytes += packet.len() as u64;
                // A packet this small is Opus saying "this frame is silence",
                // which is what DTX produces on a quiet desktop.
                if packet.len() <= 3 {
                    silent += 1;
                }
            }
            None => break,
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    producer.shutdown().await;

    println!(
        "{packets} packets in {elapsed:.1}s ({:.0}/s, expected {:.0}/s), {bytes} bytes, {silent} near-silent",
        packets as f64 / elapsed,
        1000.0 / f64::from(options.frame_ms)
    );
    println!(
        "average {:.0} kbit/s, mean packet {} bytes",
        (bytes as f64 * 8.0 / 1000.0) / elapsed,
        bytes / packets.max(1)
    );

    anyhow::ensure!(packets > 0, "loopback produced no packets at all");
    // 20 ms frames means 50 a second. Well under that would mean the resampler
    // or the frame accumulator is losing audio, which would show up as a stream
    // that plays too fast on the phone.
    let rate = packets as f64 / elapsed;
    anyhow::ensure!(
        rate > 40.0 && rate < 60.0,
        "expected about 50 packets a second for 20 ms frames, got {rate:.0}"
    );
    println!("audio pipeline looks healthy");
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
