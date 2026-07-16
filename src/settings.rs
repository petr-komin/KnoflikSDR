//! Uživatelské nastavení - ukládá se do ~/.config/knoflik-sdr/config.toml.
//!
//! Zápis je odložený o `SAVE_DELAY` od poslední změny, aby tažení posuvníku
//! nepsalo na disk každý snímek.

use crate::dsp::Mode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Jak dlouho po poslední změně se čeká, než se zapíše.
const SAVE_DELAY: Duration = Duration::from_millis(800);

/// Uložená oblíbená stanice. Frekvence je absolutní, ne offset od VFO -
/// jinak by záznam přestal platit při každém posunu okna.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Station {
    pub name: String,
    pub freq_khz: f64,
    pub mode: Mode,
    pub bandwidth_hz: f64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    pub vfo_khz: f64,
    pub offset_hz: f64,
    pub mode: Mode,
    /// Šířka pásma se pamatuje zvlášť pro AM a pro SSB - jinak by přepnutí
    /// režimu zdědilo nesmyslnou hodnotu z toho druhého.
    pub bandwidth_am_hz: f64,
    pub bandwidth_ssb_hz: f64,
    pub volume: f32,
    pub swap_iq: bool,
    pub db_min: f32,
    pub db_max: f32,
    pub window_w: f32,
    pub window_h: f32,
    /// Přiblížení panoramatu: 1 = celá vzorkovačka, 8 = osmina.
    pub zoom: f32,
    /// Podbarvovat úseky pásem podle bandplanu?
    pub show_bandplan: bool,
    pub stations: Vec<Station>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            vfo_khz: 7300.0,
            offset_hz: 0.0,
            mode: Mode::Am,
            bandwidth_am_hz: crate::radio::AM_BANDWIDTH_HZ,
            bandwidth_ssb_hz: crate::radio::SSB_BANDWIDTH_HZ,
            volume: 0.5,
            swap_iq: false,
            db_min: -110.0,
            db_max: -20.0,
            window_w: 1100.0,
            window_h: 700.0,
            zoom: 1.0,
            show_bandplan: true,
            stations: Vec::new(),
        }
    }
}

impl Settings {
    /// Šířka pásma pro aktuální režim.
    pub fn bandwidth(&self) -> f64 {
        if self.mode.is_ssb() {
            self.bandwidth_ssb_hz
        } else {
            self.bandwidth_am_hz
        }
    }

    pub fn set_bandwidth(&mut self, bw: f64) {
        if self.mode.is_ssb() {
            self.bandwidth_ssb_hz = bw;
        } else {
            self.bandwidth_am_hz = bw;
        }
    }
}

const APP_DIR: &str = "knoflik-sdr";
/// Adresář z doby, kdy se projekt jmenoval rd-sdr. Čte se z něj, dokud
/// nevznikne nový config - ať uživatel po přejmenování nepřijde o stanice.
const LEGACY_APP_DIR: &str = "rd-sdr";

fn config_base() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn config_path() -> Option<PathBuf> {
    Some(config_path_in(&config_base()?))
}

fn config_path_in(base: &Path) -> PathBuf {
    base.join(APP_DIR).join("config.toml")
}

fn legacy_config_path_in(base: &Path) -> PathBuf {
    base.join(LEGACY_APP_DIR).join("config.toml")
}

impl Settings {
    /// Načte nastavení; při jakémkoli problému vrátí výchozí hodnoty,
    /// ať se aplikace kvůli rozbitému configu neodmítne spustit.
    pub fn load() -> Self {
        match config_base() {
            Some(base) => Self::load_from(&base),
            None => Self::default(),
        }
    }

    /// Načte nastavení z daného základního adresáře. Nový config má přednost;
    /// když ještě není, vezme se ten z dob, kdy se projekt jmenoval rd-sdr.
    /// Zapisuje se pak už jen na nové místo, takže se to samo přestěhuje.
    fn load_from(base: &Path) -> Self {
        let new = config_path_in(base);
        let legacy = legacy_config_path_in(base);
        let path = if new.exists() {
            new
        } else if legacy.exists() {
            legacy
        } else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("config {path:?} se nepodařilo přečíst ({e}), beru výchozí");
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("nelze vytvořit {dir:?}: {e}");
                return;
            }
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("nelze zapsat {path:?}: {e}");
                }
            }
            Err(e) => eprintln!("nelze serializovat nastavení: {e}"),
        }
    }
}

/// Hlídá změny a zapisuje je se zpožděním.
pub struct Autosave {
    last: Settings,
    dirty_since: Option<Instant>,
}

impl Autosave {
    pub fn new(initial: Settings) -> Self {
        Autosave {
            last: initial,
            dirty_since: None,
        }
    }

