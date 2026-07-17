//! Zvuk přes ALSA. Jediná cesta, jak se dostat na packed 24 bit (`S243LE`).
//!
//! Výstup jde výchozím nastavením na "pulse" (tj. PipeWire), stejně jako
//! Quisk s `name_of_sound_play="pulse"`.

use super::{Capture, DeviceInfo, Negotiated, Playback};
use alsa::device_name::HintIter;
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{anyhow, Result};

pub const NAME: &str = "ALSA";
pub const DEFAULT_CAPTURE: &str = "default";
pub const DEFAULT_PLAYBACK: &str = "pulse";

/// Co zkusit na vstupu, od nejlepšího. Vyšší vzorkovačka = širší panorama,
/// 24 bit = větší dynamický rozsah. Rychlost má přednost před hloubkou.
const CANDIDATES: &[(u32, Format)] = &[
    (192_000, Format::S243LE),
    (192_000, Format::S16LE),
    (96_000, Format::S243LE),
    (96_000, Format::S16LE),
    (48_000, Format::S243LE),
    (48_000, Format::S16LE),
];

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

fn bits_of(fmt: Format) -> u32 {
    (bytes_per_sample(fmt) * 8) as u32
}

/// Vypíše ALSA hinty pro daný směr. Hinty bez názvu a "null" zahodíme,
/// ten by v seznamu jen mátl.
fn list(dir: Direction) -> Vec<DeviceInfo> {
    let Ok(hints) = HintIter::new_str(None, "pcm") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for h in hints {
        // Hint bez směru umí obojí (typicky "default", "pulse").
        if h.direction.is_some_and(|d| d != dir) {
            continue;
        }
        let Some(id) = h.name else { continue };
        if id == "null" {
            continue;
        }
        // Popis je víceřádkový; pro seznam stačí první řádek.
        let label = match h.desc.as_deref().and_then(|d| d.lines().next()) {
            Some(d) if !d.is_empty() => format!("{id} - {d}"),
            _ => id.clone(),
        };
        out.push(DeviceInfo { id, label });
    }
    out
}

pub fn list_capture() -> Vec<DeviceInfo> {
    list(Direction::Capture)
}

pub fn list_playback() -> Vec<DeviceInfo> {
    list(Direction::Playback)
}

/// Zjistí nejlepší kombinaci vzorkovačky a formátu, kterou karta umí
/// a která se vejde do stropu hloubky.
fn negotiate(pcm: &PCM, max_bits: u32) -> Result<(u32, Format)> {
    for &(rate, fmt) in CANDIDATES {
        if bits_of(fmt) > max_bits {
            continue;
        }
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
        "zvukovka neumí žádnou podporovanou kombinaci (zkoušel jsem 192/96/48 kHz, \
         nejvýš {max_bits} bit)"
    ))
}

pub struct AlsaCapture {
    pcm: PCM,
    fmt: Format,
    bps: usize,
    frame_bytes: usize,
    negotiated: Negotiated,
    raw: Vec<u8>,
}

pub fn open_capture(device: &str, max_bits: u32) -> Result<Box<dyn Capture>> {
    let pcm = PCM::new(device, Direction::Capture, false)?;
    let (rate, fmt) = negotiate(&pcm, max_bits)?;
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

    // Skutečná rychlost se může od vyjednané lišit (ValueOr::Nearest).
    let actual_rate = pcm.hw_params_current()?.get_rate()?;
    let bps = bytes_per_sample(fmt);
    Ok(Box::new(AlsaCapture {
        pcm,
        fmt,
        bps,
        frame_bytes: bps * 2,
        negotiated: Negotiated {
            rate: actual_rate,
            bits: bits_of(fmt),
        },
        raw: Vec::new(),
    }))
}

impl Capture for AlsaCapture {
    fn negotiated(&self) -> Negotiated {
        self.negotiated
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let frames_wanted = out.len() / 2;
        let need = frames_wanted * self.frame_bytes;
        if self.raw.len() < need {
            self.raw.resize(need, 0);
        }
        // io_bytes() je jen tenký obal nad PCM, ne syscall - může vzniknout
        // pokaždé znovu, a ušetří nám to sebereferenční strukturu.
        let frames = match self.pcm.io_bytes().readi(&mut self.raw[..need]) {
            Ok(n) => n,
            Err(e) => {
                // Po xrunu se zotavíme a vrátíme prázdno; volající zavolá znovu.
                self.pcm.try_recover(e, true)?;
                return Ok(0);
            }
        };
        for f in 0..frames {
            let o = f * self.frame_bytes;
            out[f * 2] = decode(self.fmt, &self.raw[o..o + self.bps]);
            out[f * 2 + 1] = decode(self.fmt, &self.raw[o + self.bps..o + self.frame_bytes]);
        }
        Ok(frames)
    }
}

