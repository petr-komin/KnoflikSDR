//! Rozpis rozhlasového vysílání na KV podle EiBi.
//!
//! Data jsou z <http://www.eibispace.de> - sezónní CSV, které Eike Bierwirth
//! udržuje a poskytuje volně. Stahuje se jednou za sezónu do cache.
//!
//! Formát řádku:
//! `kHz;Time(UTC);Days;ITU;Station;Lng;Target;Remarks;P;Start;Stop;`
//!
//! Pozor na tři věci, na kterých se naivní parser rozsype:
//! časy přes půlnoc (`2100-0100`), rozsahy dní přes týden (`We-Mo`)
//! a slepené dny bez oddělovače (`SaSu` = sobota i neděle).

use anyhow::{anyhow, Result};
use chrono::{Datelike, Timelike, Utc, Weekday};
use std::io::Read;
use std::path::PathBuf;

const DAY_NAMES: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

/// Množina dní jako bitová maska, pondělí = bit 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DaySet(u8);

impl DaySet {
    pub const ALL: DaySet = DaySet(0b111_1111);

    pub fn contains(&self, day: u8) -> bool {
        self.0 & (1 << day) != 0
    }

    fn day_index(s: &str) -> Option<u8> {
        DAY_NAMES.iter().position(|d| *d == s).map(|i| i as u8)
    }

    /// Rozebere pole Days z EiBi. Prázdné nebo neznámé = každý den.
    pub fn parse(s: &str) -> DaySet {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("irr") {
            return DaySet::ALL;
        }
        let mut mask = 0u8;
        for part in s.split(',') {
            let part = part.trim();
            if let Some((a, b)) = part.split_once('-') {
                // Rozsah, klidně přes konec týdne: "We-Mo".
                let (Some(from), Some(to)) = (Self::day_index(a), Self::day_index(b)) else {
                    continue;
                };
                let mut d = from;
                loop {
                    mask |= 1 << d;
                    if d == to {
                        break;
                    }
                    d = (d + 1) % 7;
                }
            } else {
                // Jeden nebo víc dní slepených za sebou: "Su", "SaSu".
                let bytes = part.as_bytes();
                let mut i = 0;
                while i + 2 <= bytes.len() {
                    if let Some(d) = Self::day_index(&part[i..i + 2]) {
                        mask |= 1 << d;
                    }
                    i += 2;
                }
            }
        }
        if mask == 0 { DaySet::ALL } else { DaySet(mask) }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub freq_khz: f64,
    /// Čas v UTC jako HHMM.
    pub start: u16,
    pub end: u16,
    pub days: DaySet,
    /// Kód země podle ITU.
    pub country: String,
    pub station: String,
    pub language: String,
    pub target: String,
}

impl Entry {
    /// Vysílá se v daný čas a den?
    pub fn active_at(&self, hhmm: u16, day: u8) -> bool {
        if !self.days.contains(day) {
            return false;
        }
        if self.start == self.end {
            return true; // celodenní
        }
        if self.start <= self.end {
            hhmm >= self.start && hhmm < self.end
        } else {
            // Přes půlnoc: 2100-0100.
            hhmm >= self.start || hhmm < self.end
        }
    }
}

pub struct Schedule {
    pub entries: Vec<Entry>,
    pub season: String,
}

fn parse_hhmm(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.len() != 4 {
        return None;
    }
    s.parse::<u16>().ok()
}

impl Schedule {
    pub fn parse(text: &str, season: &str) -> Schedule {
        let mut entries = Vec::new();
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split(';').collect();
            if f.len() < 8 {
                continue;
            }
            let Ok(freq) = f[0].trim().parse::<f64>() else {
                continue;
            };
            let Some((a, b)) = f[1].trim().split_once('-') else {
                continue;
            };
            let (Some(start), Some(end)) = (parse_hhmm(a), parse_hhmm(b)) else {
                continue;
            };
            entries.push(Entry {
                freq_khz: freq,
                start,
                end,
                days: DaySet::parse(f[2]),
                country: f[3].trim().to_string(),
                station: f[4].trim().to_string(),
                language: f[5].trim().to_string(),
                target: f[6].trim().to_string(),
            });
        }
        entries.sort_by(|a, b| a.freq_khz.partial_cmp(&b.freq_khz).unwrap());
        Schedule {
            entries,
            season: season.to_string(),
        }
    }

