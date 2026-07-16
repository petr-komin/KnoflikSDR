//! Vlákno příjmu: ALSA capture -> DSP -> audio ring + spektrum pro GUI.

use crate::dsp::{Demod, Mode};
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{anyhow, Result};
use num_complex::Complex32;
use rustfft::FftPlanner;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Cílová vzorkovací frekvence zvuku na výstupu.
pub const AUDIO_RATE: f64 = 48_000.0;
pub const FFT_SIZE: usize = 2048;
/// Výchozí šířka pásma pro AM (+-4 kHz od nosné).
pub const AM_BANDWIDTH_HZ: f64 = 8_000.0;
/// Výchozí šířka pásma pro SSB - obvyklá hodnota pro fonii.
pub const SSB_BANDWIDTH_HZ: f64 = 2_700.0;

/// Meze šířky pásma podle režimu.
///
/// Dolní mez drží FIR: přechodové pásmo je ~300 Hz, pod ním by štítek
/// přestal odpovídat skutečnému -6 dB bodu. Horní mez u AM je s rezervou
/// pod Nyquistem po decimaci (48 kHz -> 24 kHz); pro AM je 24 kHz
/// (audio do 12 kHz) stejně víc než dost. U SSB nemá smysl jít nad
/// šířku fonického kanálu.
pub fn bandwidth_range(mode: Mode) -> (f64, f64) {
    if mode.is_ssb() {
        (800.0, 4_000.0)
    } else {
        (2_000.0, 24_000.0)
    }
}

/// Co zkusit na vstupu, od nejlepšího. Vyšší vzorkovačka = širší panorama,
/// 24 bit = větší dynamický rozsah.
const CANDIDATES: &[(u32, Format)] = &[
    (192_000, Format::S243LE),
    (192_000, Format::S16LE),
    (96_000, Format::S243LE),
    (96_000, Format::S16LE),
    (48_000, Format::S243LE),
    (48_000, Format::S16LE),
];

/// Ovládací prvky, do kterých píše GUI a čte je DSP vlákno.
pub struct Controls {
    /// Odladění od středu (VFO) v Hz.
    pub offset_hz: f64,
    pub volume: f32,
    /// SoftRocky mívají I/Q prohozené - pak je spektrum zrcadlené.
    pub swap_iq: bool,
    /// Celková šířka propustného pásma demodulátoru.
    pub bandwidth_hz: f64,
    pub mode: Mode,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            offset_hz: 0.0,
            volume: 0.5,
            swap_iq: false,
            bandwidth_hz: AM_BANDWIDTH_HZ,
            mode: Mode::Am,
        }
    }
}

/// Spektrum v dB, už prohozené tak, aby střed pole = DC = VFO.
pub struct Spectrum {
    pub bins: Vec<f32>,
    pub generation: u64,
}

impl Default for Spectrum {
    fn default() -> Self {
        Spectrum {
            bins: vec![-120.0; FFT_SIZE],
            generation: 0,
        }
    }
}