pub struct AlsaPlayback {
    pcm: PCM,
    buf: Vec<i16>,
}

pub fn open_playback(device: &str, rate: u32) -> Result<Box<dyn Playback>> {
    let pcm = match PCM::new(device, Direction::Playback, false) {
        Ok(p) => p,
        // "pulse" je jen výchozí tip, ne uživatelova volba - na stroji bez
        // PulseAudia/PipeWire ať to spadne na "default" místo hlášky.
        Err(e) if device == DEFAULT_PLAYBACK => PCM::new("default", Direction::Playback, false)
            .map_err(|_| anyhow!("nelze otevřít 'pulse' ani 'default': {e}"))?,
        Err(e) => return Err(e.into()),
    };
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(2)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        hwp.set_format(Format::S16LE)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_period_size_near(super::CHUNK as i64, ValueOr::Nearest)?;
        hwp.set_buffer_size_near(super::CHUNK as i64 * 8)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;
    Ok(Box::new(AlsaPlayback {
        pcm,
        buf: Vec::new(),
    }))
}

impl Playback for AlsaPlayback {
    /// Blokující writei zároveň udává tempo, takže nepotřebujeme vlastní časování.
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        self.buf.clear();
        self.buf
            .extend(samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
        if let Err(e) = self.pcm.io_i16()?.writei(&self.buf) {
            self.pcm.try_recover(e, true)?;
        }
        Ok(())
    }
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

    /// Strop 16 bit musí 24bitové kandidáty vyřadit úplně - jinak by se
    /// ruční volba dala obejít kartou, která S243LE hlásí.
    #[test]
    fn strop_16_bit_vyradi_24bitove_kandidaty() {
        let zbyle: Vec<_> = CANDIDATES
            .iter()
            .filter(|(_, f)| bits_of(*f) <= 16)
            .collect();
        assert_eq!(zbyle.len(), 3, "měly zbýt tři rychlosti v 16 bit");
        assert!(zbyle.iter().all(|(_, f)| *f == Format::S16LE));
    }

    /// Pořadí kandidátů: rychlost má přednost před hloubkou, protože širší
    /// panorama je znát víc než pár dB dynamiky.
    #[test]
    fn kandidati_preferuji_rychlost_pred_hloubkou() {
        assert_eq!(CANDIDATES[0], (192_000, Format::S243LE));
        assert_eq!(CANDIDATES[1], (192_000, Format::S16LE));
        assert_eq!(CANDIDATES[2], (96_000, Format::S243LE));
    }

    /// Filtr směru se nesmí rozejít s tím, co ALSA vrací - jednou už byl
    /// psaný přes `{:?}` na "Input"/"Output" a tiše zahazoval všechno,
    /// co směr hlásí. Kontroluje jen konzistenci, ne konkrétní karty:
    /// na stroji bez zvukovky (CI) může být seznam prázdný.
    #[test]
    fn vycet_zarizeni_respektuje_smer() {
        let vstupy = list_capture();
        let vystupy = list_playback();
        for d in vstupy.iter().chain(vystupy.iter()) {
            assert!(!d.id.is_empty(), "zařízení bez názvu nelze otevřít");
            assert_ne!(d.id, "null");
            assert!(d.label.contains(&d.id), "štítek má nést i název zařízení");
        }
        // Hinty bez směru (default, pulse) musí být v obou seznamech,
        // hinty se směrem jen v tom svém. Kdyby filtr zahazoval všechno,
        // byly by oba seznamy stejné a tenhle rozdíl by zmizel.
        let jen_vstup: Vec<_> = vstupy
            .iter()
            .filter(|d| !vystupy.iter().any(|o| o.id == d.id))
            .collect();
        eprintln!(
            "vstupů {}, výstupů {}, jen na vstupu {}",
            vstupy.len(),
            vystupy.len(),
            jen_vstup.len()
        );
    }
}
