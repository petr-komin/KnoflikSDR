//! Ladění Si570 v SoftRocku přes USB (protokol DG8SAQ).
//!
//! # Původ kódu a licence
//!
//! Funkce [`registers()`] je port ze souboru `softrock/hardware_usb.py`
//! projektu Quisk — Copyright (C) 2006-2025 James C. Ahlstrom, GPL.
//! Vlastní výpočet HSDIV/N1/RFREQ napsal Ethan Blanton, KB8OJH.
//!
//! Kvůli tomuhle portu je celý KnoflikSDR pod GPL-3; Quisk to umožňuje
//! díky doložce "version 2 or (at your option) any later version".
//! Viz LICENSE v kořeni projektu.

use anyhow::{anyhow, Result};
use rusb::{Direction, Recipient, RequestType};
use std::time::Duration;

pub const VENDOR_ID: u16 = 0x16c0;
pub const PRODUCT_ID: u16 = 0x05dc;

/// DCO v Si570 musí zůstat v tomhle rozsahu.
const SI570_MIN_DCO: f64 = 4.85e9;
const SI570_MAX_DCO: f64 = 5.67e9;
/// Si570 má 6 platných HSDIV hodnot. Hledáme od nejvyšší.
const SI570_HSDIV_VALUES: [u32; 6] = [11, 9, 7, 6, 5, 4];

/// Zápis registrů r7..r12 přímo do Si570.
const REQ_SET_REGS: u8 = 0x30;
/// Čtení aktuální frekvence (k diagnostice - ověření, co Si570 opravdu drží).
#[allow(dead_code)]
const REQ_GET_FREQ: u8 = 0x3a;
/// Čtení verze firmware.
const REQ_VERSION: u8 = 0x00;

const TIMEOUT: Duration = Duration::from_millis(500);

fn req_out() -> u8 {
    rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device)
}
fn req_in() -> u8 {
    rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device)
}

pub struct Si570 {
    handle: rusb::DeviceHandle<rusb::GlobalContext>,
    xtal_freq: f64,
    i2c_addr: u16,
}

impl Si570 {
    pub fn open(xtal_freq: f64, i2c_addr: u16) -> Result<Self> {
        let handle = rusb::open_device_with_vid_pid(VENDOR_ID, PRODUCT_ID).ok_or_else(|| {
            anyhow!(
                "SoftRock USB nenalezen (VID 0x{:04x} PID 0x{:04x}). Běží ještě Quisk?",
                VENDOR_ID,
                PRODUCT_ID
            )
        })?;
        // set_configuration() u některých kusů selhává (Peaberry) - ignorujeme.
        let _ = handle.set_active_configuration(1);
        let mut si = Si570 {
            handle,
            xtal_freq,
            i2c_addr,
        };
        // Ověř, že zařízení odpovídá.
        si.version()?;
        Ok(si)
    }

    /// Verze firmware jako "major.minor".
    pub fn version(&mut self) -> Result<String> {
        let mut buf = [0u8; 2];
        let n = self
            .handle
            .read_control(req_in(), REQ_VERSION, 0x0e00, 0, &mut buf, TIMEOUT)?;
        if n == 2 {
            Ok(format!("{}.{}", buf[1], buf[0]))
        } else {
            Ok("neznámá".to_string())
        }
    }

    /// Aktuálně nastavená RF frekvence v Hz (Si570 / 4). K diagnostice.
    #[allow(dead_code)]
    pub fn freq(&mut self) -> Result<f64> {
        let mut buf = [0u8; 4];
        self.handle
            .read_control(req_in(), REQ_GET_FREQ, 0, 0, &mut buf, TIMEOUT)?;
        let raw = u32::from_le_bytes(buf) as f64;
        Ok(raw * 1.0e6 / 2097152.0 / 4.0)
    }

