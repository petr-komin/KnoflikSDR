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
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

const DAY_NAMES: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

/// Množina dní jako bitová maska, pondělí = bit 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DaySet(u8);

impl Default for DaySet {
    fn default() -> Self {
        DaySet::ALL
    }
}

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
        // Číselný zápis: "156" = pondělí, pátek, sobota (1 = pondělí).
        if s.chars().all(|c| ('1'..='7').contains(&c)) {
            let mut mask = 0u8;
            for c in s.chars() {
                mask |= 1 << (c as u8 - b'1');
            }
            return DaySet(mask);
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

#[derive(Clone, Debug, Default)]
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
    /// Kód místa vysílače, platí v rámci země.
    pub site: String,
    /// Kód trvanlivosti záznamu. 6 = platí jen mezi `from_date` a `to_date`.
    pub persistence: u8,
    /// Platnost v části sezóny jako (měsíc, den) - jen když persistence = 6.
    pub from_date: Option<(u32, u32)>,
    pub to_date: Option<(u32, u32)>,
}

impl Entry {
    /// Platí záznam k dnešnímu datu?
    ///
    /// Záznamy s persistence = 6 platí jen v části sezóny; EiBi u nich uvádí
    /// rozsah dat. Bez téhle kontroly bychom ukazovali vysílání, které je
    /// zrovna mimo svoje období.
    pub fn active_on_date(&self, month: u32, day: u32) -> bool {
        if self.persistence != 6 {
            return true;
        }
        let (Some((m1, d1)), Some((m2, d2))) = (self.from_date, self.to_date) else {
            return true;
        };
        let today = month * 100 + day;
        let (a, b) = (m1 * 100 + d1, m2 * 100 + d2);
        if a <= b {
            today >= a && today <= b
        } else {
            // Rozsah přes konec roku.
            today >= a || today <= b
        }
    }

    /// Země, ze které se ve skutečnosti vysílá, pokud je jiná než domovská.
    ///
    /// EiBi značí cizí vysílač předponou `/ABC` s kódem hostitelské země:
    /// `/OMA-a` znamená, že BBC jde přes vysílač v Ománu. Pro identifikaci
    /// stanice to rozhoduje - odjinud se šíří signál úplně jinak.
    pub fn relay_country(&self) -> Option<&str> {
        let s = self.site.strip_prefix('/')?;
        let end = s.find('-').unwrap_or(s.len());
        if end == 0 { None } else { Some(&s[..end]) }
    }

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

/// Vysvětlivky zkratek z README.TXT od EiBi: země, jazyky a cílové oblasti.
///
/// Bere se to z autoritativního zdroje místo vlastní tabulky, ať to nezastará
/// a ať se nemusí ručně udržovat 300 názvů zemí.
#[derive(Default)]
pub struct Codes {
    pub countries: BTreeMap<String, String>,
    pub languages: BTreeMap<String, String>,
    pub targets: BTreeMap<String, String>,
}

impl Codes {
    pub fn country(&self, code: &str) -> Option<&str> {
        self.countries.get(code).map(|s| s.as_str())
    }
    pub fn language(&self, code: &str) -> Option<&str> {
        self.languages.get(code).map(|s| s.as_str())
    }

    /// Jen holý název jazyka, bez výčtu zemí a počtu mluvčích.
    ///
    /// EiBi píše `English: UK (60m), USA (225m), India (200m), others`,
    /// do seznamu se ale hodí jenom "English".
    pub fn language_short(&self, code: &str) -> Option<&str> {
        let full = self.languages.get(code)?;
        let end = full
            .find([':', '('])
            .unwrap_or(full.len());
        Some(full[..end].trim())
    }
    pub fn target(&self, code: &str) -> Option<&str> {
        self.targets.get(code).map(|s| s.as_str())
    }

