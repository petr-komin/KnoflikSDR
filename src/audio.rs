//! Audio výstup přes ALSA.
//!
//! Jde na "pulse" (tj. PipeWire), stejně jako Quisk s name_of_sound_play="pulse".
//! Blokující writei zároveň udává tempo, takže nepotřebujeme vlastní časování.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Kolik rámců zapisujeme najednou (~10 ms při 48 kHz).
const CHUNK: usize = 512;

pub fn spawn(
    audio_rx: rtrb::Consumer<f32>,
    rate: u32,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut audio_rx = audio_rx;
        while running.load(Ordering::Relaxed) {
            // "pulse" je primární cesta; "default" jako záchrana.
            let dev = if PCM::new("pulse", Direction::Playback, false).is_ok() {
                "pulse"
            } else {
                "default"
            };
            if let Err(e) = run(dev, &mut audio_rx, rate, &running) {
                eprintln!("audio výstup ({dev}): {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    })
}

fn run(
    device: &str,
    audio_rx: &mut rtrb::Consumer<f32>,
    rate: u32,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let pcm = PCM::new(device, Direction::Playback, false)?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(2)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        hwp.set_format(Format::S16LE)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_period_size_near(CHUNK as i64, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(CHUNK as i64 * 8)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;

    let io = pcm.io_i16()?;
    let mut buf = vec![0i16; CHUNK * 2];

    while running.load(Ordering::Relaxed) {
        for f in 0..CHUNK {
            // Prázdný ring = ticho. Nemá smysl čekat, výstup si drží tempo sám.
            let s = audio_rx.pop().unwrap_or(0.0);
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            buf[f * 2] = v;
            buf[f * 2 + 1] = v;
        }
        if let Err(e) = io.writei(&buf) {
            pcm.try_recover(e, true)?;
        }
    }
    Ok(())
}