pub struct Shared {
    pub controls: Mutex<Controls>,
    pub spectrum: Mutex<Spectrum>,
    /// Stav zvukového vstupu.
    pub status: Mutex<String>,
    /// Stav SoftRocku na USB. Zvlášť, ať si to dvě vlákna nepřepisují.
    pub hw_status: Mutex<String>,
    /// Skutečná vzorkovací frekvence vstupu = šířka panoramatu.
    pub sample_rate: AtomicU32,
    pub running: Arc<AtomicBool>,
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            controls: Mutex::new(Controls::default()),
            spectrum: Mutex::new(Spectrum::default()),
            status: Mutex::new("startuji...".to_string()),
            hw_status: Mutex::new("hledám SoftRock...".to_string()),
            sample_rate: AtomicU32::new(96_000),
            running: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// Dekóduje jeden vzorek z ALSA bufferu na f32 v rozsahu -1..1.
#[inline]
fn decode(fmt: Format, b: &[u8]) -> f32 {
    match fmt {
        Format::S16LE => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        Format::S243LE => {
            // 3 bajty LE; posunem nahoru a aritmetickým posunem zpět
            // se správně rozšíří znaménko.
            let v = ((b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16)) << 8;
            (v >> 8) as f32 / 8_388_608.0
        }
        _ => 0.0,
    }
}

fn bytes_per_sample(fmt: Format) -> usize {
    match fmt {
        Format::S16LE => 2,
        Format::S243LE => 3,
        _ => 0,
    }
}

/// Zjistí nejlepší kombinaci vzorkovačky a formátu, kterou karta umí.
fn negotiate(pcm: &PCM) -> Result<(u32, Format)> {
    for &(rate, fmt) in CANDIDATES {
        let hwp = HwParams::any(pcm)?;
        if hwp.set_channels(2).is_err()
            || hwp.set_access(Access::RWInterleaved).is_err()
            || hwp.set_format(fmt).is_err()
            || hwp.test_rate(rate).is_err()
        {
            continue;
        }
        return Ok((rate, fmt));
    }
    Err(anyhow!(
        "zvukovka neumí žádnou podporovanou kombinaci (zkoušel jsem 192/96/48 kHz, 24 i 16 bit)"
    ))
}

pub fn spawn(
    device: String,
    shared: Arc<Shared>,
    audio_tx: rtrb::Producer<f32>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut audio_tx = audio_tx;
        while shared.running.load(Ordering::Relaxed) {
            match run(&device, &shared, &mut audio_tx) {
                Ok(()) => break,
                Err(e) => {
                    *shared.status.lock().unwrap() = format!("chyba capture: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    })
}

fn run(device: &str, shared: &Arc<Shared>, audio_tx: &mut rtrb::Producer<f32>) -> Result<()> {
    let pcm = PCM::new(device, Direction::Capture, false)?;
    let (rate, fmt) = negotiate(&pcm)?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(2)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(fmt)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        hwp.set_period_size_near(1024, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(8192)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;

    let actual_rate = pcm.hw_params_current()?.get_rate()? as f64;
    shared
        .sample_rate
        .store(actual_rate as u32, Ordering::Relaxed);

    let bits = bytes_per_sample(fmt) * 8;
    *shared.status.lock().unwrap() = format!(
        "příjem z {device} @ {:.0} kHz, {fmt} ({} bit)",
        actual_rate / 1000.0,
        if bits == 24 { 24 } else { 16 }
    );

    // Decimace na ~48 kHz audio.
    let decim = ((actual_rate / AUDIO_RATE).round() as usize).max(1);

    let mut rx = Demod::new(actual_rate, decim, AM_BANDWIDTH_HZ, Mode::Am);
    let bps = bytes_per_sample(fmt);
    let frame_bytes = bps * 2;
    let io = pcm.io_bytes();
    let mut raw = vec![0u8; 1024 * frame_bytes];

    let mut audio: Vec<f32> = Vec::with_capacity(1024);
    let mut iq: Vec<Complex32> = Vec::with_capacity(1024);

    // FFT pro panorama
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (FFT_SIZE - 1) as f32).cos())
        .collect();
    let mut fft_buf: Vec<Complex32> = Vec::with_capacity(FFT_SIZE);
    let mut smoothed = vec![-120.0f32; FFT_SIZE];
    let mut generation = 0u64;

    while shared.running.load(Ordering::Relaxed) {
        let frames = match io.readi(&mut raw) {
            Ok(n) => n,
            Err(e) => {
                pcm.try_recover(e, true)?;
                continue;
            }
        };
        if frames == 0 {
            continue;
        }

        let (offset, volume, swap, bandwidth, mode) = {
            let c = shared.controls.lock().unwrap();
            (c.offset_hz, c.volume, c.swap_iq, c.bandwidth_hz, c.mode)
        };
        rx.set_offset(offset);
        rx.set_mode(mode);
        rx.set_bandwidth(bandwidth);

        iq.clear();
        for f in 0..frames {
            let o = f * frame_bytes;
            let a = decode(fmt, &raw[o..o + bps]);
            let b = decode(fmt, &raw[o + bps..o + frame_bytes]);
            iq.push(if swap {
                Complex32::new(b, a)
            } else {
                Complex32::new(a, b)
            });
        }

        // Panorama
        for &s in &iq {
            fft_buf.push(s);
            if fft_buf.len() == FFT_SIZE {
                let mut scratch: Vec<Complex32> =
                    fft_buf.iter().zip(&window).map(|(s, w)| s * *w).collect();
                fft.process(&mut scratch);
                let norm = 1.0 / FFT_SIZE as f32;
                let half = FFT_SIZE / 2;
                for i in 0..FFT_SIZE {
                    // fftshift: DC doprostřed
                    let src = (i + half) % FFT_SIZE;
                    let mag = (scratch[src] * norm).norm().max(1e-12);
                    let db = 20.0 * mag.log10();
                    smoothed[i] = smoothed[i] * 0.6 + db * 0.4;
                }
                fft_buf.clear();
                generation += 1;
                if let Ok(mut sp) = shared.spectrum.try_lock() {
                    sp.bins.copy_from_slice(&smoothed);
                    sp.generation = generation;
                }
            }
        }

        // Demodulace
        audio.clear();
        rx.process(&iq, &mut audio);
        for s in &audio {
            // Když ring přeteče, vzorek zahodíme - výstup si drží tempo sám.
            let _ = audio_tx.push(s * volume);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dekoduje_s16() {
        assert_eq!(decode(Format::S16LE, &[0x00, 0x00]), 0.0);
        assert_eq!(decode(Format::S16LE, &0i16.to_le_bytes()), 0.0);
        assert!((decode(Format::S16LE, &i16::MAX.to_le_bytes()) - 1.0).abs() < 1e-4);
        assert!((decode(Format::S16LE, &i16::MIN.to_le_bytes()) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn dekoduje_s24_vcetne_znamenka() {
        assert_eq!(decode(Format::S243LE, &[0, 0, 0]), 0.0);
        // +8388607 = 0x7FFFFF -> těsně pod 1.0
        assert!((decode(Format::S243LE, &[0xFF, 0xFF, 0x7F]) - 1.0).abs() < 1e-6);
        // -8388608 = 0x800000 -> přesně -1.0
        assert!((decode(Format::S243LE, &[0x00, 0x00, 0x80]) + 1.0).abs() < 1e-9);
        // -1 = 0xFFFFFF
        assert!(decode(Format::S243LE, &[0xFF, 0xFF, 0xFF]) < 0.0);
    }
}