    /// Rozebere README.TXT. Sekce se poznají podle nadpisů "I) Language codes."
    /// atd.; uvnitř je vždy odsazený kód a za mezerami jeho význam.
    pub fn parse(text: &str) -> Codes {
        #[derive(PartialEq, Clone, Copy)]
        enum Sec {
            None,
            Lang,
            Country,
            Target,
        }
        let mut sec = Sec::None;
        let mut c = Codes::default();

        for line in text.lines() {
            let t = line.trim();
            // Nadpisy sekcí. Ten se seznamem obsahu na začátku má za textem
            // ještě další mezery, ale rozlišovat ho netřeba - přepne se to
            // do stejné sekce a druhý výskyt ji jen potvrdí.
            if t.starts_with("I) Language codes") {
                sec = Sec::Lang;
                continue;
            } else if t.starts_with("II) Country codes") {
                sec = Sec::Country;
                continue;
            } else if t.starts_with("III) Target-area codes") {
                sec = Sec::Target;
                continue;
            } else if t.starts_with("IV) Transmitter") {
                sec = Sec::None;
                continue;
            }
            if sec == Sec::None || t.is_empty() {
                continue;
            }
            // Kódy jsou odsazené; neodsazené řádky jsou vysvětlující odstavce.
            if !line.starts_with("   ") {
                continue;
            }
            let Some((code, rest)) = t.split_once(char::is_whitespace) else {
                continue;
            };
            let mut name = rest.trim();
            // Cílové oblasti se píší jako "Af  - Africa".
            if let Some(r) = name.strip_prefix("- ") {
                name = r.trim();
            }
            // U jazyků je vzadu ISO kód v hranatých závorkách.
            if let Some(i) = name.rfind(" [") {
                if name.ends_with(']') {
                    name = name[..i].trim();
                }
            }
            // Hvězdička u zemí znamená "není samostatný stát" - nezajímá nás.
            let name = name.trim_end_matches(" *").trim();
            if code.is_empty() || name.is_empty() {
                continue;
            }
            let map = match sec {
                Sec::Lang => &mut c.languages,
                Sec::Country => &mut c.countries,
                Sec::Target => &mut c.targets,
                Sec::None => continue,
            };
            map.entry(code.to_string()).or_insert_with(|| name.to_string());
        }
        c
    }
}

pub struct Schedule {
    pub entries: Vec<Entry>,
    pub season: String,
    pub codes: Codes,
}

/// Datum ve tvaru DDMM, jak ho píše EiBi ("2903" = 29. března).
/// V poli stop bývá připojené datum posledního logu v hranatých závorkách.
fn parse_ddmm(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let s = s.split('[').next()?.trim();
    if s.len() != 4 {
        return None;
    }
    let d: u32 = s[0..2].parse().ok()?;
    let m: u32 = s[2..4].parse().ok()?;
    if !(1..=31).contains(&d) || !(1..=12).contains(&m) {
        return None;
    }
    Some((m, d))
}

fn parse_hhmm(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.len() != 4 {
        return None;
    }
    s.parse::<u16>().ok()
}

impl Schedule {
    /// Rozpis bez vysvětlivek. Používají to testy; aplikace jde přes
    /// `parse_with_codes`.
    #[cfg(test)]
    pub fn parse(text: &str, season: &str) -> Schedule {
        Self::parse_with_codes(text, season, Codes::default())
    }

    pub fn parse_with_codes(text: &str, season: &str, codes: Codes) -> Schedule {
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
            let persistence = f
                .get(8)
                .and_then(|s| s.trim().parse::<u8>().ok())
                .unwrap_or(0);
            // EiBi tyhle záznamy sám nezařazuje do svých výpisů - nevysílá se.
            if persistence == 8 {
                continue;
            }
            entries.push(Entry {
                freq_khz: freq,
                start,
                end,
                days: DaySet::parse(f[2]),
                country: f[3].trim().to_string(),
                station: f[4].trim().to_string(),
                language: f[5].trim().to_string(),
                target: f[6].trim().to_string(),
                site: f.get(7).map(|s| s.trim().to_string()).unwrap_or_default(),
                persistence,
                from_date: f.get(9).and_then(|s| parse_ddmm(s)),
                to_date: f.get(10).and_then(|s| parse_ddmm(s)),
            });
        }
        entries.sort_by(|a, b| a.freq_khz.partial_cmp(&b.freq_khz).unwrap());
        Schedule {
            entries,
            season: season.to_string(),
            codes,
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
        let (month, dom) = (now.month(), now.day());
        let mut v: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| {
                (e.freq_khz - freq_khz).abs() <= tolerance_khz
                    && e.active_at(hhmm, day)
                    && e.active_on_date(month, dom)
            })
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

/// Kam s cache. Na Windows neexistuje HOME ani XDG.
fn cache_base() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }
}

fn cache_dir() -> Option<PathBuf> {
    Some(cache_base()?.join("knoflik-sdr"))
}

fn cache_path(season: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("eibi-{season}.csv")))
}

