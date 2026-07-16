//! Dekodéry digitálních režimů: RTTY a CW.
//!
//! Oba pracují nad **komplexním základním pásmem za kanálovým filtrem**,
//! ne nad zvukem. Odpadá tím dohadování se o konvencích zvukových
//! frekvencí (mark 2125 Hz apod.) a hlavně AGC, která by dekodéru
//! rozhoupala úrovně pod rukama.

use num_complex::Complex32;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Decoder {
    #[default]
    Off,
    Rtty,
    Cw,
}

impl Decoder {
    pub fn label(&self) -> &'static str {
        match self {
            Decoder::Off => "vypnuto",
            Decoder::Rtty => "RTTY",
            Decoder::Cw => "CW",
        }
    }
}

// ─────────────────────────── Baudot / ITA2 ───────────────────────────

const LTRS: u8 = 0x1F;
const FIGS: u8 = 0x1B;

/// Písmenová sada ITA2. '\0' = řídicí znak, řeší se zvlášť.
const BAUDOT_LTRS: [char; 32] = [
    '\0', 'E', '\n', 'A', ' ', 'S', 'I', 'U', '\r', 'D', 'R', 'J', 'N', 'F', 'C', 'K', 'T', 'Z',
    'L', 'W', 'H', 'Y', 'P', 'Q', 'O', 'B', 'G', '\0', 'M', 'X', 'V', '\0',
];

/// Číslicová sada, americká varianta - ta se na pásmech používá.
const BAUDOT_FIGS: [char; 32] = [
    '\0', '3', '\n', '-', ' ', '\'', '8', '7', '\r', '$', '4', '\'', ',', '!', ':', '(', '5', '"',
    ')', '2', '#', '6', '0', '1', '9', '?', '&', '\0', '.', '/', ';', '\0',
];

/// Stav dekódování Baudotu: přepínání písmena/číslice je stavové.
struct BaudotState {
    figs: bool,
}

impl BaudotState {
    fn new() -> Self {
        BaudotState { figs: false }
    }

    fn decode(&mut self, code: u8) -> Option<char> {
        let code = code & 0x1F;
        match code {
            LTRS => {
                self.figs = false;
                None
            }
            FIGS => {
                self.figs = true;
                None
            }
            _ => {
                let c = if self.figs {
                    BAUDOT_FIGS[code as usize]
                } else {
                    BAUDOT_LTRS[code as usize]
                };
                if c == '\0' { None } else { Some(c) }
            }
        }
    }
}

// ─────────────────────────────── RTTY ────────────────────────────────

/// Parametry RTTY. Výchozí hodnoty jsou amatérský standard.
#[derive(Clone, Copy, Debug)]
pub struct RttyConfig {
    pub baud: f64,
    /// Rozestup mark/space v Hz.
    pub shift_hz: f64,
    /// Prohodit mark a space (v éteru se běžně vyskytuje obojí).
    pub reverse: bool,
}

impl Default for RttyConfig {
    fn default() -> Self {
        RttyConfig {
            baud: 45.45,
            shift_hz: 170.0,
            reverse: false,
        }
    }
}

/// Jednopólová dolní propust pro vyhlazení obálky tónu.
struct Smooth {
    y: f32,
    a: f32,
}

impl Smooth {
    fn new(cutoff_hz: f64, rate: f64) -> Self {
        Smooth {
            y: 0.0,
            a: 1.0 - (-2.0 * PI * cutoff_hz / rate).exp() as f32,
        }
    }
    #[inline]
    fn push(&mut self, x: f32) -> f32 {
        self.y += (x - self.y) * self.a;
        self.y
    }
}

/// Komplexní směšovač na pevnou frekvenci - vytáhne jeden tón.
struct ToneFilter {
    phase: f64,
    step: f64,
    i: Smooth,
    q: Smooth,
}

impl ToneFilter {
    fn new(freq_hz: f64, rate: f64, bw_hz: f64) -> Self {
        ToneFilter {
            phase: 0.0,
            step: -2.0 * PI * freq_hz / rate,
            i: Smooth::new(bw_hz, rate),
            q: Smooth::new(bw_hz, rate),
        }
    }

    /// Vrátí okamžitou úroveň daného tónu.
    #[inline]
    fn push(&mut self, z: Complex32) -> f32 {
        let (s, c) = self.phase.sin_cos();
        self.phase += self.step;
        if self.phase < -PI {
            self.phase += 2.0 * PI;
        }
        let m = z * Complex32::new(c as f32, s as f32);
        let i = self.i.push(m.re);
        let q = self.q.push(m.im);
        (i * i + q * q).sqrt()
    }
}

