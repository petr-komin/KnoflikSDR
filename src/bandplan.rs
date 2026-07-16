//! Bandplan pro IARU Region 1 (Evropa) - jen HF úseky, které SoftRock pokrývá.
//!
//! Vlastní tabulka, ne převzatá z Quisku. Slouží k obarvení panoramatu,
//! ne k dodržování předpisů - pro vysílání si vždycky ověř aktuální bandplan.

/// Druh provozu na úseku pásma. Barva je zvolená tak, aby se dala
/// rozeznat i jako slabé podbarvení pod signálem.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Usage {
    Cw,
    Digital,
    Phone,
    Beacon,
    Broadcast,
}

impl Usage {
    pub fn label(&self) -> &'static str {
        match self {
            Usage::Cw => "CW",
            Usage::Digital => "digi",
            Usage::Phone => "fonie",
            Usage::Beacon => "majáky",
            Usage::Broadcast => "rozhlas",
        }
    }

    /// (r, g, b) - průhlednost si řeší vykreslování.
    pub fn color(&self) -> (u8, u8, u8) {
        match self {
            Usage::Cw => (90, 150, 230),
            Usage::Digital => (200, 120, 220),
            Usage::Phone => (90, 200, 120),
            Usage::Beacon => (230, 180, 70),
            Usage::Broadcast => (230, 130, 90),
        }
    }
}

/// Úsek pásma. Meze jsou v kHz.
pub struct Segment {
    pub from_khz: f64,
    pub to_khz: f64,
    pub usage: Usage,
    /// Název pásma pro popisek, např. "40 m".
    pub band: &'static str,
}

const fn seg(from_khz: f64, to_khz: f64, usage: Usage, band: &'static str) -> Segment {
    Segment {
        from_khz,
        to_khz,
        usage,
        band,
    }
}

/// Amatérská pásma a hlavní rozhlasové úseky na KV.
/// Řazeno vzestupně podle frekvence.
pub const SEGMENTS: &[Segment] = &[
    // 160 m
    seg(1810.0, 1838.0, Usage::Cw, "160 m"),
    seg(1838.0, 1843.0, Usage::Digital, "160 m"),
    seg(1843.0, 2000.0, Usage::Phone, "160 m"),
    // 80 m
    seg(3500.0, 3570.0, Usage::Cw, "80 m"),
    seg(3570.0, 3600.0, Usage::Digital, "80 m"),
    seg(3600.0, 3800.0, Usage::Phone, "80 m"),
    // 49 m rozhlas
    seg(5900.0, 6200.0, Usage::Broadcast, "49 m"),
    // 60 m
    seg(5351.5, 5354.0, Usage::Cw, "60 m"),
    seg(5354.0, 5366.0, Usage::Phone, "60 m"),
    // 40 m
    seg(7000.0, 7040.0, Usage::Cw, "40 m"),
    seg(7040.0, 7050.0, Usage::Digital, "40 m"),
    seg(7050.0, 7200.0, Usage::Phone, "40 m"),
    // 41 m rozhlas
    seg(7200.0, 7450.0, Usage::Broadcast, "41 m"),
    // 31 m rozhlas
    seg(9400.0, 9900.0, Usage::Broadcast, "31 m"),
    // 30 m
    seg(10100.0, 10130.0, Usage::Cw, "30 m"),
    seg(10130.0, 10150.0, Usage::Digital, "30 m"),
    // 25 m rozhlas
    seg(11600.0, 12100.0, Usage::Broadcast, "25 m"),
    // 20 m
    seg(14000.0, 14070.0, Usage::Cw, "20 m"),
    seg(14070.0, 14099.0, Usage::Digital, "20 m"),
    seg(14099.0, 14101.0, Usage::Beacon, "20 m"),
    seg(14101.0, 14350.0, Usage::Phone, "20 m"),
    // 19 m rozhlas
    seg(15100.0, 15800.0, Usage::Broadcast, "19 m"),
    // 17 m
    seg(18068.0, 18095.0, Usage::Cw, "17 m"),
    seg(18095.0, 18109.0, Usage::Digital, "17 m"),
    seg(18109.0, 18111.0, Usage::Beacon, "17 m"),
    seg(18111.0, 18168.0, Usage::Phone, "17 m"),
    // 15 m
    seg(21000.0, 21070.0, Usage::Cw, "15 m"),
    seg(21070.0, 21149.0, Usage::Digital, "15 m"),
    seg(21149.0, 21151.0, Usage::Beacon, "15 m"),
    seg(21151.0, 21450.0, Usage::Phone, "15 m"),
    // 13 m rozhlas
    seg(21450.0, 21850.0, Usage::Broadcast, "13 m"),
    // 12 m
    seg(24890.0, 24915.0, Usage::Cw, "12 m"),
    seg(24915.0, 24929.0, Usage::Digital, "12 m"),
    seg(24929.0, 24931.0, Usage::Beacon, "12 m"),
    seg(24931.0, 24990.0, Usage::Phone, "12 m"),
    // 10 m
    seg(28000.0, 28070.0, Usage::Cw, "10 m"),
    seg(28070.0, 28190.0, Usage::Digital, "10 m"),
    seg(28190.0, 28225.0, Usage::Beacon, "10 m"),
    seg(28225.0, 29700.0, Usage::Phone, "10 m"),
];

/// Úseky, které zasahují do rozsahu `from_khz`..`to_khz`.
pub fn overlapping(from_khz: f64, to_khz: f64) -> impl Iterator<Item = &'static Segment> {
    SEGMENTS
        .iter()
        .filter(move |s| s.to_khz > from_khz && s.from_khz < to_khz)
}

/// Úsek, do kterého frekvence spadá (pokud nějaký).
pub fn at(khz: f64) -> Option<&'static Segment> {
    SEGMENTS
        .iter()
        .find(|s| khz >= s.from_khz && khz < s.to_khz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmenty_jsou_serazene_a_platne() {
        for s in SEGMENTS {
            assert!(
                s.from_khz < s.to_khz,
                "{} {}: {} >= {}",
                s.band,
                s.usage.label(),
                s.from_khz,
                s.to_khz
            );
        }
    }

    #[test]
    fn nasel_spravny_usek() {
        // Střed fonie na 40 m.
        let s = at(7100.0).expect("7100 kHz má být v bandplanu");
        assert_eq!(s.usage, Usage::Phone);
        assert_eq!(s.band, "40 m");
        // CW konec 20 m.
        assert_eq!(at(14010.0).unwrap().usage, Usage::Cw);
        // 41 m rozhlas.
        assert_eq!(at(7300.0).unwrap().usage, Usage::Broadcast);
    }

    #[test]
    fn mimo_pasma_nic() {
        assert!(at(6800.0).is_none(), "6800 kHz není v žádném úseku");
    }

    #[test]
    fn preryv_najde_vic_useku() {
        // Okno 96 kHz kolem 7050 zasahuje do CW, digi i fonie na 40 m.
        let v: Vec<_> = overlapping(7000.0, 7100.0).collect();
        assert!(v.len() >= 3, "našlo jen {} úseků", v.len());
    }
}
