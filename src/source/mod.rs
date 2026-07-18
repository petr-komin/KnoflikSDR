//! Zdroj I/Q, nezávisle na tom, jaké rádio je na druhém konci.
//!
//! Zdroj je schválně rozdělený na dvě půlky, které jdou do různých vláken:
//!
//! - [`Source`] dodává vzorky a bydlí v DSP vlákně, kde se čte v těsné smyčce.
//! - [`Tuner`] ladí a řídí zisk a bydlí ve vlastním vlákně, protože u SoftRocku
//!   je to zápis do Si570 po USB - ten trvá jednotky ms a v DSP vlákně by cvakal.
//!
//! U SoftRocku jsou to dvě fyzicky nezávislé věci (zvukovka a Si570 na USB),
//! u RSP1 jedno zařízení - ale `soapysdr::Device` je `Clone` a ladění bere
//! `&self`, takže se dá držet klon v každé půlce.

use crate::settings::Settings;
use anyhow::Result;
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

mod softrock;

#[cfg(feature = "rsp1")]
mod rsp1;

/// Které rádio zrovna jede.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Hardware {
    // Jinak by ze `SoftRock` vyšlo v configu "soft_rock".
    #[default]
    #[serde(rename = "softrock")]
    SoftRock,
    Rsp1,
}

/// Nabízené vzorkovačky RSP1, od nejužší. Každá je násobek 48 kHz, takže
/// decimace na zvuk vychází celým číslem (viz docs/sdrplay-rsp1.md §2.1).
/// Nižší = užší panorama a méně zátěže; vyšší = širší přehled, ale dráž.
/// Všechny ověřeny na hardwaru (100 % nenulových vzorků, špička >85 dB).
pub const RSP1_RATES_HZ: &[f64] = &[
    1_344_000.0, // 48k × 28 - nejužší, co libmirisdr pustí (min 1,3 MHz)
    1_920_000.0, // 48k × 40
    3_072_000.0, // 48k × 64
    4_800_000.0, // 48k × 100
    6_000_000.0, // 48k × 125
];
pub const RSP1_DEFAULT_RATE_HZ: f64 = RSP1_RATES_HZ[0];

/// Decimace ze vzorkovačky RSP1 na 48 kHz audio.
pub fn rsp1_decim(rate_hz: f64) -> usize {
    (rate_hz / 48_000.0).round() as usize
}

impl Hardware {
    pub fn label(self) -> &'static str {
        match self {
            Hardware::SoftRock => "SoftRock",
            Hardware::Rsp1 => "SDRplay RSP1",
        }
    }

    /// Je tahle volba v tomhle sestavení vůbec k dispozici? RSP1 jede přes
    /// SoapySDR, který je jen na Linuxu a za feature `rsp1`.
    pub fn available(self) -> bool {
        match self {
            Hardware::SoftRock => true,
            Hardware::Rsp1 => cfg!(feature = "rsp1"),
        }
    }

    /// Ladí se rádio přes Si570 po USB? Podle toho se v nastavení ukazuje
    /// kalibrace krystalu.
    pub fn uses_si570(self) -> bool {
        self == Hardware::SoftRock
    }

    /// Meze ladění VFO v kHz. SoftRock je krátkovlnný (Si570 přes zvukovku),
    /// RSP1 ladí souvisle do 2 GHz - ten strop je celý důvod, proč tohle
    /// záviselo na rádiu. "Díry", které hlásí knihovna, jsou lež v metadatech
    /// (ověřeno na hardwaru, viz docs/sdrplay-rsp1.md §2), takže se ladí i tam.
    pub fn tuning_range_khz(self) -> (f64, f64) {
        match self {
            Hardware::SoftRock => (100.0, 60_000.0),
            Hardware::Rsp1 => (100.0, 2_000_000.0),
        }
    }

    pub const ALL: [Hardware; 2] = [Hardware::SoftRock, Hardware::Rsp1];
}

/// Rozsah zisku v dB, pokud ho rádio umí.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GainRange {
    pub min: f64,
    pub max: f64,
}

/// Přísun vzorků. Žije v DSP vlákně.
pub trait Source: Send {
    /// Blokující. Naplní `out` vzorky I/Q a vrátí jejich počet.
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize>;
    /// Skutečná vzorkovačka = šířka panoramatu.
    fn rate(&self) -> f64;
    /// Do stavového řádku.
    fn label(&self) -> String;
}