    /// Stanice, které se teď mají vysílat na dané frekvenci.
    ///
    /// `tolerance_khz` pokrývá nepřesnost ladění i to, že se stanice
    /// od rozpisu občas o kousek liší.
    pub fn lookup(&self, freq_khz: f64, tolerance_khz: f64) -> Vec<&Entry> {
        let now = Utc::now();
        let hhmm = (now.hour() * 100 + now.minute()) as u16;
        let day = weekday_index(now.weekday());
        let mut v: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| (e.freq_khz - freq_khz).abs() <= tolerance_khz && e.active_at(hhmm, day))
            .collect();
        // Nejbližší frekvenci napřed.
        v.sort_by(|a, b| {
            (a.freq_khz - freq_khz)
                .abs()
                .partial_cmp(&(b.freq_khz - freq_khz).abs())
                .unwrap()
        });
        v.dedup_by(|a, b| a.station == b.station && a.freq_khz == b.freq_khz);
        v
    }
}

fn weekday_index(w: Weekday) -> u8 {
    match w {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    }
}

/// Poslední neděle v daném měsíci - tak se přepínají vysílací sezóny.
fn last_sunday(year: i32, month: u32) -> u32 {
    use chrono::NaiveDate;
    let mut d = 31;
    loop {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, d) {
            if date.weekday() == Weekday::Sun {
                return d;
            }
            d -= 1;
        } else {
            d -= 1;
        }
        if d == 0 {
            return 31;
        }
    }
}

/// Označení sezóny pro dnešek, např. "a26" (léto) nebo "b25" (zima).
///
/// Sezóna A běží od poslední březnové neděle do poslední říjnové,
/// B po zbytek roku - stejně jako letní čas.
pub fn current_season() -> String {
    let now = Utc::now().date_naive();
    let (y, m, d) = (now.year(), now.month(), now.day());
    let summer = match m {
        4..=9 => true,
        3 => d >= last_sunday(y, 3),
        10 => d < last_sunday(y, 10),
        _ => false,
    };
    if summer {
        format!("a{:02}", y % 100)
    } else {
        // Zimní sezóna začínající v říjnu patří k tomu roku; v lednu až
        // březnu jsme pořád v sezóně, která začala loni na podzim.
        let yy = if m <= 3 { y - 1 } else { y };
        format!("b{:02}", yy % 100)
    }
}

fn cache_path(season: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("knoflik-sdr").join(format!("eibi-{season}.csv")))
}