/// EiBi posílá CSV v Latin-1, ne v UTF-8 - jsou v něm jména jako
/// "Rádio Clube do Pará". Prvních 256 znaků Unicode je shodných s Latin-1,
/// takže převod je prosté rozšíření bajtu na znak. Bez toho by se soubor
/// buď vůbec nepřečetl, nebo by se z diakritiky staly otazníky.
fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Stáhne soubor z eibispace.de a převede z Latin-1.
fn fetch(name: &str) -> Result<String> {
    let url = format!("http://www.eibispace.de/dx/{name}");
    let mut body = Vec::new();
    ureq::get(&url)
        .call()
        .map_err(|e| anyhow!("stažení {url} selhalo: {e}"))?
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|e| anyhow!("čtení odpovědi selhalo: {e}"))?;
    Ok(latin1_to_string(&body))
}

/// Vysvětlivky zkratek. Bez nich se dá žít, tak se chyba nehlásí -
/// ukáže se prostě holý kód.
fn load_codes() -> Codes {
    let path = cache_dir().map(|d| d.join("eibi-readme.txt"));
    if let Some(p) = &path {
        if let Ok(text) = std::fs::read_to_string(p) {
            let c = Codes::parse(&text);
            if !c.countries.is_empty() {
                return c;
            }
        }
    }
    match fetch("README.TXT") {
        Ok(text) => {
            if let Some(p) = &path {
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(p, &text);
            }
            Codes::parse(&text)
        }
        Err(_) => Codes::default(),
    }
}