    /// Volat každý snímek s aktuálním nastavením.
    pub fn tick(&mut self, current: Settings) {
        if current != self.last {
            self.last = current;
            self.dirty_since = Some(Instant::now());
        }
        if self.dirty_since.is_some_and(|t| t.elapsed() >= SAVE_DELAY) {
            self.last.save();
            self.dirty_since = None;
        }
    }

    /// Dopsat okamžitě (při ukončení).
    pub fn flush(&mut self) {
        if self.dirty_since.take().is_some() {
            self.last.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prazdny_config_da_vychozi() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn castecny_config_doplni_zbytek() {
        // Chybějící klíče musí vzít výchozí, ne selhat - jinak by
        // starý config po přidání položky rozbil start.
        let s: Settings = toml::from_str("vfo_khz = 1350.0").unwrap();
        assert_eq!(s.vfo_khz, 1350.0);
        assert_eq!(s.bandwidth_am_hz, Settings::default().bandwidth_am_hz);
        assert_eq!(s.mode, Mode::Am);
    }

    #[test]
    fn stary_config_s_jinymi_klici_nespadne() {
        // Config z verze před režimy měl "bandwidth_hz" a žádný "mode".
        let s: Settings = toml::from_str("vfo_khz = 1350.0\nbandwidth_hz = 5000.0").unwrap();
        assert_eq!(s.vfo_khz, 1350.0);
        assert_eq!(s.mode, Mode::Am);
    }

    #[test]
    fn ulozeni_a_nacteni_zachova_hodnoty() {
        let s = Settings {
            vfo_khz: 5900.0,
            offset_hz: -12_000.0,
            mode: Mode::Lsb,
            bandwidth_am_hz: 5500.0,
            bandwidth_ssb_hz: 2400.0,
            volume: 0.33,
            swap_iq: true,
            db_min: -120.0,
            db_max: -30.0,
            window_w: 1280.0,
            window_h: 800.0,
            zoom: 4.0,
            show_bandplan: false,
            stations: vec![
                Station {
                    name: "Test AM".into(),
                    freq_khz: 1350.0,
                    mode: Mode::Am,
                    bandwidth_hz: 8000.0,
                },
                Station {
                    name: "Test SSB".into(),
                    freq_khz: 7130.5,
                    mode: Mode::Lsb,
                    bandwidth_hz: 2700.0,
                },
            ],
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    /// Dočasný adresář, ať testy nesahají na skutečný config uživatele.
    fn tmp_base(jmeno: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("knoflik-test-{jmeno}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn zapis(base: &Path, dir: &str, obsah: &str) {
        let d = base.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("config.toml"), obsah).unwrap();
    }

    /// Po přejmenování projektu se musí načíst config ze starého adresáře,
    /// jinak by uživatel přišel o nastavení i oblíbené stanice.
    #[test]
    fn nacte_config_z_puvodniho_adresare_rd_sdr() {
        let base = tmp_base("legacy");
        zapis(
            &base,
            "rd-sdr",
            "vfo_khz = 6060.0\nvolume = 0.92\nbandwidth_am_hz = 11000.0\n",
        );
        let s = Settings::load_from(&base);
        assert_eq!(s.vfo_khz, 6060.0, "nenačetlo se VFO ze starého configu");
        assert_eq!(s.volume, 0.92);
        assert_eq!(s.bandwidth_am_hz, 11000.0);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Jakmile existuje nový config, starý se ignoruje - jinak by se
    /// nastavení po prvním uložení vracelo zpátky.
    #[test]
    fn novy_config_ma_prednost_pred_starym() {
        let base = tmp_base("prednost");
        zapis(&base, "rd-sdr", "vfo_khz = 6060.0\n");
        zapis(&base, "knoflik-sdr", "vfo_khz = 1350.0\n");
        let s = Settings::load_from(&base);
        assert_eq!(s.vfo_khz, 1350.0, "vzalo se staré místo nového");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bez_configu_vychozi_hodnoty() {
        let base = tmp_base("prazdno");
        assert_eq!(Settings::load_from(&base), Settings::default());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn config_bez_stanic_da_prazdny_seznam() {
        // Configy z verze před oblíbenými nesmí po přidání seznamu spadnout.
        let s: Settings = toml::from_str("vfo_khz = 1350.0").unwrap();
        assert!(s.stations.is_empty());
    }

    #[test]
    fn sirka_pasma_se_pamatuje_zvlast_pro_kazdy_rezim() {
        let mut s = Settings::default();
        s.mode = Mode::Am;
        s.set_bandwidth(9000.0);
        s.mode = Mode::Usb;
        s.set_bandwidth(2400.0);
        // Návrat do AM musí vrátit původní šířku, ne tu z SSB.
        s.mode = Mode::Am;
        assert_eq!(s.bandwidth(), 9000.0);
        s.mode = Mode::Lsb;
        assert_eq!(s.bandwidth(), 2400.0, "USB a LSB sdílejí jednu šířku");
    }
}
