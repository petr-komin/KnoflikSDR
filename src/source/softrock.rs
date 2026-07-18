//! SoftRock: I/Q ze zvukovky, ladění Si570 po USB.
//!
//! Dvě nezávislá zařízení, takže se otevírají zvlášť a ani o sobě nevědí.

use super::{Source, Tuner};
use crate::audio::{self, Capture};
use crate::settings::Settings;
use crate::si570::Si570;
use anyhow::Result;
use num_complex::Complex32;

pub fn open(set: &Settings) -> Result<(Box<dyn Source>, Box<dyn Tuner>)> {
    let cap = audio::open_capture(&set.capture_device, set.depth)?;
    let src = SoftRockSource {
        device: set.capture_device.clone(),
        cap,
        buf: Vec::new(),
    };
    // Si570 se otevírá zvlášť: bez rádia na USB má pořád smysl vidět
    // panorama ze zvukovky, jen se nedá ladit.
    let tuner = SoftRockTuner::open(set.si570_xtal_hz, set.si570_i2c_addr);
    Ok((Box::new(src), Box::new(tuner)))
}

struct SoftRockSource {
    device: String,
    cap: Box<dyn Capture>,
    /// Prokládané f32 ze zvukovky, než se z nich složí Complex32.
    buf: Vec<f32>,
}

impl Source for SoftRockSource {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        let need = out.len() * 2;
        if self.buf.len() < need {
            self.buf.resize(need, 0.0);
        }
        let frames = self.cap.read(&mut self.buf[..need])?;
        for (i, o) in out.iter_mut().take(frames).enumerate() {
            *o = Complex32::new(self.buf[i * 2], self.buf[i * 2 + 1]);
        }
        Ok(frames)
    }

    fn rate(&self) -> f64 {
        self.cap.negotiated().rate as f64
    }

    fn label(&self) -> String {
        let neg = self.cap.negotiated();
        // Prázdný název znamená u cpalu výchozí zařízení systému.
        let kde = if self.device.is_empty() {
            "výchozího zařízení"
        } else {
            &self.device
        };
        format!(
            "příjem z {kde} @ {:.0} kHz, {} bit ({})",
            neg.rate as f64 / 1000.0,
            neg.bits,
            audio::backend_name()
        )
    }
}

struct SoftRockTuner {
    /// `None` = Si570 se nenašel; ladění pak jen hlásí, proč.
    si: Option<Si570>,
    label: String,
}

impl SoftRockTuner {
    fn open(xtal_hz: f64, i2c_addr: u16) -> Self {
        match Si570::open(xtal_hz, i2c_addr) {
            Ok(mut s) => {
                let ver = s.version().unwrap_or_else(|_| "?".into());
                SoftRockTuner {
                    si: Some(s),
                    label: format!("SoftRock fw {ver}"),
                }
            }
            Err(e) => SoftRockTuner {
                si: None,
                label: format!("{e}"),
            },
        }
    }
}

impl Tuner for SoftRockTuner {
    fn set_center(&mut self, hz: f64) -> Result<()> {
        match &mut self.si {
            Some(si) => si.set_freq(hz),
            None => Ok(()),
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}