pub struct RttyDecoder {
    cfg: RttyConfig,
    mark: ToneFilter,
    space: ToneFilter,
    /// Vzorků na jeden bit.
    samples_per_bit: f64,
    /// Přijímáme zrovna znak?
    rx: bool,
    /// Vzorků od začátku start bitu.
    n: f64,
    /// Který bit čekáme: 0 = start, 1..=5 = data, 6 = stop.
    next_bit: usize,
    bits: Vec<bool>,
    prev_mark: bool,
    baudot: BaudotState,
}

impl RttyDecoder {
    pub fn new(rate: f64, cfg: RttyConfig) -> Self {
        // Filtry tónů jsou úzké - šířka zhruba baudová rychlost.
        let half = cfg.shift_hz / 2.0;
        let (fm, fs) = if cfg.reverse {
            (-half, half)
        } else {
            (half, -half)
        };
        RttyDecoder {
            cfg,
            mark: ToneFilter::new(fm, rate, cfg.baud),
            space: ToneFilter::new(fs, rate, cfg.baud),
            samples_per_bit: rate / cfg.baud,
            rx: false,
            n: 0.0,
            next_bit: 0,
            bits: Vec::with_capacity(5),
            prev_mark: true,
            baudot: BaudotState::new(),
        }
    }

    pub fn config(&self) -> RttyConfig {
        self.cfg
    }

    /// Zpracuje vzorek; když dojde ke znaku, vrátí ho.
    ///
    /// Bity se odečítají uprostřed, tedy v čase `(i + 0.5) * vzorků_na_bit`
    /// od sestupné hrany start bitu - tam je rozhodnutí nejjistější.
    pub fn push(&mut self, z: Complex32) -> Option<char> {
        let is_mark = self.mark.push(z) > self.space.push(z);
        let was_mark = self.prev_mark;
        self.prev_mark = is_mark;

        if !self.rx {
            // Klid je mark; start bit se ohlásí sestupnou hranou.
            if was_mark && !is_mark {
                self.rx = true;
                self.n = 0.0;
                self.next_bit = 0;
                self.bits.clear();
            }
            return None;
        }

        self.n += 1.0;
        let stred = self.samples_per_bit * (self.next_bit as f64 + 0.5);
        if self.n < stred {
            return None;
        }

        match self.next_bit {
            0 => {
                // Start bit musí být space, jinak to byl jen šum na hraně.
                if is_mark {
                    self.rx = false;
                    return None;
                }
            }
            1..=5 => self.bits.push(is_mark),
            _ => {
                // Za daty musí následovat stop bit (mark), jinak znak zahodíme.
                self.rx = false;
                if is_mark && self.bits.len() == 5 {
                    // Baudot jde LSB napřed.
                    let code = self
                        .bits
                        .iter()
                        .enumerate()
                        .fold(0u8, |acc, (i, &b)| if b { acc | (1 << i) } else { acc });
                    return self.baudot.decode(code);
                }
                return None;
            }
        }
        self.next_bit += 1;
        None
    }
}

// ──────────────────────────────── CW ─────────────────────────────────

/// Morseovka: (znak, kód). Tečka = '.', čárka = '-'.
const MORSE: &[(char, &str)] = &[
    ('A', ".-"),
    ('B', "-..."),
    ('C', "-.-."),
    ('D', "-.."),
    ('E', "."),
    ('F', "..-."),
    ('G', "--."),
    ('H', "...."),
    ('I', ".."),
    ('J', ".---"),
    ('K', "-.-"),
    ('L', ".-.."),
    ('M', "--"),
    ('N', "-."),
    ('O', "---"),
    ('P', ".--."),
    ('Q', "--.-"),
    ('R', ".-."),
    ('S', "..."),
    ('T', "-"),
    ('U', "..-"),
    ('V', "...-"),
    ('W', ".--"),
    ('X', "-..-"),
    ('Y', "-.--"),
    ('Z', "--.."),
    ('0', "-----"),
    ('1', ".----"),
    ('2', "..---"),
    ('3', "...--"),
    ('4', "....-"),
    ('5', "....."),
    ('6', "-...."),
    ('7', "--..."),
    ('8', "---.."),
    ('9', "----."),
    ('/', "-..-."),
    ('?', "..--.."),
    ('.', ".-.-.-"),
    (',', "--..--"),
    ('=', "-...-"),
    ('+', ".-.-."),
    ('-', "-....-"),
];

fn morse_to_char(code: &str) -> Option<char> {
    MORSE.iter().find(|(_, m)| *m == code).map(|(c, _)| *c)
}