/// Ladění a zisk. Žije ve vlastním vlákně, ať pomalý zápis po USB
/// nebrzdí DSP.
pub trait Tuner: Send {
    /// Naladit střed pásma (VFO) na danou frekvenci v Hz.
    fn set_center(&mut self, hz: f64) -> Result<()>;
    /// Do stavového řádku - typicky verze firmware nebo název zařízení.
    fn label(&self) -> String;
    /// Rozsah zisku, nebo `None` u rádia, kde se zisk neřídí (SoftRock).
    fn gain_range(&self) -> Option<GainRange> {
        None
    }
    fn set_gain(&mut self, _db: f64) -> Result<()> {
        Ok(())
    }
}

/// Otevře rádio a rozdělí ho na obě půlky.
///
/// Volá se z DSP vlákna, které si nechá [`Source`] a [`Tuner`] pošle dál -
/// tím pádem se obě půlky otevřou naráz a při opětovném připojení se obě
/// vymění společně.
pub fn open(hw: Hardware, set: &Settings) -> Result<(Box<dyn Source>, Box<dyn Tuner>)> {
    match hw {
        Hardware::SoftRock => softrock::open(set),
        #[cfg(feature = "rsp1")]
        Hardware::Rsp1 => rsp1::open(set),
        #[cfg(not(feature = "rsp1"))]
        Hardware::Rsp1 => Err(anyhow::anyhow!(
            "podpora RSP1 není v tomhle sestavení (chybí feature `rsp1`)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softrock_je_vzdy_k_dispozici() {
        assert!(Hardware::SoftRock.available());
    }

    /// RSP1 se smí nabízet jen tam, kde je za ním kód - jinak by uživatel
    /// přepnul na hardware, který se nemá jak otevřít.
    #[test]
    fn rsp1_jen_s_feature() {
        assert_eq!(Hardware::Rsp1.available(), cfg!(feature = "rsp1"));
    }

    /// Každá nabízená vzorkovačka RSP1 musí dělit 48 kHz beze zbytku a být
    /// v rozsahu, který libmirisdr pustí (1,3-12 MHz). Kdyby někdo do seznamu
    /// přidal hodnotu, co nevychází, decimace by zkreslila stupnici.
    #[test]
    fn vsechny_rsp1_vzorkovacky_deli_48k_a_jsou_v_rozsahu() {
        for &r in RSP1_RATES_HZ {
            assert_eq!(r as usize % 48_000, 0, "{r} Hz nedělí 48 kHz");
            assert!(
                (1_300_000.0..=12_000_000.0).contains(&r),
                "{r} Hz je mimo rozsah libmirisdr"
            );
            assert_eq!(rsp1_decim(r) as f64 * 48_000.0, r);
        }
    }

    #[test]
    fn rsp1_vzorkovacky_jsou_serazene_od_nejuzsi() {
        assert!(RSP1_RATES_HZ.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(RSP1_DEFAULT_RATE_HZ, RSP1_RATES_HZ[0]);
    }

    #[test]
    fn si570_ma_jen_softrock() {
        assert!(Hardware::SoftRock.uses_si570());
        assert!(!Hardware::Rsp1.uses_si570());
    }

    /// RSP1 musí dosáhnout na VKV (byl to celý důvod téhle změny), SoftRock
    /// zůstává krátkovlnný.
    #[test]
    fn rozsah_ladeni_odpovida_radiu() {
        let (_, sr_hi) = Hardware::SoftRock.tuning_range_khz();
        let (_, rsp_hi) = Hardware::Rsp1.tuning_range_khz();
        assert!(sr_hi <= 100_000.0, "SoftRock nemá sahat na VKV");
        assert!(rsp_hi >= 108_000.0, "RSP1 musí dosáhnout na FM rozhlas");
        // Každé rádio musí umět naladit svá vlastní bandplanová pásma.
        for (lo, hi) in [
            Hardware::SoftRock.tuning_range_khz(),
            Hardware::Rsp1.tuning_range_khz(),
        ] {
            assert!(lo < hi && lo > 0.0);
        }
    }

    #[test]
    fn hardware_prezije_toml() {
        for hw in Hardware::ALL {
            let text = toml::to_string(&W { hw }).unwrap();
            assert_eq!(toml::from_str::<W>(&text).unwrap().hw, hw);
        }
    }

    /// Config je uživatelský soubor, tak ať v něm nestojí "soft_rock".
    #[test]
    fn hardware_ma_v_configu_ctitelna_jmena() {
        assert!(toml::to_string(&W {
            hw: Hardware::SoftRock
        })
        .unwrap()
        .contains("\"softrock\""));
        assert!(toml::to_string(&W { hw: Hardware::Rsp1 })
            .unwrap()
            .contains("\"rsp1\""));
    }

    #[derive(Serialize, Deserialize)]
    struct W {
        hw: Hardware,
    }
}