    /// Nastaví RF frekvenci v Hz. Si570 se ladí na 4x RF kvůli
    /// kvadraturní děličce /4 v SoftRocku.
    pub fn set_freq(&mut self, rf_hz: f64) -> Result<()> {
        let buf = registers(rf_hz, self.xtal_freq)?;
        self.handle.write_control(
            req_out(),
            REQ_SET_REGS,
            self.i2c_addr + 0x700,
            0,
            &buf,
            TIMEOUT,
        )?;
        Ok(())
    }
}

/// Spočítá obsah registrů r7..r12 Si570 pro danou RF frekvenci.
/// Si570 běží na 4x RF kvůli kvadraturní děličce /4 v SoftRocku.
pub fn registers(rf_hz: f64, xtal_freq: f64) -> Result<[u8; 6]> {
    if rf_hz <= 0.0 {
        return Err(anyhow!("neplatná frekvence {rf_hz}"));
    }
    let f = (rf_hz * 4.0).trunc();

    // Najdi nejnižší DCO, které danou frekvenci vyrobí.
    let mut best: Option<(f64, u32, u32)> = None; // (dco, hsdiv, n1)
    for &hsdiv in &SI570_HSDIV_VALUES {
        let mut n1 = (SI570_MIN_DCO / (f * hsdiv as f64)).ceil() as i64;
        n1 = if n1 < 1 { 1 } else { ((n1 + 1) / 2) * 2 };
        if n1 > 128 {
            continue;
        }
        let dco = f * hsdiv as f64 * n1 as f64;
        if dco < SI570_MIN_DCO || dco > SI570_MAX_DCO {
            continue;
        }
        if best.map_or(true, |(d, _, _)| dco < d) {
            best = Some((dco, hsdiv, n1 as u32));
        }
    }
    let (dco, hsdiv, n1) =
        best.ok_or_else(|| anyhow!("frekvence {:.0} Hz je mimo rozsah Si570", rf_hz))?;

    let rfreq = dco / xtal_freq;
    let rfreq_int = rfreq.trunc() as u64;
    let rfreq_frac = ((rfreq - rfreq_int as f64) * (1u64 << 28) as f64).round() as u64;

    // n1 se posílá jako n1-1, hsdiv jako hsdiv-4.
    let hs = (hsdiv - 4) as u64;
    let n = (n1 - 1) as u64;

    let mut buf = [0u8; 6];
    buf[0] = ((hs << 5) | (n >> 2)) as u8;
    buf[1] = (((n & 0x3) << 6) | (rfreq_int >> 4)) as u8;
    let tail = (((rfreq_int & 0xf) << 28) | rfreq_frac) as u32;
    buf[2..6].copy_from_slice(&tail.to_be_bytes());
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XTAL: f64 = 114_269_790.0;

    /// Referenční hodnoty vygenerované z quisk/softrock/hardware_usb.py
    /// (SetFreqByDirect) se stejným krystalem. Musí sedět bajt po bajtu.
    #[test]
    fn registry_sedi_s_quiskem() {
        let cases: &[(f64, [u8; 6])] = &[
            (1_000_000.0, [251, 194, 178, 4, 34, 22]),
            (3_700_000.0, [20, 66, 167, 181, 241, 10]),
            (7_300_000.0, [101, 194, 174, 225, 52, 141]),
            (14_200_000.0, [225, 194, 187, 223, 158, 235]),
            (21_200_000.0, [66, 66, 200, 107, 85, 16]),
            (28_500_000.0, [224, 194, 190, 86, 126, 32]),
        ];
        for &(freq, expected) in cases {
            let got = registers(freq, XTAL).expect("výpočet selhal");
            assert_eq!(got, expected, "neshoda na {:.0} Hz", freq);
        }
    }

    #[test]
    fn mimo_rozsah_selze() {
        // 500 kHz je pod dosahem Si570 - quisk tu taky vrací False.
        assert!(registers(500_000.0, XTAL).is_err());
        assert!(registers(0.0, XTAL).is_err());
    }
}