/// Dekodér CW s automatickým odhadem tempa.
///
/// Délku tečky si odvozuje z nejkratších značek, které vidí - operátoři
/// nedodržují tempo přesně a předepsané WPM by bylo k ničemu.
pub struct CwDecoder {
    rate: f64,
    env: Smooth,
    /// Klouzavá špička a dno obálky pro plovoucí práh.
    peak: f32,
    floor: f32,
    on: bool,
    run: f64,
    /// Odhad délky tečky ve vzorcích.
    dit: f64,
    code: String,
    /// Jak dlouho je ticho, kvůli mezerám mezi znaky a slovy.
    silence: f64,
    emitted_space: bool,
}

impl CwDecoder {
    pub fn new(rate: f64) -> Self {
        CwDecoder {
            rate,
            env: Smooth::new(50.0, rate),
            peak: 0.0,
            floor: 0.0,
            on: false,
            run: 0.0,
            // Výchozí odhad ~20 WPM (tečka 60 ms).
            dit: rate * 0.06,
            code: String::new(),
            silence: 0.0,
            emitted_space: true,
        }
    }

    /// Aktuální odhad tempa ve slovech za minutu.
    pub fn wpm(&self) -> f64 {
        // PARIS: 1 WPM = tečka 1200 ms.
        1200.0 / (self.dit / self.rate * 1000.0).max(1.0)
    }

    pub fn push(&mut self, z: Complex32) -> Option<char> {
        let e = self.env.push(z.norm());

        // Plovoucí práh: špička rychle nahoru, pomalu dolů; dno naopak.
        if e > self.peak {
            self.peak = e;
        } else {
            self.peak += (e - self.peak) * 0.0002;
        }
        if e < self.floor {
            self.floor = e;
        } else {
            self.floor += (e - self.floor) * 0.00002;
        }
        let span = (self.peak - self.floor).max(1e-6);
        // Hystereze, ať šum nepřeklápí klíč.
        let hi = self.floor + span * 0.6;
        let lo = self.floor + span * 0.4;

        let was = self.on;
        if self.on && e < lo {
            self.on = false;
        } else if !self.on && e > hi {
            self.on = true;
        }

        let mut out = None;
        if self.on != was {
            if self.on {
                // Konec ticha - délka mezery rozhodne o hranici znaku/slova.
                if self.run > self.dit * 2.0 {
                    out = self.flush();
                }
                self.run = 0.0;
            } else {
                // Konec značky - tečka, nebo čárka?
                if self.run < self.dit * 2.0 {
                    self.code.push('.');
                    // Doladíme odhad tečky podle skutečnosti.
                    self.dit = self.dit * 0.8 + self.run * 0.2;
                } else {
                    self.code.push('-');
                    self.dit = self.dit * 0.9 + (self.run / 3.0) * 0.1;
                }
                self.dit = self.dit.clamp(self.rate * 0.01, self.rate * 0.3);
                self.run = 0.0;
            }
            self.silence = 0.0;
            self.emitted_space = false;
        }
        self.run += 1.0;

        if !self.on {
            self.silence += 1.0;
            // Mezera mezi znaky ~3 tečky, mezi slovy ~7.
            if out.is_none() && !self.code.is_empty() && self.silence > self.dit * 2.5 {
                out = self.flush();
            } else if self.code.is_empty() && !self.emitted_space && self.silence > self.dit * 6.0 {
                self.emitted_space = true;
                return Some(' ');
            }
        }
        out
    }