/// Načte rozpis z cache, a když tam není, stáhne ho.
pub fn load_or_fetch() -> Result<Schedule> {
    let season = current_season();
    let path = cache_path(&season).ok_or_else(|| anyhow!("nelze určit adresář cache"))?;

    // V cache je už UTF-8, protože ho tam ukládáme převedený.
    if let Ok(text) = std::fs::read_to_string(&path) {
        let s = Schedule::parse_with_codes(&text, &season, load_codes());
        if !s.entries.is_empty() {
            return Ok(s);
        }
    }

    let text = fetch(&format!("sked-{season}.csv"))?;
    let s = Schedule::parse_with_codes(&text, &season, load_codes());
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

        println!(
            "vysvětlivky: {} zemí, {} jazyků, {} cílových oblastí",
            s.codes.countries.len(),
            s.codes.languages.len(),
            s.codes.targets.len()
        );
        assert!(s.codes.countries.len() > 200, "málo zemí: {}", s.codes.countries.len());
        // Pozor: EiBi píše "Brasil", ne "Brazil".
        for (k, v) in [("CHN", "China"), ("B", "Brasil"), ("D", "Germany"), ("G", "United")] {
            let got = s.codes.country(k).unwrap_or("???");
            println!("  {k:4} -> {got}");
            assert!(got.contains(v), "{k} přeloženo jako {got}");
        }
        // Kolik kódů zemí z rozpisu umíme vysvětlit?
        let total = s.entries.len();
        let znamych = s
            .entries
            .iter()
            .filter(|e| e.country.is_empty() || s.codes.country(&e.country).is_some())
            .count();
        println!("vysvětlených zemí u záznamů: {znamych}/{total}");
        assert!(znamych * 100 / total > 95, "moc nevysvětlených zemí");

        let now = Utc::now();
        println!("teď je {} UTC", now.format("%a %H:%M"));
        for f in [6060.0, 7300.0, 9400.0, 5900.0] {
            let v = s.lookup(f, 2.0);
            println!("  {f:.0} kHz -> {} stanic teď na vysílání", v.len());
            for e in v.iter().take(3) {
                println!(
                    "      {} | {} · {} | {}-{} | vysílač {}",
                    e.station,
                    s.codes.country(&e.country).unwrap_or("?"),
                    s.codes.language_short(&e.language).unwrap_or("?"),
                    e.start,
                    e.end,
                    if e.site.is_empty() { "?" } else { &e.site }
                );
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

    /// EiBi zapisuje dny i čísly: 1 = pondělí.
    #[test]
    fn ciselne_dny() {
        let d = DaySet::parse("156");
        assert!(d.contains(0), "1 = pondělí");
        assert!(d.contains(4), "5 = pátek");
        assert!(d.contains(5), "6 = sobota");
        assert!(!d.contains(1) && !d.contains(6));
    }

    /// Persistence 8 znamená neaktivní záznam - EiBi ho sám do svých
    /// výpisů nedává, takže se nesmí ukazovat ani u nás.
    #[test]
    fn neaktivni_zaznamy_se_zahodi() {
        let csv = "h\n                   6060;1600-1700;;CHN;Aktivni;E;SEA;k;1;;\n                   6060;1600-1700;;CHN;Neaktivni;E;SEA;k;8;;\n";
        let s = Schedule::parse(csv, "a26");
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].station, "Aktivni");
    }

    #[test]
    fn datum_ddmm() {
        assert_eq!(parse_ddmm("2903"), Some((3, 29)), "29. března");
        assert_eq!(parse_ddmm("0109"), Some((9, 1)), "1. září");
        // V poli stop bývá připojené datum logu.
        assert_eq!(parse_ddmm("2106[0626]"), Some((6, 21)));
        assert_eq!(parse_ddmm(""), None);
        assert_eq!(parse_ddmm("9999"), None, "99. měsíc neexistuje");
    }

    /// Záznam s persistence 6 platí jen mezi svými daty.
    #[test]
    fn cast_sezony_plati_jen_ve_svem_obdobi() {
        let csv = "h\n6060;0000-2400;;X;Letni;E;Eu;k;6;0106;3108;\n";
        let s = Schedule::parse(csv, "a26");
        let e = &s.entries[0];
        assert!(e.active_on_date(7, 15), "15. července je uvnitř 1.6.-31.8.");
        assert!(!e.active_on_date(12, 1), "1. prosince ne");
        assert!(e.active_on_date(6, 1), "hranice včetně");
    }

    #[test]
    fn cast_sezony_pres_konec_roku() {
        let csv = "h\n6060;0000-2400;;X;Zimni;E;Eu;k;6;0112;3101;\n";
        let s = Schedule::parse(csv, "a26");
        let e = &s.entries[0];
        assert!(e.active_on_date(12, 15), "prosinec");
        assert!(e.active_on_date(1, 15), "leden");
        assert!(!e.active_on_date(7, 15), "červenec ne");
    }

    /// Bez persistence 6 se datumy neuplatňují, i kdyby tam byly.
    #[test]
    fn jina_persistence_datum_neresi() {
        let csv = "h\n6060;0000-2400;;X;Stala;E;Eu;k;1;0106;3108;\n";
        let s = Schedule::parse(csv, "a26");
        assert!(s.entries[0].active_on_date(12, 1));
    }

    /// Vysílání z cizího vysílače se pozná podle předpony "/ABC".
    #[test]
    fn relay_pozna_hostitelskou_zemi() {
        let mk = |site: &str| Entry {
            site: site.into(),
            ..Default::default()
        };
        assert_eq!(mk("/OMA-a").relay_country(), Some("OMA"), "BBC přes Omán");
        assert_eq!(mk("/MDG").relay_country(), Some("MDG"), "i bez kódu místa");
        assert_eq!(mk("/D-n").relay_country(), Some("D"));
        // Domácí vysílač předponu nemá.
        assert_eq!(mk("am").relay_country(), None);
        assert_eq!(mk("").relay_country(), None);
    }

    #[test]
    fn kratky_nazev_jazyka() {
        let readme = "   I) Language codes.\n\n                         E     English: UK (60m), USA (225m)   [eng]\n                         A     Arabic (300m)                   [ara]\n                         -CW   Morse Station\n\n   II) Country codes.\n";
        let c = Codes::parse(readme);
        assert_eq!(c.language_short("E"), Some("English"), "výčet zemí pryč");
        assert_eq!(c.language_short("A"), Some("Arabic"), "počet mluvčích pryč");
        assert_eq!(c.language_short("-CW"), Some("Morse Station"));
    }

    #[test]
    fn kody_se_rozeberou_ze_readme() {
        let readme = "\
nejaky uvod
   I) Language codes.

   E     English (400m)                                        [eng]
   TW    Taiwanese/Fujian (CHN 25m)                            [nan]

   II) Country codes.

   B    Brazil
   CHN  China (People's Republic)
   CLA  Clandestine stations *
   D    Germany

   III) Target-area codes.
   Af  - Africa
   SEA - Southeast Asia

   IV) Transmitter site codes.
   CHN: a-Baoji-Xinjie
";
        let c = Codes::parse(readme);
        assert_eq!(c.country("CHN"), Some("China (People's Republic)"));
        assert_eq!(c.country("B"), Some("Brazil"), "jednopísmenné kódy jsou ty nejmatoucí");
        assert_eq!(c.country("D"), Some("Germany"));
        // Hvězdička značí "není samostatný stát" - do názvu nepatří.
        assert_eq!(c.country("CLA"), Some("Clandestine stations"));
        // ISO kód v závorce se má useknout.
        assert_eq!(c.language("E"), Some("English (400m)"));
        assert_eq!(c.target("SEA"), Some("Southeast Asia"));
        // Sekce IV nesmí protéct do zemí.
        assert!(c.country("CHN:").is_none(), "sekce vysílačů se nemá číst");
    }

    #[test]
    fn neznamy_kod_vrati_nic() {
        let c = Codes::default();
        assert!(c.country("XYZ").is_none());
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
