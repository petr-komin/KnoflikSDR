//! Zvukový vstup a výstup, nezávisle na systému.
//!
//! Na Linuxu jde všechno přes ALSA napřímo ([`alsa`]) - ta umí packed 24 bit
//! (`S243LE`), což je pro panorama znát na dynamickém rozsahu. Jinde se použije
//! [`cpal`] (WASAPI na Windows, CoreAudio na macOS).
//!
//! Zbytek programu vidí jen [`Capture`] a [`Playback`]: vzorky chodí ven
//! v f32 v rozsahu -1..1 a o formát na drátě se stará backend.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Kolik rámců zapisujeme na výstup najednou (~10 ms při 48 kHz).
const CHUNK: usize = 512;

#[cfg(target_os = "linux")]
mod alsa;
#[cfg(not(target_os = "linux"))]
mod cpal;

#[cfg(target_os = "linux")]
use alsa as backend;
#[cfg(not(target_os = "linux"))]
use cpal as backend;

/// Zvukové zařízení tak, jak ho nabídneme v nastavení.
#[derive(Clone, PartialEq, Debug)]
pub struct DeviceInfo {
    /// Co se předá backendu při otevření a uloží do configu.
    pub id: String,
    /// Co uvidí uživatel.
    pub label: String,
}

/// Strop bitové hloubky vstupu.
///
/// Existuje kvůli tomu, že packed 24 bit umí spolehlivě jen ALSA. Na Windows
/// a macOS jde přes cpal a tam se o hloubku dohaduje zvukový server, takže
/// [`Depth::Auto`] tam raději rovnou cílí na 16 bit. Ruční volba je tu pro
/// případy, kdy se automatika splete - karta hlásí 24 bit a neumí je,
/// nebo naopak na Windows kartu, která 24 bit zvládne.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    #[default]
    Auto,
    Bits16,
    Bits24,
}

impl Depth {
    /// Nejvyšší hloubka, kterou se smí zkusit vyjednat.
    pub fn max_bits(self) -> u32 {
        match self {
            // 24 bit jen tam, kde na něj sáhneme napřímo přes ALSA.
            Depth::Auto if cfg!(target_os = "linux") => 24,
            Depth::Auto => 16,
            Depth::Bits16 => 16,
            Depth::Bits24 => 24,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Depth::Auto => "automaticky",
            Depth::Bits16 => "nejvýš 16 bit",
            Depth::Bits24 => "nejvýš 24 bit",
        }
    }

    /// Popis pro nastavení - ať je u „automaticky“ vidět, co z toho vyleze.
    pub fn hint(self) -> String {
        match self {
            Depth::Auto => format!("{} bit ({})", self.max_bits(), backend::NAME),
            _ => format!("{} bit", self.max_bits()),
        }
    }

    pub const ALL: [Depth; 3] = [Depth::Auto, Depth::Bits16, Depth::Bits24];
}

/// Vyjednaný formát vstupu.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Negotiated {
    pub rate: u32,
    pub bits: u32,
}

/// Vstup I/Q ze zvukovky.
pub trait Capture: Send {
    fn negotiated(&self) -> Negotiated;
    /// Blokující čtení. Naplní `buf` prokládaným I/Q v rozsahu -1..1
    /// a vrátí počet rámců (tj. `buf` se naplní do `2 * návratová hodnota`).
    fn read(&mut self, buf: &mut [f32]) -> Result<usize>;
}

/// Výstup na reproduktory.
pub trait Playback: Send {
    /// Blokující zápis prokládaného sterea. Blokování zároveň udává tempo,
    /// takže si nemusíme držet vlastní časování.
    fn write(&mut self, buf: &[f32]) -> Result<()>;
}

/// Jak se jmenuje zvuková vrstva pod námi - do stavového řádku a nastavení.
pub fn backend_name() -> &'static str {
    backend::NAME
}

/// Zařízení, ze kterých jde brát I/Q. Když se výčet nepovede, vrátí prázdno -
/// nastavení pak nechá uživatele napsat si název ručně.
pub fn list_capture() -> Vec<DeviceInfo> {
    backend::list_capture()
}

pub fn list_playback() -> Vec<DeviceInfo> {
    backend::list_playback()
}

/// Co nabídnout, dokud si uživatel nevybral.
pub fn default_capture_device() -> String {
    backend::DEFAULT_CAPTURE.to_string()
}

pub fn default_playback_device() -> String {
    backend::DEFAULT_PLAYBACK.to_string()
}

/// Otevře vstup a vyjedná nejlepší kombinaci rychlosti a hloubky do `depth`.
pub fn open_capture(device: &str, depth: Depth) -> Result<Box<dyn Capture>> {
    backend::open_capture(device, depth.max_bits())
}

pub fn open_playback(device: &str, rate: u32) -> Result<Box<dyn Playback>> {
    backend::open_playback(device, rate)
}

/// Vlákno výstupu: bere z ringu a sype na zvukovku. Když se zařízení nepovede
/// otevřít (nebo cestou zmizí), zkouší to dokola - uživatel může mezitím
/// v nastavení přepnout na jiné.
pub fn spawn(
    audio_rx: rtrb::Consumer<f32>,
    device: String,
    rate: u32,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut audio_rx = audio_rx;
        while running.load(Ordering::Relaxed) {
            if let Err(e) = run_playback(&device, &mut audio_rx, rate, &running) {
                eprintln!("audio výstup ({device}): {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    })
}

fn run_playback(
    device: &str,
    audio_rx: &mut rtrb::Consumer<f32>,
    rate: u32,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let mut out = open_playback(device, rate)?;
    let mut buf = vec![0f32; CHUNK * 2];
    while running.load(Ordering::Relaxed) {
        for f in 0..CHUNK {
            // Prázdný ring = ticho. Nemá smysl čekat, výstup si drží tempo sám.
            let s = audio_rx.pop().unwrap_or(0.0);
            buf[f * 2] = s;
            buf[f * 2 + 1] = s;
        }
        out.write(&buf)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ruční volba musí platit i tam, kde by automatika rozhodla jinak -
    /// jinak by se uživatel nedostal přes kartu, co 24 bit hlásí a neumí.
    #[test]
    fn rucni_volba_prebije_automatiku() {
        assert_eq!(Depth::Bits16.max_bits(), 16);
        assert_eq!(Depth::Bits24.max_bits(), 24);
    }

    /// Auto smí sáhnout na 24 bit jen na Linuxu, kde jdeme na ALSA napřímo.
    #[test]
    fn auto_ceka_24_bit_jen_na_linuxu() {
        let ocekavano = if cfg!(target_os = "linux") { 24 } else { 16 };
        assert_eq!(Depth::Auto.max_bits(), ocekavano);
    }

    #[test]
    fn depth_prezije_toml() {
        for d in Depth::ALL {
            let text = toml::to_string(&Wrap { depth: d }).unwrap();
            let back: Wrap = toml::from_str(&text).unwrap();
            assert_eq!(back.depth, d);
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Wrap {
        depth: Depth,
    }
}