    fn flush(&mut self) -> Option<char> {
        if self.code.is_empty() {
            return None;
        }
        let c = morse_to_char(&self.code);
        self.code.clear();
        // Neznámou sekvenci ohlásíme, ať je vidět, že tam něco bylo.
        Some(c.unwrap_or('¿'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 8000.0;

    /// Vyrobí RTTY signál z textu: pro každý znak start bit, 5 datových
    /// bitů LSB napřed a stop bit.
    fn make_rtty(codes: &[u8], cfg: RttyConfig) -> Vec<Complex32> {
        let spb = RATE / cfg.baud;
        let half = cfg.shift_hz / 2.0;
        let mut out = Vec::new();
        let mut phase = 0.0f64;
        let mut emit = |mark: bool, n: usize, out: &mut Vec<Complex32>, phase: &mut f64| {
            let f = if mark == !cfg.reverse { half } else { -half };
            for _ in 0..n {
                *phase += 2.0 * PI * f / RATE;
                out.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
            }
        };
        // Náběh v klidovém stavu (mark).
        emit(true, spb as usize * 3, &mut out, &mut phase);
        for &code in codes {
            emit(false, spb as usize, &mut out, &mut phase); // start
            for i in 0..5 {
                emit(code & (1 << i) != 0, spb as usize, &mut out, &mut phase);
            }
            emit(true, (spb * 1.5) as usize, &mut out, &mut phase); // stop
        }
        emit(true, spb as usize * 3, &mut out, &mut phase);
        out
    }

    #[test]
    fn rtty_dekoduje_text() {
        // "RY" je klasická zkušební sekvence, střídá krajní bitové vzory.
        let cfg = RttyConfig::default();
        let codes = [0x0A, 0x15, 0x0A, 0x15]; // R Y R Y
        let sig = make_rtty(&codes, cfg);
        let mut d = RttyDecoder::new(RATE, cfg);
        let text: String = sig.iter().filter_map(|&z| d.push(z)).collect();
        assert_eq!(text, "RYRY", "dekódováno {text:?}");
    }

    #[test]
    fn rtty_prepina_pismena_a_cislice() {
        let cfg = RttyConfig::default();
        // FIGS, "1" (0x17), LTRS, "A" (0x03)
        let codes = [FIGS, 0x17, LTRS, 0x03];
        let sig = make_rtty(&codes, cfg);
        let mut d = RttyDecoder::new(RATE, cfg);
        let text: String = sig.iter().filter_map(|&z| d.push(z)).collect();
        assert_eq!(text, "1A", "dekódováno {text:?}");
    }

    #[test]
    fn rtty_reverse_je_potreba_spravne() {
        // Signál vyrobený obráceně se s normálním nastavením nemá dekódovat.
        let normal = RttyConfig::default();
        let rev = RttyConfig {
            reverse: true,
            ..normal
        };
        let sig = make_rtty(&[0x0A, 0x15], rev);
        let mut d = RttyDecoder::new(RATE, normal);
        let text: String = sig.iter().filter_map(|&z| d.push(z)).collect();
        assert_ne!(text, "RY", "obrácený signál se neměl přečíst správně");

        // A se správným nastavením ano.
        let mut d = RttyDecoder::new(RATE, rev);
        let text: String = sig.iter().filter_map(|&z| d.push(z)).collect();
        assert_eq!(text, "RY", "dekódováno {text:?}");
    }

    /// Vyrobí CW signál: tón o dané frekvenci klíčovaný podle morseovky.
    fn make_cw(text: &str, wpm: f64) -> Vec<Complex32> {
        let dit = (1.2 / wpm * RATE) as usize;
        let mut out = Vec::new();
        let mut phase = 0.0f64;
        let mut emit = |on: bool, n: usize, out: &mut Vec<Complex32>, phase: &mut f64| {
            for _ in 0..n {
                *phase += 2.0 * PI * 700.0 / RATE;
                let a = if on { 1.0 } else { 0.0 };
                out.push(Complex32::new(
                    (a * phase.cos()) as f32,
                    (a * phase.sin()) as f32,
                ));
            }
        };
        emit(false, dit * 8, &mut out, &mut phase);
        for (ci, ch) in text.chars().enumerate() {
            if ch == ' ' {
                emit(false, dit * 4, &mut out, &mut phase);
                continue;
            }
            if ci > 0 {
                emit(false, dit * 3, &mut out, &mut phase);
            }
            let code = MORSE.iter().find(|(c, _)| *c == ch).unwrap().1;
            for (i, sym) in code.chars().enumerate() {
                if i > 0 {
                    emit(false, dit, &mut out, &mut phase);
                }
                emit(true, if sym == '.' { dit } else { dit * 3 }, &mut out, &mut phase);
            }
        }
        emit(false, dit * 10, &mut out, &mut phase);
        out
    }

    #[test]
    fn cw_dekoduje_volacku() {
        let sig = make_cw("CQ DE OK1ABC", 20.0);
        let mut d = CwDecoder::new(RATE);
        let text: String = sig.iter().filter_map(|&z| d.push(z)).collect();
        let text = text.trim().to_string();
        assert_eq!(text, "CQ DE OK1ABC", "dekódováno {text:?}");
    }

    #[test]
    fn cw_zvlada_i_jine_tempo() {
        // Dekodér si musí tempo odvodit sám, ne spoléhat na přednastavení.
        for wpm in [12.0, 25.0] {
            let sig = make_cw("TEST", wpm);
            let mut d = CwDecoder::new(RATE);
            let text: String = sig.iter().filter_map(|&z| d.push(z)).collect();
            assert_eq!(text.trim(), "TEST", "při {wpm} WPM dekódováno {text:?}");
        }
    }

    #[test]
    fn baudot_zna_obe_sady() {
        let mut b = BaudotState::new();
        assert_eq!(b.decode(0x03), Some('A'));
        assert_eq!(b.decode(FIGS), None);
        assert_eq!(b.decode(0x17), Some('1'));
        assert_eq!(b.decode(LTRS), None);
        assert_eq!(b.decode(0x03), Some('A'));
    }
}
