//! Vlákno příjmu: zvukový vstup -> DSP -> audio ring + spektrum pro GUI.
//!
//! O to, odkud se vzorky berou a v jakém formátu jsou na drátě, se stará
//! [`crate::audio`]; sem už chodí hotové f32.

use crate::audio::{self, Depth};
use crate::decode::{Decoder, RttyConfig};
use crate::dsp::{Demod, Mode};
use anyhow::Result;
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
    match mode {
        Mode::Cw => (CW_MIN_BANDWIDTH_HZ, 2_000.0),
        Mode::Usb | Mode::Lsb => (400.0, 4_000.0),
        Mode::Am => (1_000.0, 24_000.0),
    }
}

/// Nejužší poctivý CW filtr. Změřeno: kanálový filtr na 48 kHz s 1023
/// koeficienty trefí -6 dB bod na hertz přesně až sem; při 100 Hz už
/// vyjde 58 místo 50 Hz.
pub const CW_MIN_BANDWIDTH_HZ: f64 = 150.0;
/// Výchozí šířka pro CW - obvyklá volba pro běžný provoz.
pub const CW_BANDWIDTH_HZ: f64 = 500.0;

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
    pub decoder: Decoder,
    pub rtty: RttyConfig,
    /// Squelch CW v dB nad šumovým dnem.
    pub cw_squelch_db: f32,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            offset_hz: 0.0,
            volume: 0.5,
            swap_iq: false,
            bandwidth_hz: AM_BANDWIDTH_HZ,
            mode: Mode::Am,
            decoder: Decoder::Off,
            rtty: RttyConfig::default(),
            cw_squelch_db: crate::decode::CW_SQUELCH_DB,
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
    /// Text z dekodéru, který si GUI průběžně vyzvedává.
    pub decoded: Mutex<String>,
    /// Skutečná vzorkovací frekvence vstupu = šířka panoramatu.
    pub sample_rate: AtomicU32,
    /// Úroveň naladěného signálu v dBFS před AGC, uložená jako bity f32.
    pub level_dbfs: AtomicU32,
    /// Odhadnuté tempo CW ve WPM (bity f32), 0 = neběží CW dekodér.
    pub cw_wpm: AtomicU32,
    pub running: Arc<AtomicBool>,
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            controls: Mutex::new(Controls::default()),
            spectrum: Mutex::new(Spectrum::default()),
            status: Mutex::new("startuji...".to_string()),
            hw_status: Mutex::new("hledám SoftRock...".to_string()),
            decoded: Mutex::new(String::new()),
            sample_rate: AtomicU32::new(96_000),
            level_dbfs: AtomicU32::new((-120.0f32).to_bits()),
            cw_wpm: AtomicU32::new(0f32.to_bits()),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn level_dbfs(&self) -> f32 {
        f32::from_bits(self.level_dbfs.load(Ordering::Relaxed))
    }

    pub fn cw_wpm(&self) -> f32 {
        f32::from_bits(self.cw_wpm.load(Ordering::Relaxed))
    }
}

/// Kolik rámců si říct od vstupu najednou.
const READ_FRAMES: usize = 1024;

pub fn spawn(
    device: String,
    depth: Depth,
    shared: Arc<Shared>,
    audio_tx: rtrb::Producer<f32>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut audio_tx = audio_tx;
        while shared.running.load(Ordering::Relaxed) {
            match run(&device, depth, &shared, &mut audio_tx) {
                Ok(()) => break,
                Err(e) => {
                    *shared.status.lock().unwrap() = format!("chyba vstupu: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    })
}

fn run(
    device: &str,
    depth: Depth,
    shared: &Arc<Shared>,
    audio_tx: &mut rtrb::Producer<f32>,
) -> Result<()> {
    let mut cap = audio::open_capture(device, depth)?;
    let neg = cap.negotiated();
    let actual_rate = neg.rate as f64;
    shared.sample_rate.store(neg.rate, Ordering::Relaxed);

    // Prázdný název znamená u cpalu výchozí zařízení systému.
    let kde = if device.is_empty() {
        "výchozího zařízení".to_string()
    } else {
        device.to_string()
    };
    *shared.status.lock().unwrap() = format!(
        "příjem z {kde} @ {:.0} kHz, {} bit ({})",
        actual_rate / 1000.0,
        neg.bits,
        audio::backend_name()
    );

    // Decimace na ~48 kHz audio.
    let decim = ((actual_rate / AUDIO_RATE).round() as usize).max(1);

    let mut rx = Demod::new(actual_rate, decim, AM_BANDWIDTH_HZ, Mode::Am);
    let mut raw = vec![0f32; READ_FRAMES * 2];

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
        // Nula znamená, že se vstup po zádrhelu zotavil a data přijdou příště.
        let frames = cap.read(&mut raw)?;
        if frames == 0 {
            continue;
        }

        let (offset, volume, swap, bandwidth, mode, decoder, rtty, squelch) = {
            let c = shared.controls.lock().unwrap();
            (
                c.offset_hz,
                c.volume,
                c.swap_iq,
                c.bandwidth_hz,
                c.mode,
                c.decoder,
                c.rtty,
                c.cw_squelch_db,
            )
        };
        rx.set_offset(offset);
        rx.set_mode(mode);
        rx.set_bandwidth(bandwidth);
        rx.set_decoder(decoder, rtty, squelch);

        iq.clear();
        for f in 0..frames {
            let (a, b) = (raw[f * 2], raw[f * 2 + 1]);
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
        shared
            .level_dbfs
            .store(rx.level_dbfs().to_bits(), Ordering::Relaxed);

        shared.cw_wpm.store(
            (rx.cw_wpm().unwrap_or(0.0) as f32).to_bits(),
            Ordering::Relaxed,
        );

        // Přečtený text předáme GUI. Kdyby si ho nikdo nebral, necháme
        // ho useknout, ať paměť neroste donekonečna.
        let text = rx.take_text();
        if !text.is_empty() {
            if let Ok(mut d) = shared.decoded.lock() {
                d.push_str(&text);
                if d.len() > 8192 {
                    let cut = d.len() - 4096;
                    *d = d[cut..].to_string();
                }
            }
        }
        for s in &audio {
            // Když ring přeteče, vzorek zahodíme - výstup si drží tempo sám.
            let _ = audio_tx.push(s * volume);
        }
    }
    Ok(())
}