/// EiBi posílá CSV v Latin-1, ne v UTF-8 - jsou v něm jména jako
/// "Rádio Clube do Pará". Prvních 256 znaků Unicode je shodných s Latin-1,
/// takže převod je prosté rozšíření bajtu na znak. Bez toho by se soubor
/// buď vůbec nepřečetl, nebo by se z diakritiky staly otazníky.
fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Načte rozpis z cache, a když tam není, stáhne ho.
pub fn load_or_fetch() -> Result<Schedule> {
    let season = current_season();
    let path = cache_path(&season).ok_or_else(|| anyhow!("nelze určit adresář cache"))?;

    // V cache je už UTF-8, protože ho tam ukládáme převedený.
    if let Ok(text) = std::fs::read_to_string(&path) {
        let s = Schedule::parse(&text, &season);
        if !s.entries.is_empty() {
            return Ok(s);
        }
    }

    let url = format!("http://www.eibispace.de/dx/sked-{season}.csv");
    let mut body = Vec::new();
    ureq::get(&url)
        .call()
        .map_err(|e| anyhow!("stažení {url} selhalo: {e}"))?
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|e| anyhow!("čtení odpovědi selhalo: {e}"))?;
    let text = latin1_to_string(&body);

    let s = Schedule::parse(&text, &season);
    if s.entries.is_empty() {
        return Err(anyhow!("rozpis {season} se stáhl, ale nedal se přečíst"));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, &text);
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prazdne_dny_znamenaji_kazdy_den() {
        assert_eq!(DaySet::parse(""), DaySet::ALL);
        // "irr" = nepravidelně; ať to radši ukážeme, než abychom to zamlčeli.
        assert_eq!(DaySet::parse("irr"), DaySet::ALL);
    }

    #[test]
    fn jeden_den() {
        let d = DaySet::parse("Su");
        assert!(d.contains(6));
        assert!(!d.contains(0));
    }

    #[test]
    fn rozsah_dnu() {
        let d = DaySet::parse("Mo-Fr");
        for i in 0..5 {
            assert!(d.contains(i), "má obsahovat den {i}");
        }
        assert!(!d.contains(5) && !d.contains(6), "víkend tam nepatří");
    }

    /// "We-Mo" přetéká přes konec týdne - středa až pondělí.
    #[test]
    fn rozsah_dnu_pres_tyden() {
        let d = DaySet::parse("We-Mo");
        for i in [2, 3, 4, 5, 6, 0] {
            assert!(d.contains(i), "má obsahovat den {i}");
        }
        assert!(!d.contains(1), "úterý tam nepatří");
    }

    /// EiBi píše víkend jako "SaSu" bez oddělovače.
    #[test]
    fn slepene_dny() {
        let d = DaySet::parse("SaSu");
        assert!(d.contains(5) && d.contains(6));
        assert!(!d.contains(0));
    }

    #[test]
    fn seznam_dnu() {
        let d = DaySet::parse("Tu,Fr");
        assert!(d.contains(1) && d.contains(4));
        assert!(!d.contains(2));
    }

    #[test]
    fn cas_v_ramci_dne() {
        let e = Entry {
            freq_khz: 6060.0,
            start: 1600,
            end: 1700,
            days: DaySet::ALL,
            country: "CHN".into(),
            station: "Test".into(),
            language: "E".into(),
            target: "SEA".into(),
        };
        assert!(e.active_at(1600, 0));
        assert!(e.active_at(1659, 0));
        assert!(!e.active_at(1700, 0), "konec je nevčetně");
        assert!(!e.active_at(1559, 0));
    }

    /// Vysílání přes půlnoc: 2100-0100 platí ve 23:00 i v 00:30.
    #[test]
    fn cas_pres_pulnoc() {
        let e = Entry {
            freq_khz: 6060.0,
            start: 2100,
            end: 100,
            days: DaySet::ALL,
            country: "X".into(),
            station: "T".into(),
            language: "".into(),
            target: "".into(),
        };
        assert!(e.active_at(2300, 0), "23:00 spadá do 2100-0100");
        assert!(e.active_at(30, 0), "00:30 taky");
        assert!(!e.active_at(1200, 0), "poledne ne");
    }

    #[test]
    fn den_omezuje_i_kdyz_cas_sedi() {
        let e = Entry {
            freq_khz: 6060.0,
            start: 0,
            end: 2400,
            days: DaySet::parse("Su"),
            country: "X".into(),
            station: "T".into(),
            language: "".into(),
            target: "".into(),
        };
        assert!(e.active_at(1200, 6), "v neděli ano");
        assert!(!e.active_at(1200, 0), "v pondělí ne");
    }

    #[test]
    fn parser_precte_radek_z_eibi() {
        let csv = "kHz:75;Time(UTC):93;Days:59;ITU:49;Station:201;Lng:49;Target:62;Remarks;P;Start;Stop;\n\
                   6060;1600-1700;;CHN;China Radio Int.;E;SEA;k;0;;\n\
                   7300;2115-2145;Su;AUS;HCJB R.Akhbar Mufriha;A;NAf;/G-w;1;;\n";
        let s = Schedule::parse(csv, "a26");
        assert_eq!(s.entries.len(), 2);
        let e = &s.entries[0];
        assert_eq!(e.freq_khz, 6060.0);
        assert_eq!(e.station, "China Radio Int.");
        assert_eq!(e.country, "CHN");
        assert_eq!(e.start, 1600);
        assert!(s.entries[1].days.contains(6));
    }

    #[test]
    fn parser_preskoci_nesmysly() {
        let csv = "hlavicka\nnesmysl\n;;;;\n6060;xxxx-yyyy;;X;Y;;;\n6060;1600-1700;;CHN;OK;E;SEA;k;0;;\n";
        let s = Schedule::parse(csv, "a26");
        assert_eq!(s.entries.len(), 1, "měl projít jen platný řádek");
        assert_eq!(s.entries[0].station, "OK");
    }

    /// Ověření proti skutečnému souboru z EiBi, ne jen proti mému vzorku.
    /// Spustit: cargo test --release ostry_rozpis -- --ignored --nocapture
    #[test]
    #[ignore]
    fn ostry_rozpis() {
        let s = load_or_fetch().expect("rozpis se nenačetl");
        println!("\nsezóna {}: {} záznamů", s.season, s.entries.len());
        assert!(s.entries.len() > 5000, "jen {} záznamů, něco je špatně", s.entries.len());

        // Kolik z nich má omezené dny - kdyby to bylo 0, parser dnů je mrtvý.
        let omezene = s.entries.iter().filter(|e| e.days != DaySet::ALL).count();
        println!("záznamů s omezenými dny: {omezene}");
        assert!(omezene > 500, "podezřele málo záznamů s omezenými dny: {omezene}");

        // Vysílání přes půlnoc musí existovat.
        let pres_pulnoc = s.entries.iter().filter(|e| e.start > e.end).count();
        println!("záznamů přes půlnoc: {pres_pulnoc}");
        assert!(pres_pulnoc > 10, "žádné přes půlnoc? {pres_pulnoc}");

        let now = Utc::now();
        println!("teď je {} UTC", now.format("%a %H:%M"));
        for f in [6060.0, 7300.0, 9400.0, 5900.0] {
            let v = s.lookup(f, 2.0);
            println!("  {f:.0} kHz -> {} stanic teď na vysílání", v.len());
            for e in v.iter().take(3) {
                println!("      {} ({}) {} {}-{}", e.station, e.country, e.language, e.start, e.end);
            }
        }
    }

    /// EiBi je v Latin-1. Bez převodu by se diakritika buď ztratila,
    /// nebo by se soubor vůbec nedal přečíst jako UTF-8.
    #[test]
    fn latin1_prevede_diakritiku() {
        // "Rádio Clube do Pará" tak, jak to leží v souboru.
        let raw = b"690;0000-2400;;B;R\xe1dio Clube do Par\xe1;P;B;be;1;;";
        assert!(
            std::str::from_utf8(raw).is_err(),
            "tenhle vzorek má být neplatné UTF-8, jinak test nic netestuje"
        );
        let s = latin1_to_string(raw);
        let sch = Schedule::parse(&format!("hlavicka\n{s}\n"), "a26");
        assert_eq!(sch.entries.len(), 1);
        assert_eq!(sch.entries[0].station, "Rádio Clube do Pará");
    }

    #[test]
    fn sezona_ma_spravny_tvar() {
        let s = current_season();
        assert!(
            s.len() == 3 && (s.starts_with('a') || s.starts_with('b')),
            "podivná sezóna: {s}"
        );
        assert!(s[1..].parse::<u32>().is_ok());
    }

    #[test]
    fn posledni_nedele_v_breznu_2026() {
        // 29. 3. 2026 je neděle - tehdy začíná sezóna A26.
        assert_eq!(last_sunday(2026, 3), 29);
        // 25. 10. 2026 je neděle.
        assert_eq!(last_sunday(2026, 10), 25);
    }
}
