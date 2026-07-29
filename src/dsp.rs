//! DSP řetězec pro AM, SSB a CW příjem z I/Q.
//!
//! 96 kHz I/Q -> směšovač na offset -> antialiasingová propust + decimace /2
//! -> 48 kHz -> kanálový filtr -> detektor podle režimu -> odstranění DC
//! -> AGC -> 48 kHz audio.
//!
//! Filtr je záměrně až za decimací. Na čtvrtinové vzorkovačce je při stejném
//! počtu koeficientů čtyřikrát ostřejší, takže jde udělat i 150 Hz CW filtr;
//! před decimací by přechodové pásmo bylo širší než celá propust.
//!
//! Kanálový filtr má komplexní koeficienty, takže může být nesymetrický kolem
//! nosné - přesně to dělá z I/Q jednopásmový příjem: propustíme jen jednu
//! stranu spektra a reálná složka výsledku je rovnou zvuk.

use crate::decode::{CwDecoder, Decoder, RttyConfig, RttyDecoder};
use crate::rds::{RdsDecoder, RdsInfo};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Nejmíň koeficientů antialiasingové propusti. Na malé decimaci (SoftRock,
/// 96 kHz -> 48 kHz) je propust široká a tolik jich bohatě stačí.
const PRE_TAPS_MIN: usize = 127;
/// Strop, ať se to nezvrhne u nesmyslně velké decimace.
const PRE_TAPS_MAX: usize = 1535;

/// Kolik koeficientů potřebuje antialiasingová propust před decimací.
///
/// Přechodové pásmo FIR je zhruba `3,3 × vstupní_rychlost / počet_koeficientů`
/// a musí se vejít mezi konec propusti (0,45 × výstupní) a první alias
/// (0,55 × výstupní), tedy do 0,1 × výstupní. Z toho vyjde počet koeficientů
/// úměrný decimaci - na 96 kHz vstupu (decim 2) stačí desítky, na 1,344 MSps
/// z RSP1 (decim 28) je potřeba přes tisíc, jinak by se alias složil do zvuku.
///
/// Platit se za to nemusí: [`FirDecim`] počítá skalární součin jen na výstupním
/// vzorku, takže cena je `koeficienty × výstupní_rychlost` bez ohledu na vstupní.
fn pre_taps(decim: usize) -> usize {
    let n = (40 * decim).clamp(PRE_TAPS_MIN, PRE_TAPS_MAX);
    n | 1 // liché, ať je FIR symetrický kolem středu
}
/// Koeficienty kanálového filtru. Ten běží až za decimací, tedy na čtvrtinové
/// vzorkovačce - proto při stejném počtu koeficientů vyjde přechodové pásmo
/// mnohem užší a dá se dělat pořádný CW filtr.
const CHAN_TAPS: usize = 1023;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Am,
    Usb,
    Lsb,
    Cw,
    /// Širokopásmová FM - rozhlas na VKV. Jiný řetězec než ostatní: kanál je
    /// ~180 kHz široký, demoduluje se frekvenčním diskriminátorem. Dává smysl
    /// jen na širokopásmovém vstupu (RSP1), na krátkovlnném SoftRocku ne.
    Wfm,
    /// Úzkopásmová FM - amatérský provoz na 2 m/70 cm, kanál ~16 kHz. Vejde se
    /// do stejného řetězce jako AM/SSB, jen za kanálovým filtrem je diskriminátor.
    Nfm,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Am => "AM",
            Mode::Usb => "USB",
            Mode::Lsb => "LSB",
            Mode::Cw => "CW",
            Mode::Wfm => "WFM",
            Mode::Nfm => "NFM",
        }
    }

    pub fn is_ssb(&self) -> bool {
        matches!(self, Mode::Usb | Mode::Lsb)
    }

    /// Širokopásmová FM potřebuje vlastní demodulační cestu.
    pub fn is_wfm(&self) -> bool {
        matches!(self, Mode::Wfm)
    }
}

/// Výška tónu CW na výstupu. Filtr je centrovaný na nosnou, takže se
/// pípání musí vyrobit až tady - stejně jako BFO v klasickém přijímači.
pub const CW_PITCH_HZ: f64 = 700.0;

/// Číslicově řízený oscilátor. Fázi držíme v f64, sin/cos při 96 kHz
/// je zanedbatelná zátěž a nehrozí kumulace chyby jako u inkrementální rotace.
pub struct Nco {
    phase: f64,
    step: f64,
}

impl Nco {
    pub fn new() -> Self {
        Nco {
            phase: 0.0,
            step: 0.0,
        }
    }

    /// Kladné `freq_hz` posune signál na dané frekvenci dolů na DC.
    pub fn set_freq(&mut self, freq_hz: f64, sample_rate: f64) {
        self.step = -2.0 * PI * freq_hz / sample_rate;
    }

    #[inline]
    pub fn next(&mut self) -> Complex32 {
        let (s, c) = self.phase.sin_cos();
        self.phase += self.step;
        if self.phase > PI {
            self.phase -= 2.0 * PI;
        } else if self.phase < -PI {
            self.phase += 2.0 * PI;
        }
        Complex32::new(c as f32, s as f32)
    }
}

/// Návrh FIR dolní propusti oknem (sinc * Hann), normalizovaný na jednotkový zisk v DC.
pub fn lowpass_taps(cutoff_hz: f64, sample_rate: f64, n: usize) -> Vec<f32> {
    let fc = cutoff_hz / sample_rate;
    let m = (n - 1) as f64;
    let mut taps: Vec<f32> = (0..n)
        .map(|i| {
            let k = i as f64 - m / 2.0;
            let sinc = if k.abs() < 1e-9 {
                2.0 * fc
            } else {
                (2.0 * PI * fc * k).sin() / (PI * k)
            };
            let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / m).cos();
            (sinc * w) as f32
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    if sum.abs() > 1e-12 {
        taps.iter_mut().for_each(|t| *t /= sum);
    }
    taps
}

/// Koeficienty filtru pro daný režim a šířku pásma.
///
/// AM potřebuje propust symetrickou kolem nosné (+-bw/2), SSB jen jednu
/// stranu (0..+bw pro USB, -bw..0 pro LSB). Jednopásmový filtr vznikne
/// frekvenčním posunem dolní propusti, čímž se koeficienty stanou komplexní.
pub fn filter_taps(mode: Mode, bandwidth_hz: f64, sample_rate: f64, n: usize) -> Vec<Complex32> {
    let proto = lowpass_taps(bandwidth_hz / 2.0, sample_rate, n);
    let shift = match mode {
        // AM, CW i NFM jsou symetrické kolem nosné. WFM sem nechodí (má vlastní
        // řetězec), ale ať je match úplný, chová se jako symetrický.
        Mode::Am | Mode::Cw | Mode::Wfm | Mode::Nfm => 0.0,
        Mode::Usb => bandwidth_hz / 2.0,
        Mode::Lsb => -bandwidth_hz / 2.0,
    };
    let mid = (n - 1) as f64 / 2.0;
    proto
        .iter()
        .enumerate()
        .map(|(k, &h)| {
            let ph = 2.0 * PI * shift * (k as f64 - mid) / sample_rate;
            Complex32::new(h * ph.cos() as f32, h * ph.sin() as f32)
        })
        .collect()
}

/// Komplexní FIR s decimací. Historie v kruhovém bufferu o velikosti
/// mocniny dvou, aby se indexovalo maskou místo dělení.
pub struct FirDecim {
    taps: Vec<Complex32>,
    hist: Vec<Complex32>,
    mask: usize,
    idx: usize,
    pub decim: usize,
    phase: usize,
}

impl FirDecim {
    pub fn new(taps: Vec<Complex32>, decim: usize) -> Self {
        let size = taps.len().next_power_of_two();
        FirDecim {
            taps,
            hist: vec![Complex32::new(0.0, 0.0); size],
            mask: size - 1,
            idx: 0,
            decim,
            phase: 0,
        }
    }

    /// Vymění koeficienty za jiné o stejné délce. Historie zůstává,
    /// takže změna šířky pásma nebo režimu za běhu necvakne.
    pub fn set_taps(&mut self, taps: Vec<Complex32>) {
        debug_assert_eq!(taps.len(), self.taps.len());
        if taps.len() == self.taps.len() {
            self.taps = taps;
        }
    }

    /// Vloží vzorek; vrátí výstup jen každý `decim`-tý vzorek.
    #[inline]
    pub fn push(&mut self, x: Complex32) -> Option<Complex32> {
        self.hist[self.idx] = x;
        self.idx = (self.idx + 1) & self.mask;
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        let mut acc = Complex32::new(0.0, 0.0);
        for (k, &t) in self.taps.iter().enumerate() {
            let i = self.idx.wrapping_sub(1 + k) & self.mask;
            acc += self.hist[i] * t;
        }
        Some(acc)
    }
}

/// FIR s decimací pro **reálný** signál.
///
/// Za diskriminátorem je zvuk reálný, ale [`FirDecim`] by ho hnal přes
/// komplexní násobení - tedy čtyřikrát víc práce, než je potřeba, a to na
/// nejdražším místě řetězce. U WFM jsou takové filtry dva (součtový
/// a rozdílový kanál), takže se to sečte.
pub struct FirDecimReal {
    taps: Vec<f32>,
    hist: Vec<f32>,
    mask: usize,
    idx: usize,
    decim: usize,
    phase: usize,
}

impl FirDecimReal {
    pub fn new(taps: Vec<f32>, decim: usize) -> Self {
        let size = taps.len().next_power_of_two();
        FirDecimReal {
            taps,
            hist: vec![0.0; size],
            mask: size - 1,
            idx: 0,
            decim,
            phase: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, x: f32) -> Option<f32> {
        self.hist[self.idx] = x;
        self.idx = (self.idx + 1) & self.mask;
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        let mut acc = 0.0;
        for (k, &t) in self.taps.iter().enumerate() {
            let i = self.idx.wrapping_sub(1 + k) & self.mask;
            acc += self.hist[i] * t;
        }
        Some(acc)
    }
}

/// Odstranění stejnosměrné složky: y[n] = x[n] - x[n-1] + r*y[n-1].
/// U AM tím zmizí nosná a zůstane modulace.
pub struct DcBlock {
    x1: f32,
    y1: f32,
    r: f32,
}

impl DcBlock {
    pub fn new(r: f32) -> Self {
        DcBlock {
            x1: 0.0,
            y1: 0.0,
            r,
        }
    }

    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + self.r * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// Jak rychle AGC pouští zisk zpátky nahoru. Náběh je vždycky rychlý (5 ms),
/// ať silný signál nepráskne do sluchátek; liší se jen uvolnění.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AgcMode {
    /// ~100 ms - na CW a pileup, kde má být slyšet každý znak zvlášť.
    Fast,
    /// ~500 ms - všeobecný kompromis.
    #[default]
    Medium,
    /// ~2 s - na SSB a AM, kde rychlé uvolnění pumpuje šum mezi slovy.
    Slow,
    /// AGC vypnutá, zisk se řídí ručně. Na silné stanice, kde AGC jen
    /// vytahuje šum v mezerách.
    Off,
}

impl AgcMode {
    pub const ALL: [AgcMode; 4] = [AgcMode::Fast, AgcMode::Medium, AgcMode::Slow, AgcMode::Off];

    pub fn label(&self) -> &'static str {
        match self {
            AgcMode::Fast => "rychlá",
            AgcMode::Medium => "střední",
            AgcMode::Slow => "pomalá",
            AgcMode::Off => "vypnutá",
        }
    }

    /// Časová konstanta uvolnění v sekundách. U vypnuté AGC nemá smysl,
    /// vrací se hodnota jen proto, aby šel koeficient spočítat jednotně.
    fn decay_s(&self) -> f32 {
        match self {
            AgcMode::Fast => 0.100,
            AgcMode::Medium => 0.500,
            AgcMode::Slow => 2.000,
            AgcMode::Off => 0.500,
        }
    }
}

/// Jednoduchá AGC s rychlým náběhem a nastavitelným uvolněním.
///
/// Obálku (`env`) sleduje **i když je AGC vypnutá** - visí na ní S-metr
/// i šumová brána, takže kdyby se přestala počítat, přestaly by obě fungovat.
pub struct Agc {
    env: f32,
    target: f32,
    attack: f32,
    decay: f32,
    max_gain: f32,
    mode: AgcMode,
    /// Ruční zisk pro `AgcMode::Off` (lineární činitel).
    manual_gain: f32,
    sample_rate: f32,
}

impl Agc {
    pub fn new(sample_rate: f32) -> Self {
        Agc {
            env: 0.0,
            target: 0.25,
            // ~5 ms náběh
            attack: 1.0 - (-1.0 / (0.005 * sample_rate)).exp(),
            decay: 1.0 - (-1.0 / (AgcMode::default().decay_s() * sample_rate)).exp(),
            max_gain: 500.0,
            mode: AgcMode::default(),
            manual_gain: 1.0,
            sample_rate,
        }
    }

    /// Přepne režim AGC a nastaví ruční zisk v dB (uplatní se jen u `Off`).
    pub fn set_mode(&mut self, mode: AgcMode, manual_gain_db: f32) {
        if mode != self.mode {
            self.mode = mode;
            self.decay = 1.0 - (-1.0 / (mode.decay_s() * self.sample_rate)).exp();
        }
        self.manual_gain = 10f32.powf(manual_gain_db / 20.0);
    }

    /// Obálka signálu před regulací - přesně to, co má ukazovat S-metr.
    pub fn envelope(&self) -> f32 {
        self.env
    }

    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        let a = x.abs();
        let coef = if a > self.env { self.attack } else { self.decay };
        self.env += (a - self.env) * coef;
        // Obálka se počítá vždycky (drží ji S-metr i squelch), regulace ne.
        if self.mode == AgcMode::Off {
            return (x * self.manual_gain).clamp(-1.0, 1.0);
        }
        let g = if self.env > 1e-9 {
            (self.target / self.env).min(self.max_gain)
        } else {
            1.0
        };
        (x * g).clamp(-1.0, 1.0)
    }
}

/// Biquad podle RBJ Cookbook. Používá se na vytažení pilotu 19 kHz
/// a na RDS podnosnou; koeficienty jsou už poděleny a0.
#[derive(Clone, Copy)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Pásmová propust s jednotkovým ziskem ve špičce.
    pub fn bandpass(f0: f64, q: f64, fs: f64) -> Self {
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (alpha / a0) as f32,
            b1: 0.0,
            b2: (-alpha / a0) as f32,
            a1: (-2.0 * w0.cos() / a0) as f32,
            a2: ((1.0 - alpha) / a0) as f32,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Dolní propust.
    pub fn lowpass(f0: f64, q: f64, fs: f64) -> Self {
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let cos_w0 = w0.cos();
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (((1.0 - cos_w0) / 2.0) / a0) as f32,
            b1: ((1.0 - cos_w0) / a0) as f32,
            b2: (((1.0 - cos_w0) / 2.0) / a0) as f32,
            a1: (-2.0 * cos_w0 / a0) as f32,
            a2: ((1.0 - alpha) / a0) as f32,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Meze ručního notche v Hz (audio pásmo, kde heterodyn píská).
pub const NOTCH_MIN_HZ: f64 = 100.0;
pub const NOTCH_MAX_HZ: f64 = 5_000.0;
/// Jakost notche. Vysoká = úzký zásek, který sebere pískot a hlas nechá být;
/// při 30 je zádrž na 1 kHz široká zhruba 33 Hz.
const NOTCH_Q: f64 = 30.0;

/// Úzká pásmová zádrž (biquad podle RBJ Cookbook) na audiu.
///
/// Na KV je nejotravnější vada heterodynní pískot od nosné sousední stanice.
/// Zádrž běží **před AGC** schválně: kdyby byla až za ní, silný pískot by
/// mezitím stáhl zisk a užitečný signál by zůstal potichu i po odstranění tónu.
pub struct Notch {
    /// Koeficienty normalizované na a0; `None` = vypnuto.
    coefs: Option<[f32; 5]>,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
    sample_rate: f64,
    freq_hz: f64,
}

impl Notch {
    pub fn new(sample_rate: f64) -> Self {
        Notch {
            coefs: None,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
            sample_rate,
            freq_hz: 0.0,
        }
    }

    /// Nastaví kmitočet zádrže v Hz, nebo `None` pro vypnutí.
    pub fn set_freq(&mut self, hz: Option<f64>) {
        match hz {
            Some(f) => {
                // Přepočítávat koeficienty na každém vzorku nemá smysl; jen
                // když se kmitočet opravdu hnul.
                if self.coefs.is_some() && (f - self.freq_hz).abs() < 0.5 {
                    return;
                }
                self.freq_hz = f;
                let w0 = 2.0 * PI * f / self.sample_rate;
                let alpha = w0.sin() / (2.0 * NOTCH_Q);
                let (cos_w0, a0) = (w0.cos(), 1.0 + alpha);
                // b0, b1, b2, a1, a2 - už poděleno a0.
                self.coefs = Some([
                    (1.0 / a0) as f32,
                    (-2.0 * cos_w0 / a0) as f32,
                    (1.0 / a0) as f32,
                    (-2.0 * cos_w0 / a0) as f32,
                    ((1.0 - alpha) / a0) as f32,
                ]);
            }
            None => self.coefs = None,
        }
    }

    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        let Some([b0, b1, b2, a1, a2]) = self.coefs else {
            return x;
        };
        let y = b0 * x + b1 * self.x1 + b2 * self.x2 - a1 * self.y1 - a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Hystereze squelche v dB. Otevře se o kus výš, než zavře, ať brána na
/// hraně prahu nekmitá, když signál kolísá kolem něj.
const SQUELCH_HYST_DB: f32 = 2.0;

/// Šumová brána. Umlčí zvuk, když síla naladěného signálu klesne pod práh -
/// jinak by mezi stanicemi jen syčel šum do sluchátek. Práh je v dBFS, tedy
/// ve stejné stupnici, jakou ukazuje S-metr (`Demod::level_dbfs`), takže čára
/// v panoramatu sedí na to, co metr měří.
///
/// Rozhoduje se z obálky před AGC (té samé, ze které se bere S-metr), ne z
/// hlasitosti po AGC - ta by slabý šum vytáhla na úroveň signálu a brána by
/// nikdy nezavřela.
pub struct Squelch {
    /// Práh otevření a zavření v lineární amplitudě (dBFS převedené na 10^(dB/20)).
    /// `None` = squelch vypnutý, zvuk projde vždy.
    open_lin: Option<f32>,
    close_lin: f32,
    /// Aktuální zisk brány 0..1. Mění se plynule, aby přechod necvakl.
    gain: f32,
    /// Přírůstek zisku na jeden vzorek (náběh i doběh).
    ramp: f32,
    /// Je brána otevřená? Drží se přes hysterezi.
    open: bool,
}

impl Squelch {
    pub fn new(sample_rate: f32) -> Self {
        Squelch {
            open_lin: None,
            close_lin: 0.0,
            gain: 1.0,
            // ~10 ms náběh/doběh - dost rychlé, aby neukously začátek slova,
            // a dost pomalé, aby otevření/zavření nelusklo.
            ramp: 1.0 / (0.010 * sample_rate),
            open: true,
        }
    }

    /// Nastaví práh v dBFS, nebo `None` pro vypnutí. Práh se převádí na
    /// lineární amplitudu, ať se v `gate` neloguje na každém vzorku.
    pub fn set_threshold(&mut self, db: Option<f32>) {
        match db {
            Some(thr) => {
                self.open_lin = Some(10f32.powf((thr + SQUELCH_HYST_DB / 2.0) / 20.0));
                self.close_lin = 10f32.powf((thr - SQUELCH_HYST_DB / 2.0) / 20.0);
            }
            None => {
                self.open_lin = None;
                // Vypnutý squelch nechá bránu dojet naplno otevřenou.
                self.open = true;
            }
        }
    }

    /// Posune bránu o jeden vzorek podle síly signálu a vrátí zisk 0..1.
    ///
    /// Odděleno od [`Squelch::gate`] kvůli stereu: oba kanály musí dostat
    /// **týž** zisk, jinak by se při otevírání a zavírání rozjel stereo obraz.
    #[inline]
    pub fn gain_for(&mut self, level_lin: f32) -> f32 {
        if let Some(open_lin) = self.open_lin {
            if self.open {
                if level_lin < self.close_lin {
                    self.open = false;
                }
            } else if level_lin > open_lin {
                self.open = true;
            }
        }
        let target = if self.open { 1.0 } else { 0.0 };
        if self.gain < target {
            self.gain = (self.gain + self.ramp).min(target);
        } else if self.gain > target {
            self.gain = (self.gain - self.ramp).max(target);
        }
        self.gain
    }

    /// Aktualizuje bránu podle aktuální síly signálu (lineární obálka před AGC)
    /// a vrátí vzorek přiškrcený aktuálním ziskem brány.
    #[inline]
    pub fn gate(&mut self, level_lin: f32, x: f32) -> f32 {
        x * self.gain_for(level_lin)
    }
}

/// Šířka FM rozhlasového kanálu (Carson: 2×(75 kHz zdvih + 15 kHz audio)).
const FM_CHANNEL_HZ: f64 = 180_000.0;
/// Maximální zdvih FM rozhlasu - podle něj se diskriminátor normuje na ±1.
const FM_DEVIATION_HZ: f64 = 75_000.0;
/// Časová konstanta deemfáze pro Evropu (CCIR). USA má 75 µs.
const FM_DEEMPHASIS_S: f64 = 50e-6;
/// Nejvyšší modulační kmitočet FM rozhlasu - meze audio propusti.
const FM_AUDIO_HZ: f64 = 15_000.0;

/// Stereo pilot. Vysílač ho posílá na 19 kHz a podnosná s rozdílovým signálem
/// je přesně na jeho dvojnásobku a ve fázi s ním - proto se 38 kHz nemusí
/// hledat, stačí pilot vynásobit sám sebou.
const FM_PILOT_HZ: f64 = 19_000.0;
/// Jakost propusti na pilot. Úzko, ať do ní neleze okolní modulace.
const FM_PILOT_Q: f64 = 60.0;
/// Jak silný musí být pilot, aby se pustilo stereo. Vysílá se s ~9 % zdvihu,
/// tedy amplitudou kolem 0,09 po normalizaci diskriminátoru na ±1; pod 0,02
/// už jde spíš o šum a stereo by jen syčelo.
pub const FM_PILOT_LOCK: f32 = 0.02;
/// Kolikanásobek prahu drží už chycený pilot. Hystereze - jinak by stanice
/// na hraně překlápěla stereo sem a tam a lupalo by to.
const FM_PILOT_UNLOCK: f32 = 0.6;
/// Šířka smyčky fázového závěsu na pilot v Hz. Úzko: pilot je kmitočtově
/// stabilní, tak ať smyčku nerozhoupe šum a nosná zůstane čistá.
const FM_PLL_BW_HZ: f64 = 20.0;

/// Fázový závěs na stereo pilot.
///
/// Nosná 38 kHz i 57 kHz se dá vyrobit umocňováním pilotu (cos2θ = 2cos²θ−1),
/// jenže vstupní pilot je zašuměný a umocňování ten šum ještě znásobí -
/// do nosné se tím dostane fázový šum a RDS pak chybuje. Závěs místo toho
/// **nahradí** pilot vlastním čistým oscilátorem, který se na něj naladí;
/// násobky úhlu z něj pak vyjdou přesně.
struct PilotPll {
    /// Fáze a kmitočet oscilátoru v radiánech na vzorek.
    phase: f32,
    freq: f32,
    /// Klidový kmitočet, od kterého se smyčka nesmí utrhnout.
    nominal: f32,
    kp: f32,
    ki: f32,
    /// Koherentně měřená amplituda pilotu - průmět vstupu do fáze oscilátoru.
    /// Poctivější míra než obálka za propustí, protože šum mimo fázi se vyruší.
    amp: f32,
}

impl PilotPll {
    fn new(sample_rate: f64) -> Self {
        let nominal = 2.0 * PI * FM_PILOT_HZ / sample_rate;
        // Standardní návrh smyčky 2. řádu s tlumením 0,707.
        let wn = 2.0 * PI * FM_PLL_BW_HZ / sample_rate;
        PilotPll {
            phase: 0.0,
            freq: nominal as f32,
            nominal: nominal as f32,
            kp: (2.0 * 0.707 * wn) as f32,
            ki: (wn * wn) as f32,
            amp: 0.0,
        }
    }

    /// Posune oscilátor o vzorek podle vstupního pilotu a vrátí (cosθ, sinθ).
    #[inline]
    fn next(&mut self, pilot: f32) -> (f32, f32) {
        let (s, c) = self.phase.sin_cos();
        // Koherentní amplituda: průmět vstupu do fáze oscilátoru.
        self.amp += (pilot * c * 2.0 - self.amp) * 0.0005;
        // Fázový detektor. Dělením amplitudou je zisk smyčky nezávislý na
        // síle signálu, takže se chová stejně na slabé i silné stanici.
        let err = -pilot * s / self.amp.abs().max(1e-4);
        self.freq += self.ki * err;
        // Kmitočet držíme u klidové hodnoty, ať se smyčka nechytí na něčem jiném.
        let max_odchylka = self.nominal * 0.02;
        self.freq = self
            .freq
            .clamp(self.nominal - max_odchylka, self.nominal + max_odchylka);
        self.phase += self.freq + self.kp * err;
        if self.phase > PI as f32 {
            self.phase -= 2.0 * PI as f32;
        } else if self.phase < -(PI as f32) {
            self.phase += 2.0 * PI as f32;
        }
        (c, s)
    }
}

/// Typický zdvih amatérské úzkopásmové FM - normuje diskriminátor na ±1.
const NFM_DEVIATION_HZ: f64 = 5_000.0;
/// Meze audio propusti u NFM - lidský hlas, zbytek je jen syčení.
const NFM_AUDIO_HZ: f64 = 3_400.0;

/// Zvolí decimaci na mezifrekvenci pro WFM: vstup se nejdřív stáhne na
/// pásmo dost široké pro FM kanál (~200-400 kHz), tam se demoduluje, a teprve
/// pak jde zvuk dolů na 48 kHz. Vrací dělitele `total_decim`, který trefí
/// mezifrekvenci nejblíž cíli; obě decimace pak vyjdou celočíselně.
fn wfm_if_decim(in_rate: f64, total_decim: usize) -> usize {
    const TARGET_IF: f64 = 300_000.0;
    const MIN_IF: f64 = 200_000.0;
    let mut best = 1;
    let mut best_err = f64::MAX;
    for f in 1..=total_decim {
        if total_decim % f != 0 {
            continue;
        }
        let if_rate = in_rate / f as f64;
        if if_rate < MIN_IF {
            continue;
        }
        let err = (if_rate - TARGET_IF).abs();
        if err < best_err {
            best_err = err;
            best = f;
        }
    }
    best
}

/// Demodulátor širokopásmové FM. Vlastní řetězec, protože FM kanál je moc
/// široký, než aby se dal stáhnout rovnou na 48 kHz jako AM/SSB.
struct WfmDemod {
    /// Antialias + decimace na mezifrekvenci (široká, drží celý FM kanál).
    if_lp: FirDecim,
    /// Předchozí vzorek pro frekvenční diskriminátor.
    last: Complex32,
    /// Normalizace úhlu na ±1 při plném zdvihu.
    disc_gain: f32,
    /// Propust na stereo pilot 19 kHz a sledovač jeho amplitudy.
    pilot_bp: Biquad,
    pilot_env: f32,
    /// Fázový závěs na pilot. Z jeho fáze se vyrábí podnosná 38 kHz i nosná
    /// RDS 57 kHz - čistě, bez šumu, který by přineslo umocňování pilotu.
    pll: PilotPll,
    /// Přeje si uživatel stereo? Bez pilotu se stejně jede mono.
    stereo_wanted: bool,
    /// Je pilot dost silný na to, aby stereo dávalo smysl?
    pilot_locked: bool,
    /// Plynulé prolnutí mono (0) - stereo (1) a jeho krok na jeden audio vzorek.
    stereo_blend: f32,
    blend_step: f32,
    /// Součtová (L+R) a rozdílová (L-R) cesta: propust do 15 kHz + decimace
    /// na 48 kHz. Obě mají stejné koeficienty i decimaci, takže výstupy
    /// vypadávají zároveň a kanály se nerozejdou.
    sum_lp: FirDecimReal,
    diff_lp: FirDecimReal,
    /// Deemfáze až za oddělením kanálů - kdyby běžela na celém multiplexu,
    /// srazila by 38 kHz podnosnou a stereo by nebylo z čeho složit.
    deemph_l: f32,
    deemph_r: f32,
    deemph_a: f32,
    /// Odstranění DC z rozladění (funguje jako jemné AFC do sluchátek).
    dc_l: DcBlock,
    dc_r: DcBlock,
    /// Obálka mezifrekvence pro S-metr (FM má konstantní amplitudu).
    level: f32,
    /// Dekodér RDS. Bere multiplex na mezifrekvenci a nosnou 57 kHz
    /// odvozenou z pilotu.
    rds: RdsDecoder,
    /// Vstupní vzorkovačka, decimace a šířka kanálu - drží se kvůli přepočtu
    /// mezifrekvenční propusti, když se šířka změní za běhu.
    in_rate: f64,
    if_decim: usize,
    bandwidth_hz: f64,
}

impl WfmDemod {
    fn new(in_rate: f64, total_decim: usize) -> Self {
        let if_decim = wfm_if_decim(in_rate, total_decim).max(1);
        let if_rate = in_rate / if_decim as f64;
        let audio_decim = (total_decim / if_decim).max(1);

        // MF propust: propustí celý FM kanál a zahradí alias při decimaci.
        let if_cut = (FM_CHANNEL_HZ / 2.0).min(if_rate * 0.45);
        let if_taps: Vec<Complex32> = lowpass_taps(if_cut, in_rate, pre_taps(if_decim))
            .into_iter()
            .map(|h| Complex32::new(h, 0.0))
            .collect();

        // Audio propust: do 15 kHz, zahradí alias při decimaci na 48 kHz.
        // Za diskriminátorem je signál reálný, tak i filtr jede reálně.
        let audio_taps = lowpass_taps(FM_AUDIO_HZ, if_rate, pre_taps(audio_decim));

        // Deemfáze teď běží až na 48 kHz, za decimací.
        let audio_rate = if_rate / audio_decim as f64;
        let deemph_a = 1.0 - (-1.0 / (FM_DEEMPHASIS_S * audio_rate)).exp();
        WfmDemod {
            if_lp: FirDecim::new(if_taps, if_decim),
            last: Complex32::new(0.0, 0.0),
            disc_gain: (if_rate / (2.0 * std::f64::consts::PI * FM_DEVIATION_HZ)) as f32,
            pilot_bp: Biquad::bandpass(FM_PILOT_HZ, FM_PILOT_Q, if_rate),
            pilot_env: 0.0,
            pll: PilotPll::new(if_rate),
            stereo_wanted: true,
            pilot_locked: false,
            stereo_blend: 0.0,
            // ~30 ms přechod mezi mono a stereem.
            blend_step: (1.0 / (0.030 * audio_rate)) as f32,
            sum_lp: FirDecimReal::new(audio_taps.clone(), audio_decim),
            diff_lp: FirDecimReal::new(audio_taps, audio_decim),
            deemph_l: 0.0,
            deemph_r: 0.0,
            deemph_a: deemph_a as f32,
            dc_l: DcBlock::new(0.995),
            dc_r: DcBlock::new(0.995),
            level: 1e-9,
            rds: RdsDecoder::new(if_rate),
            in_rate,
            if_decim,
            bandwidth_hz: FM_CHANNEL_HZ,
        }
    }

    /// Vstup je už smíšený na offset NCO (stejně jako u ostatních režimů),
    /// takže sem chodí pásmo se stanicí kolem DC. `squelch` hradluje výstup
    /// stejně jako u ostatních režimů - u FM podle síly nosné na mezifrekvenci.
    ///
    /// Na výstup jdou prokládané dvojice L, R.
    fn process(&mut self, mixed: Complex32, out: &mut Vec<f32>, squelch: &mut Squelch) {
        let Some(z) = self.if_lp.push(mixed) else {
            return;
        };
        // S-metr: |z| je u FM konstantní, tak stačí pomalý sledovač.
        let mag = z.norm();
        self.level += (mag - self.level) * 0.001;

        // Frekvenční diskriminátor: úhel mezi po sobě jdoucími vzorky je
        // úměrný okamžité frekvenci, tedy modulaci. Výsledek je multiplex:
        // L+R v základním pásmu, pilot 19 kHz, L-R na 38 kHz, RDS na 57 kHz.
        let d = z * self.last.conj();
        self.last = z;
        let mpx = d.im.atan2(d.re) * self.disc_gain;

        // Pilot: úzká propust a na ni navěšený fázový závěs. Podnosná 38 kHz
        // je přesně dvojnásobek pilotu a ve fázi s ním, RDS pak trojnásobek -
        // z fáze oscilátoru tedy vyjdou oba přesně a bez šumu.
        let pilot = self.pilot_bp.push(mpx);
        let (c1, s1) = self.pll.next(pilot);
        let cos38 = 2.0 * c1 * c1 - 1.0;
        let sin38 = 2.0 * s1 * c1;
        // 57 kHz = 3θ, složeno z 2θ a θ: cos(a+b), sin(a+b).
        let cos57 = c1 * cos38 - s1 * sin38;
        let sin57 = s1 * cos38 + c1 * sin38;

        // Sílu pilotu bere z koherentní amplitudy závěsu: ta měří jen to, co je
        // s pilotem ve fázi, kdežto obálka za propustí počítá i šum kolem něj
        // a na slabé stanici by lhala nahoru.
        //
        // Rozhodnutí o stereu má hysterezi - u stanice na hraně by se jinak
        // překlápělo sem a tam a každé překlopení by luplo do sluchátek.
        self.pilot_env = self.pll.amp.abs();
        if self.pilot_locked {
            if self.pilot_env < FM_PILOT_LOCK * FM_PILOT_UNLOCK {
                self.pilot_locked = false;
            }
        } else if self.pilot_env > FM_PILOT_LOCK {
            self.pilot_locked = true;
        }
        let stereo = self.stereo_wanted && self.pilot_locked;

        // RDS jede vždycky, když je pilot - na hlasitosti ani na stereu
        // nezávisí a jeho text se hodí i u mono poslechu. Dostává obě fáze
        // nosné: skutečná podnosná je proti té odvozené z pilotu natočená
        // o neznámý úhel (skupinové zpoždění propustí, a norma navíc připouští
        // kvadraturu), takže si dekodér fázi musí dohledat sám.
        if self.pilot_locked {
            self.rds.push(mpx, cos57, sin57);
        }

        // Rozdílová složka: multiplex vynásobený podnosnou. Součin má užitečný
        // signál v základním pásmu a smetí kolem 76 kHz, které sundá propust.
        let diff_in = if stereo { 2.0 * mpx * cos38 } else { 0.0 };
        let sum_out = self.sum_lp.push(mpx);
        let diff_out = self.diff_lp.push(diff_in);

        let (Some(s), Some(dd)) = (sum_out, diff_out) else {
            return;
        };
        // Přechod mezi mono a stereem se prolíná, ne přepíná. Skokem by se
        // výstup posunul o celou rozdílovou složku naráz - a to je slyšet
        // jako lupnutí pokaždé, když pilot přeskočí přes práh.
        let cil = if stereo { 1.0 } else { 0.0 };
        if self.stereo_blend < cil {
            self.stereo_blend = (self.stereo_blend + self.blend_step).min(cil);
        } else if self.stereo_blend > cil {
            self.stereo_blend = (self.stereo_blend - self.blend_step).max(cil);
        }
        // L = (L+R) + (L−R), R = (L+R) − (L−R); při prolnutí se rozdílová
        // složka přimíchává postupně.
        let d_mix = dd * self.stereo_blend;
        let (l_raw, r_raw) = (s + d_mix, s - d_mix);
        // Deemfáze zvedne basy zpět po vysílačově preemfázi - každý kanál zvlášť.
        self.deemph_l += self.deemph_a * (l_raw - self.deemph_l);
        self.deemph_r += self.deemph_a * (r_raw - self.deemph_r);

        let l = self.dc_l.push(self.deemph_l);
        let r = self.dc_r.push(self.deemph_r);
        // Oba kanály musí dostat týž zisk brány, jinak by se stereo obraz
        // při otevírání a zavírání rozjel do strany.
        let g = squelch.gain_for(self.level);
        out.push(l * g);
        out.push(r * g);
    }

    fn level_dbfs(&self) -> f32 {
        20.0 * self.level.max(1e-9).log10()
    }

    /// Hraje se zrovna doopravdy stereo? (Uživatel ho chce a pilot je slyšet.)
    fn stereo_active(&self) -> bool {
        self.stereo_wanted && self.pilot_locked
    }

    /// Změní šířku mezifrekvenční propusti za běhu.
    ///
    /// Užší filtr zvládne sousední stanici, která přebíjí tu naladěnou; širší
    /// pustí plný zdvih bez zkreslení. Historie filtru zůstává, takže změna
    /// za poslechu necvakne. Užší než ~120 kHz už ale ořezává i vlastní
    /// signál - proto se pod to nedá jít, viz `bandwidth_range`.
    fn set_bandwidth(&mut self, bw_hz: f64) {
        if (bw_hz - self.bandwidth_hz).abs() < 1.0 {
            return;
        }
        self.bandwidth_hz = bw_hz;
        let if_rate = self.in_rate / self.if_decim as f64;
        let cut = (bw_hz / 2.0).min(if_rate * 0.45);
        let taps: Vec<Complex32> = lowpass_taps(cut, self.in_rate, pre_taps(self.if_decim))
            .into_iter()
            .map(|h| Complex32::new(h, 0.0))
            .collect();
        self.if_lp.set_taps(taps);
    }
}

/// Kompletní přijímač: I/Q dovnitř, mono audio ven.
pub struct Demod {
    nco: Nco,
    /// Antialiasingová propust + decimace na výstupní vzorkovačku.
    pre: FirDecim,
    /// Kanálový filtr, běží až za decimací (decim = 1).
    chan: FirDecim,
    /// BFO pro CW - vyrábí slyšitelný tón z nosné, která leží na DC.
    bfo: Nco,
    dc: DcBlock,
    agc: Agc,
    /// Šumová brána na výstupu. Společná pro všechny režimy.
    squelch: Squelch,
    /// Ruční zádrž na heterodyn, běží na audiu před AGC.
    notch: Notch,
    /// Širokopásmová FM. Vlastní řetězec s vlastní decimací; drží se pořád,
    /// zapojí se jen v režimu WFM.
    wfm: WfmDemod,
    /// Stav úzkopásmové FM (běží ve stejném řetězci jako AM/SSB).
    /// Předchozí vzorek diskriminátoru, stav a koeficient audio propusti,
    /// normalizace úhlu a obálka pro S-metr.
    nfm_last: Complex32,
    nfm_lp: f32,
    nfm_lp_a: f32,
    nfm_gain: f32,
    nfm_level: f32,
    in_rate: f64,
    offset_hz: f64,
    bandwidth_hz: f64,
    mode: Mode,
    /// Dekodér digitálních režimů. Bere komplexní pásmo za filtrem,
    /// tedy před detekcí i AGC.
    decoder: DecoderState,
    /// Co dekodér přečetl, než si to odtud někdo vyzvedne.
    decoded: String,
}

/// Běžící dekodér. Drží se stranou od `Decoder`, což je jen volba režimu.
enum DecoderState {
    Off,
    Rtty(Box<RttyDecoder>),
    Cw(Box<CwDecoder>),
}

impl DecoderState {
    fn kind(&self) -> Decoder {
        match self {
            DecoderState::Off => Decoder::Off,
            DecoderState::Rtty(_) => Decoder::Rtty,
            DecoderState::Cw(_) => Decoder::Cw,
        }
    }
}

impl Demod {
    pub fn new(in_rate: f64, decim: usize, bandwidth_hz: f64, mode: Mode) -> Self {
        let out_rate = in_rate / decim as f64;
        // Před decimací stačí zahradit alias: propust těsně pod Nyquistem
        // výstupní vzorkovačky. Tvarování kanálu dělá až druhý stupeň.
        let pre: Vec<Complex32> = lowpass_taps(out_rate * 0.45, in_rate, pre_taps(decim))
            .into_iter()
            .map(|h| Complex32::new(h, 0.0))
            .collect();
        let mut bfo = Nco::new();
        bfo.set_freq(-CW_PITCH_HZ, out_rate);
        Demod {
            nco: Nco::new(),
            pre: FirDecim::new(pre, decim),
            chan: FirDecim::new(filter_taps(mode, bandwidth_hz, out_rate, CHAN_TAPS), 1),
            bfo,
            dc: DcBlock::new(0.995),
            agc: Agc::new(out_rate as f32),
            squelch: Squelch::new(out_rate as f32),
            notch: Notch::new(out_rate),
            wfm: WfmDemod::new(in_rate, decim),
            nfm_last: Complex32::new(0.0, 0.0),
            nfm_lp: 0.0,
            nfm_lp_a: (1.0 - (-2.0 * std::f64::consts::PI * NFM_AUDIO_HZ / out_rate).exp()) as f32,
            nfm_gain: (out_rate / (2.0 * std::f64::consts::PI * NFM_DEVIATION_HZ)) as f32,
            nfm_level: 1e-9,
            in_rate,
            offset_hz: 0.0,
            bandwidth_hz,
            mode,
            decoder: DecoderState::Off,
            decoded: String::new(),
        }
    }

    /// Přepne dekodér. Rozdělaný znak se zahodí, což je při přepnutí v pořádku.
    pub fn set_decoder(&mut self, kind: Decoder, rtty: RttyConfig, squelch_db: f32) {
        let rate = self.out_rate();
        let same_rtty = match &self.decoder {
            DecoderState::Rtty(d) => {
                let c = d.config();
                c.reverse == rtty.reverse && c.baud == rtty.baud && c.shift_hz == rtty.shift_hz
            }
            _ => false,
        };
        if self.decoder.kind() == kind && (kind != Decoder::Rtty || same_rtty) {
            return;
        }
        self.decoder = match kind {
            Decoder::Off => DecoderState::Off,
            Decoder::Rtty => DecoderState::Rtty(Box::new(RttyDecoder::new(rate, rtty))),
            Decoder::Cw => DecoderState::Cw(Box::new(CwDecoder::new(rate, squelch_db))),
        };
    }

    fn out_rate(&self) -> f64 {
        self.in_rate / self.pre.decim as f64
    }

    /// Odhad tempa CW ve WPM, pokud zrovna běží CW dekodér.
    pub fn cw_wpm(&self) -> Option<f64> {
        match &self.decoder {
            DecoderState::Cw(d) => Some(d.wpm()),
            _ => None,
        }
    }

    /// Vyzvedne přečtený text a vyprázdní vnitřní zásobník.
    pub fn take_text(&mut self) -> String {
        std::mem::take(&mut self.decoded)
    }

    pub fn set_offset(&mut self, hz: f64) {
        if (hz - self.offset_hz).abs() > f64::EPSILON {
            self.offset_hz = hz;
            self.nco.set_freq(hz, self.in_rate);
        }
    }

    /// Změní šířku propustného pásma za běhu (přepočet koeficientů).
    ///
    /// WFM má vlastní řetězec, takže se šířka propisuje do jeho mezifrekvenční
    /// propusti; ostatní režimy ji mají v kanálovém filtru.
    pub fn set_bandwidth(&mut self, bw_hz: f64) {
        if self.mode.is_wfm() {
            self.wfm.set_bandwidth(bw_hz);
            return;
        }
        if (bw_hz - self.bandwidth_hz).abs() > 1.0 {
            self.bandwidth_hz = bw_hz;
            self.refresh_taps();
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if mode != self.mode {
            self.mode = mode;
            self.refresh_taps();
        }
    }

    /// Nastaví práh šumové brány v dBFS (stejná stupnice jako S-metr),
    /// nebo `None` pro vypnutí squelche.
    pub fn set_squelch(&mut self, db: Option<f32>) {
        self.squelch.set_threshold(db);
    }

    /// Přepne režim AGC. WFM se to netýká - ten má konstantní obálku, takže
    /// AGC v jeho řetězci není a hlasitost dělá zdvih, ne síla signálu.
    pub fn set_agc(&mut self, mode: AgcMode, manual_gain_db: f32) {
        self.agc.set_mode(mode, manual_gain_db);
    }

    /// Nastaví kmitočet ruční zádrže na audiu, nebo `None` pro vypnutí.
    pub fn set_notch(&mut self, hz: Option<f64>) {
        self.notch.set_freq(hz);
    }

    /// Přeje si uživatel stereo u WFM? Bez pilotu se stejně jede mono.
    pub fn set_stereo(&mut self, on: bool) {
        self.wfm.stereo_wanted = on;
    }

    /// Hraje se zrovna stereo? Jen ve WFM, a jen když je slyšet pilot.
    pub fn stereo_active(&self) -> bool {
        self.mode.is_wfm() && self.wfm.stereo_active()
    }

    /// Je chycený stereo pilot? Odděleně od [`Demod::stereo_active`] - ten
    /// navíc závisí na tom, jestli uživatel stereo vůbec chce, kdežto RDS
    /// jede z pilotu bez ohledu na to.
    pub fn pilot_locked(&self) -> bool {
        self.mode.is_wfm() && self.wfm.pilot_locked
    }

    /// Naměřená úroveň pilotu - přímo ta veličina, která se porovnává
    /// s [`FM_PILOT_LOCK`], ať jde číslo v GUI číst proti prahu.
    ///
    /// Na poctivé stereo stanici má vyjít kolem 0,09: pilot se vysílá
    /// s ~9 % zdvihu a závěs měří přímo jeho amplitudu.
    pub fn pilot_level(&self) -> f32 {
        self.wfm.pilot_env
    }

    /// Co zatím přečetl RDS. Prázdné, když stanice RDS nevysílá nebo je slabá.
    pub fn rds(&self) -> &RdsInfo {
        self.wfm.rds.info()
    }

    /// Kolik datových bloků RDS prošlo kontrolou a kolik ne.
    pub fn rds_blocks(&self) -> (u32, u32) {
        self.wfm.rds.block_stats()
    }

    /// Otevření oka RDS a kolikrát se chytala synchronizace - pro diagnostiku.
    pub fn rds_quality(&self) -> (f32, u32, bool) {
        let (n, drzi) = self.wfm.rds.sync_stats();
        (self.wfm.rds.eye(), n, drzi)
    }

    /// Úroveň naladěného signálu v dBFS (před AGC). Pro S-metr.
    pub fn level_dbfs(&self) -> f32 {
        match self.mode {
            // FM má konstantní obálku, tak S-metr bere sílu z mezifrekvence,
            // ne hlasitost audia (ta u FM se silou signálu nesouvisí).
            Mode::Wfm => self.wfm.level_dbfs(),
            Mode::Nfm => 20.0 * self.nfm_level.max(1e-9).log10(),
            _ => 20.0 * self.agc.envelope().max(1e-9).log10(),
        }
    }

    fn refresh_taps(&mut self) {
        let rate = self.out_rate();
        self.chan
            .set_taps(filter_taps(self.mode, self.bandwidth_hz, rate, CHAN_TAPS));
    }

    /// Zpracuje blok I/Q vzorků a připojí audio na konec `out`.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        // WFM má úplně jiný řetězec (široký kanál + diskriminátor), tak jde
        // vlastní cestou. Dekodéry a AGC se ho netýkají.
        if self.mode.is_wfm() {
            for &x in iq {
                let mixed = x * self.nco.next();
                self.wfm.process(mixed, out, &mut self.squelch);
            }
            return;
        }
        for &x in iq {
            let mixed = x * self.nco.next();
            // Stupeň 1: zahradit alias a decimovat. Stupeň 2: vytvarovat kanál
            // - na nižší vzorkovačce je stejný počet koeficientů mnohem ostřejší.
            let Some(decimated) = self.pre.push(mixed) else {
                continue;
            };
            if let Some(z) = self.chan.push(decimated) {
                // Dekodér dostane pásmo za filtrem, ale před detekcí a AGC -
                // AGC by mu rozhoupala úrovně pod rukama.
                match &mut self.decoder {
                    DecoderState::Off => {}
                    DecoderState::Rtty(d) => {
                        if let Some(c) = d.push(z) {
                            self.decoded.push(c);
                        }
                    }
                    DecoderState::Cw(d) => {
                        if let Some(c) = d.push(z) {
                            self.decoded.push(c);
                        }
                    }
                }
                let detected = match self.mode {
                    // AM: obálka komplexního signálu.
                    Mode::Am => z.norm(),
                    // SSB: filtr už nechal jen jednu stranu spektra, takže
                    // reálná složka je přímo zvuk.
                    Mode::Usb | Mode::Lsb => z.re,
                    // CW: nosná leží na DC, tak ji BFO posune na slyšitelný tón.
                    Mode::Cw => (z * self.bfo.next()).re,
                    // NFM: frekvenční diskriminátor na kanálově filtrovaném
                    // signálu, pak audio propust proti syčení.
                    Mode::Nfm => {
                        self.nfm_level += (z.norm() - self.nfm_level) * 0.01;
                        let d = z * self.nfm_last.conj();
                        self.nfm_last = z;
                        let raw = d.im.atan2(d.re) * self.nfm_gain;
                        self.nfm_lp += self.nfm_lp_a * (raw - self.nfm_lp);
                        self.nfm_lp
                    }
                    // WFM odbočuje na začátku process(), sem se nedostane.
                    Mode::Wfm => unreachable!("WFM má vlastní řetězec"),
                };
                // Zádrž na heterodyn ještě před AGC - jinak by pískot stáhl
                // zisk a po jeho odstranění by zbyl podregulovaný signál.
                let audio = self.notch.push(self.dc.push(detected));
                let s = self.agc.push(audio);
                // Squelch bere sílu z obálky před AGC - té samé, ze které se
                // počítá S-metr. NFM má vlastní obálku (nosná je konstantní),
                // ostatní režimy berou obálku AGC.
                let level = match self.mode {
                    Mode::Nfm => self.nfm_level,
                    _ => self.agc.envelope(),
                };
                // Výstup je vždycky prokládané stereo; mono režimy dají do
                // obou kanálů totéž. Díky tomu má zvuková cesta jediný tvar
                // a při přepnutí na WFM stereo se nemůže rozejít L a R.
                let g = self.squelch.gate(level, s);
                out.push(g);
                out.push(g);
            }
        }
    }
}

#[cfg(test)]
mod pre_taps_tests {
    use super::*;

    /// Antialiasingová propust musí na velké decimaci potlačit první alias.
    /// Kdyby `pre_taps` nerostlo s decimací, projde přes filtr signál z okolí
    /// a složí se do zvuku - a to je slyšet až v éteru, žádný test to jinak
    /// nechytí. Měříme přímo přenos filtru na kmitočtu prvního aliasu.
    fn utlum_db(taps: &[f32], f_hz: f64, rate: f64) -> f64 {
        // DTFT filtru na dané frekvenci.
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &h) in taps.iter().enumerate() {
            let w = -2.0 * PI * f_hz * n as f64 / rate;
            re += h as f64 * w.cos();
            im += h as f64 * w.sin();
        }
        20.0 * (re * re + im * im).sqrt().log10()
    }

    #[test]
    fn propust_potlaci_prvni_alias_i_pri_decimaci_28() {
        // RSP1: 1,344 MSps -> 48 kHz.
        let in_rate = 1_344_000.0;
        let decim = 28;
        let out_rate = in_rate / decim as f64;
        let taps = lowpass_taps(out_rate * 0.45, in_rate, pre_taps(decim));

        // V propustném pásmu nesmí nic ubrat...
        let propust = utlum_db(&taps, 5_000.0, in_rate);
        assert!(propust > -1.0, "propust na 5 kHz utlumena o {propust:.1} dB");

        // ...a první alias (co se složí na 0 Hz) musí zmizet.
        let alias = utlum_db(&taps, out_rate, in_rate);
        assert!(
            alias < -50.0,
            "alias na {out_rate} Hz utlumen jen o {alias:.1} dB - prolezl by do zvuku"
        );
    }

    /// SoftRock (96 kHz, decim 2) se nesmí zhoršit ani zdražit.
    #[test]
    fn softrock_zustava_na_minimu_koeficientu() {
        assert_eq!(pre_taps(2), PRE_TAPS_MIN);
        let in_rate = 96_000.0;
        let taps = lowpass_taps(48_000.0 * 0.45, in_rate, pre_taps(2));
        let alias = utlum_db(&taps, 48_000.0, in_rate);
        assert!(alias < -50.0, "alias utlumen jen o {alias:.1} dB");
    }

    #[test]
    fn pocet_koeficientu_je_vzdy_lichy() {
        for d in [1, 2, 3, 4, 28, 100, 1000] {
            assert_eq!(pre_taps(d) % 2, 1, "decim {d} dalo sudý počet");
        }
    }
}

#[cfg(test)]
mod squelch_tests {
    use super::*;

    /// Ustálený zisk brány po dost dlouhém přivádění dané úrovně.
    fn settle(sq: &mut Squelch, level_lin: f32) -> f32 {
        let mut g = 0.0;
        // 0,1 s na 48 kHz bohatě stačí na 10ms rampu.
        for _ in 0..4800 {
            g = sq.gate(level_lin, 1.0);
        }
        g
    }

    /// Nad prahem musí brána naplno otevřít, hluboko pod ním úplně zavřít.
    #[test]
    fn otevre_nad_prahem_zavre_pod_nim() {
        let mut sq = Squelch::new(48_000.0);
        sq.set_threshold(Some(-40.0)); // ~0,01 lineárně

        // Signál 20 dB nad prahem -> plně otevřeno.
        assert!(settle(&mut sq, 0.1) > 0.99, "silný signál má bránu otevřít");
        // Signál 20 dB pod prahem -> plně zavřeno (ticho do sluchátek).
        assert!(settle(&mut sq, 0.001) < 0.01, "slabý šum má bránu zavřít");
    }

    /// Vypnutý squelch nesmí zvuk nikdy přiškrtit, ani při nulové úrovni.
    #[test]
    fn vypnuty_squelch_pousti_vzdy() {
        let mut sq = Squelch::new(48_000.0);
        sq.set_threshold(None);
        assert!((settle(&mut sq, 0.0) - 1.0).abs() < 1e-6);
    }

    /// Hystereze: signál kousek nad prahem otevře, pak musí spadnout znatelně
    /// níž, aby zavřel - u samotného prahu brána nekmitá.
    #[test]
    fn hystereze_drzi_branu_stabilni() {
        let mut sq = Squelch::new(48_000.0);
        sq.set_threshold(Some(-40.0));
        let thr = 10f32.powf(-40.0 / 20.0); // lineární práh

        // Otevři silným signálem.
        assert!(settle(&mut sq, 10.0 * thr) > 0.99);
        // Přesně na prahu (uvnitř hystereze) musí zůstat otevřeno.
        assert!(settle(&mut sq, thr) > 0.99, "na prahu se nesmí zavřít");
    }
}

#[cfg(test)]
mod wfm_tests {
    use super::*;
    use std::f64::consts::PI;

    /// Vytáhne levý kanál z prokládaného stereo výstupu. Testy měří spektrum
    /// zvuku, a to by se na prokládaném proudu počítalo na dvojnásobné
    /// vzorkovačce - tón by pak vyšel na půlce binu, kde má být.
    fn levy_kanal(out: &[f32]) -> Vec<f32> {
        out.chunks_exact(2).map(|p| p[0]).collect()
    }

    /// Rozdělení decimace pro WFM musí dát mezifrekvenci v použitelném pásmu
    /// (200-400 kHz) a obě dílčí decimace musí vyjít celočíselně na 48 kHz.
    #[test]
    fn rozdeleni_decimace_je_smysluplne() {
        // RSP1 vzorkovačky a jejich celková decimace na 48 kHz.
        for (rate, total) in [
            (1_344_000.0, 28usize),
            (1_920_000.0, 40),
            (3_072_000.0, 64),
            (4_800_000.0, 100),
            (6_000_000.0, 125),
        ] {
            let ifd = wfm_if_decim(rate, total);
            assert!(total % ifd == 0, "IF decimace {ifd} nedělí {total}");
            let if_rate = rate / ifd as f64;
            assert!(
                (200_000.0..=450_000.0).contains(&if_rate),
                "IF {if_rate} Hz mimo použitelné pásmo (rate {rate})"
            );
            let audio_decim = total / ifd;
            assert_eq!(if_rate / audio_decim as f64, 48_000.0, "audio nevyjde na 48 kHz");
        }
    }

    /// Diskriminátor musí z FM tónu vytáhnout modulační kmitočet. Nasyntetizuji
    /// FM nosnou s jedním tónem a ověřím, že na výstupu je slyšet ten tón.
    #[test]
    fn diskriminator_demoduluje_ton() {
        let in_rate = 1_344_000.0;
        let decim = 28; // -> 48 kHz
        let mut wfm = WfmDemod::new(in_rate, decim);
        // Squelch pro tenhle test vypnutý - ověřujeme demodulaci, ne bránu.
        let mut sq = Squelch::new(48_000.0);

        let tone_hz = 1_000.0; // modulační tón
        let dev_hz = 40_000.0; // zdvih
        let n = 48_000 * 2; // 2 s zvuku po decimaci -> dost na FFT
        let samples = n * decim;

        let mut out: Vec<f32> = Vec::new();
        let mut phase = 0.0f64;
        let mut tphase = 0.0f64;
        for _ in 0..samples {
            // Okamžitá frekvence = zdvih * sin(tón).
            let f = dev_hz * (2.0 * PI * tphase).sin();
            phase += 2.0 * PI * f / in_rate;
            tphase += tone_hz / in_rate;
            let iq = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            wfm.process(iq, &mut out, &mut sq);
        }

        // Výstup je prokládané stereo, na spektrum se bere levý kanál.
        let out = levy_kanal(&out);
        assert!(out.len() > 4096, "málo audio vzorků: {}", out.len());
        // Ve spektru výstupu musí dominovat 1 kHz.
        let m = 4096.min(out.len() & !1);
        let mut buf: Vec<Complex32> =
            out[out.len() - m..].iter().map(|&s| Complex32::new(s, 0.0)).collect();
        rustfft::FftPlanner::new().plan_fft_forward(m).process(&mut buf);
        let bin_1k = (tone_hz / 48_000.0 * m as f64).round() as usize;
        let mag: Vec<f32> = buf[..m / 2].iter().map(|c| c.norm()).collect();
        let peak_bin = (0..m / 2).max_by(|&a, &b| mag[a].partial_cmp(&mag[b]).unwrap()).unwrap();
        assert!(
            (peak_bin as i64 - bin_1k as i64).abs() <= 2,
            "špička na binu {peak_bin}, čekal jsem kolem {bin_1k} (1 kHz)"
        );
    }

    /// NFM přes celý řetězec Demod: nasyntetizuji úzkopásmovou FM s tónem
    /// a ověřím, že na výstupu ten tón dominuje (tedy diskriminátor za
    /// kanálovým filtrem opravdu demoduluje).
    #[test]
    fn nfm_demoduluje_ton() {
        use num_complex::Complex32;
        let in_rate = 96_000.0; // jako SoftRock; NFM se vejde i sem
        let decim = 2; // -> 48 kHz
        let mut d = Demod::new(in_rate, decim, 16_000.0, Mode::Nfm);

        let tone_hz = 1_200.0;
        let dev_hz = 3_000.0;
        let n_audio = 48_000; // 1 s
        let samples = n_audio * decim;
        let mut iq = Vec::with_capacity(samples);
        let (mut ph, mut tph) = (0.0f64, 0.0f64);
        for _ in 0..samples {
            let f = dev_hz * (2.0 * std::f64::consts::PI * tph).sin();
            ph += 2.0 * std::f64::consts::PI * f / in_rate;
            tph += tone_hz / in_rate;
            iq.push(Complex32::new(ph.cos() as f32, ph.sin() as f32));
        }
        let mut out = Vec::new();
        d.process(&iq, &mut out);

        // Prokládané stereo - NFM dává do obou kanálů totéž.
        let out = levy_kanal(&out);
        assert!(out.len() > 8192, "málo audia: {}", out.len());
        let m = 8192;
        let mut buf: Vec<Complex32> =
            out[out.len() - m..].iter().map(|&s| Complex32::new(s, 0.0)).collect();
        rustfft::FftPlanner::new().plan_fft_forward(m).process(&mut buf);
        let bin_tone = (tone_hz / 48_000.0 * m as f64).round() as usize;
        let mag: Vec<f32> = buf[..m / 2].iter().map(|c| c.norm()).collect();
        let peak = (1..m / 2).max_by(|&a, &b| mag[a].partial_cmp(&mag[b]).unwrap()).unwrap();
        assert!(
            (peak as i64 - bin_tone as i64).abs() <= 2,
            "špička na binu {peak}, čekal jsem {bin_tone} (1,2 kHz)"
        );
    }

    /// Deemfáze běží až za decimací, na 48 kHz - kdyby zůstala na multiplexu
    /// před oddělením kanálů, srazila by 38 kHz podnosnou a stereo by nebylo
    /// z čeho složit. Koeficient tomu musí odpovídat.
    #[test]
    fn deemfaze_koeficient_odpovida_audio_rychlosti() {
        let wfm = WfmDemod::new(1_344_000.0, 28);
        // Pro 48 kHz a 50 µs: 1 - exp(-1/(50e-6 * 48000)) = 0,342.
        let cekano = 1.0 - (-1.0f32 / (FM_DEEMPHASIS_S as f32 * 48_000.0)).exp();
        assert!(
            (wfm.deemph_a - cekano).abs() < 1e-4,
            "a = {}, čekáno {cekano}",
            wfm.deemph_a
        );
    }

    /// Kolik procesoru sežere WFM řetězec na plné vzorkovačce RSP1.
    ///
    /// Tohle je to podstatné číslo: když demodulace nestíhá realtime, DSP
    /// vlákno odebírá z USB pomalu, libmirisdr začne hlásit "samples lost"
    /// a v éteru je to slyšet jako lupání. Musí zbýt rezerva i na FFT
    /// panoramatu a na zbytek systému.
    #[test]
    fn zmer_vykon_wfm() {
        let in_rate = 1_344_000.0;
        let decim = 28;
        let secs = 5.0;
        let n = (in_rate * secs) as usize;

        // Plný multiplex včetně pilotu, ať se počítá i stereo a RDS.
        let mut iq = Vec::with_capacity(n);
        let mut ph = 0.0f64;
        for i in 0..n {
            let t = i as f64 / in_rate;
            let l = (2.0 * PI * 1_000.0 * t).sin();
            let pilot = (2.0 * PI * FM_PILOT_HZ * t).cos();
            let sub = (2.0 * PI * 2.0 * FM_PILOT_HZ * t).cos();
            let mpx = 0.45 * l + 0.45 * l * sub + 0.1 * pilot;
            ph += 2.0 * PI * (FM_DEVIATION_HZ * mpx) / in_rate;
            iq.push(Complex32::new(ph.cos() as f32, ph.sin() as f32));
        }

        let mut wfm = WfmDemod::new(in_rate, decim);
        let mut sq = Squelch::new(48_000.0);
        let mut out = Vec::with_capacity(n / decim * 2);

        // Rozehřát cache.
        for &x in iq.iter().take(n / 10) {
            wfm.process(x, &mut out, &mut sq);
        }
        out.clear();

        let t0 = std::time::Instant::now();
        for &x in &iq {
            wfm.process(x, &mut out, &mut sq);
        }
        let el = t0.elapsed().as_secs_f64();
        println!(
            "\nWFM stereo + RDS, {secs} s na {:.3} MSps: {:.2} s CPU -> {:.1}x realtime, {:.1} % jádra",
            in_rate / 1e6,
            el,
            secs / el,
            el / secs * 100.0
        );
        assert!(
            el < secs * 0.5,
            "WFM sežral {:.0} % jádra - to je na hraně a bude to ztrácet vzorky",
            el / secs * 100.0
        );
    }

    /// Jádro stereo dekódování: nasyntetizuji plný FM multiplex s pilotem
    /// a signálem jen v levém kanálu. Na výstupu musí být levý kanál znatelně
    /// silnější než pravý - jinak se kanály neoddělily.
    #[test]
    fn stereo_oddeli_kanaly() {
        let in_rate = 1_344_000.0;
        let decim = 28;
        let mut wfm = WfmDemod::new(in_rate, decim);
        let mut sq = Squelch::new(48_000.0);

        // L = tón 1 kHz, R = ticho. Multiplex podle normy:
        // 0,45(L+R) + 0,45(L−R)cos(38k) + 0,1 cos(19k)
        let tone_hz = 1_000.0;
        let mut out = Vec::new();
        let mut ph = 0.0f64; // fáze FM nosné
        for n in 0..(48_000 * decim) {
            let t = n as f64 / in_rate;
            let l = (2.0 * PI * tone_hz * t).sin();
            let r = 0.0;
            // Pilot i podnosná jako kosinus: dekodér podnosnou vyrábí ze
            // vztahu cos(2θ) = 2cos²θ − 1, takže na fázi záleží.
            let pilot = (2.0 * PI * FM_PILOT_HZ * t).cos();
            let sub = (2.0 * PI * 2.0 * FM_PILOT_HZ * t).cos();
            let mpx = 0.45 * (l + r) + 0.45 * (l - r) * sub + 0.1 * pilot;
            // Multiplex namoduluji na nosnou zdvihem 75 kHz.
            ph += 2.0 * PI * (FM_DEVIATION_HZ * mpx) / in_rate;
            wfm.process(
                Complex32::new(ph.cos() as f32, ph.sin() as f32),
                &mut out,
                &mut sq,
            );
        }

        assert!(wfm.stereo_active(), "pilot je v signálu, stereo se nepustilo");

        // Na ustálené druhé polovině porovnám sílu kanálů.
        let pulka = (out.len() / 2) & !1;
        let (mut el, mut er) = (0.0f64, 0.0f64);
        for p in out[pulka..].chunks_exact(2) {
            el += (p[0] * p[0]) as f64;
            er += (p[1] * p[1]) as f64;
        }
        let pomer_db = 10.0 * (el.max(1e-20) / er.max(1e-20)).log10();
        assert!(
            pomer_db > 6.0,
            "oddělení kanálů jen {pomer_db:.1} dB - stereo nefunguje"
        );
    }

    /// RDS přes **celý** WFM řetězec: syntetizuji plný multiplex s pilotem
    /// i RDS podnosnou, namoduluji ho na FM a ověřím, že z toho vypadne název
    /// stanice.
    ///
    /// Testy v `rds.rs` krmí dekodér nosnou 57 kHz napřímo, takže neověří
    /// to podstatné - jestli si ji přijímač umí sám vyrobit z pilotu.
    /// Právě tam byla chyba, kvůli které RDS na skutečné stanici mlčelo.
    #[test]
    fn rds_projde_celym_retezcem_vcetne_zavesu() {
        let in_rate = 1_344_000.0;
        let decim = 28;
        let mut wfm = WfmDemod::new(in_rate, decim);
        let mut sq = Squelch::new(48_000.0);

        // Úrovně RDS bitů; biphase se vyrobí až při vzorkování.
        let urovne = crate::rds::testovaci_urovne(0x2601, b"KNOFLIK ", 24);
        let spb = in_rate / crate::rds::RDS_BITRATE;

        let mut out = Vec::new();
        let mut ph = 0.0f64; // fáze FM nosné
        let vzorku = (urovne.len() as f64 * spb) as usize;
        for n in 0..vzorku {
            let t = n as f64 / in_rate;
            // Zvuk: tón 1 kHz v obou kanálech (mono obsah).
            let audio = 0.3 * (2.0 * PI * 1_000.0 * t).sin();
            // Pilot 19 kHz s ~9 % zdvihu.
            let pilot = 0.09 * (2.0 * PI * FM_PILOT_HZ * t).cos();
            // RDS: biphase symbol na podnosné 57 kHz, ~3 % zdvihu.
            let bit = (n as f64 / spb) as usize;
            let uvnitr = n as f64 / spb - bit as f64;
            let lvl = urovne.get(bit).copied().unwrap_or(false);
            let amp = if lvl { 1.0 } else { -1.0 };
            let symbol = if uvnitr < 0.5 { amp } else { -amp };
            let rds = 0.03 * symbol * (2.0 * PI * 3.0 * FM_PILOT_HZ * t).cos();

            let mpx = 0.45 * audio + pilot + rds;
            ph += 2.0 * PI * (FM_DEVIATION_HZ * mpx) / in_rate;
            wfm.process(
                Complex32::new(ph.cos() as f32, ph.sin() as f32),
                &mut out,
                &mut sq,
            );
        }

        let (ok, spatne) = wfm.rds.block_stats();
        assert!(
            wfm.pilot_locked,
            "závěs se nechytil na pilot (bloky {ok}/{})",
            ok + spatne
        );
        assert_eq!(
            wfm.rds.info().ps,
            "KNOFLIK",
            "název se nepřečetl - bloků prošlo {ok}, neprošlo {spatne}"
        );
    }

    /// RDS musí projít i vedle **silného sterea**. To je ten skutečný případ:
    /// rozdílový kanál sahá do 53 kHz, po směšování na 57 kHz padne jeho okraj
    /// na 4 kHz - tedy hned vedle RDS pásma - a bývá o dvacet dB silnější.
    /// Se slabou propustí se do dat propere a bloky přestanou procházet.
    #[test]
    fn rds_projde_i_vedle_silneho_sterea() {
        let in_rate = 1_344_000.0;
        let decim = 28;
        let mut wfm = WfmDemod::new(in_rate, decim);
        let mut sq = Squelch::new(48_000.0);

        let urovne = crate::rds::testovaci_urovne(0x2601, b"KNOFLIK ", 24);
        let spb = in_rate / crate::rds::RDS_BITRATE;

        let mut out = Vec::new();
        let mut ph = 0.0f64;
        let vzorku = (urovne.len() as f64 * spb) as usize;
        for n in 0..vzorku {
            let t = n as f64 / in_rate;
            // Rozdílový kanál naplno a s obsahem až u 15 kHz - tedy přesně to,
            // co po směšování dopadne nejblíž k RDS.
            let l_minus_r = 0.9 * (2.0 * PI * 14_500.0 * t).sin();
            let sub = (2.0 * PI * 2.0 * FM_PILOT_HZ * t).cos();
            let pilot = 0.09 * (2.0 * PI * FM_PILOT_HZ * t).cos();

            let bit = (n as f64 / spb) as usize;
            let uvnitr = n as f64 / spb - bit as f64;
            let amp = if urovne.get(bit).copied().unwrap_or(false) {
                1.0
            } else {
                -1.0
            };
            let symbol = if uvnitr < 0.5 { amp } else { -amp };
            // RDS je slabé: ~3 % zdvihu proti 45 % sterea.
            let rds = 0.03 * symbol * (2.0 * PI * 3.0 * FM_PILOT_HZ * t).cos();

            let mpx = 0.45 * l_minus_r * sub + pilot + rds;
            ph += 2.0 * PI * (FM_DEVIATION_HZ * mpx) / in_rate;
            wfm.process(
                Complex32::new(ph.cos() as f32, ph.sin() as f32),
                &mut out,
                &mut sq,
            );
        }

        let (ok, spatne) = wfm.rds.block_stats();
        assert_eq!(
            wfm.rds.info().ps,
            "KNOFLIK",
            "název se nepřečetl vedle silného sterea - bloků prošlo {ok}, neprošlo {spatne}"
        );
    }

    /// Bez pilotu se nesmí pustit stereo - jinak by se z šumu kolem 38 kHz
    /// vyrobil rozdílový kanál a poslech by jen syčel.
    #[test]
    fn bez_pilotu_zustava_mono() {
        let in_rate = 1_344_000.0;
        let decim = 28;
        let mut wfm = WfmDemod::new(in_rate, decim);
        let mut sq = Squelch::new(48_000.0);

        // FM s jedním tónem, žádný pilot na 19 kHz.
        let mut out = Vec::new();
        let (mut ph, mut tph) = (0.0f64, 0.0f64);
        for _ in 0..(48_000 * decim) {
            let f = 30_000.0 * (2.0 * PI * tph).sin();
            ph += 2.0 * PI * f / in_rate;
            tph += 1_000.0 / in_rate;
            wfm.process(
                Complex32::new(ph.cos() as f32, ph.sin() as f32),
                &mut out,
                &mut sq,
            );
        }
        assert!(!wfm.stereo_active(), "bez pilotu se pustilo stereo");
        // A oba kanály musí být totožné.
        let rozdil = out
            .chunks_exact(2)
            .map(|p| (p[0] - p[1]).abs())
            .fold(0.0f32, f32::max);
        assert!(rozdil < 1e-6, "kanály se liší o {rozdil}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Přenos filtru v dB na frekvenci `f` (DTFT koeficientů).
    fn response_db(taps: &[Complex32], f: f64, fs: f64) -> f64 {
        let mut re = 0.0;
        let mut im = 0.0;
        for (k, &t) in taps.iter().enumerate() {
            let ph = -2.0 * PI * f * k as f64 / fs;
            let (s, c) = ph.sin_cos();
            re += t.re as f64 * c - t.im as f64 * s;
            im += t.re as f64 * s + t.im as f64 * c;
        }
        20.0 * (re * re + im * im).sqrt().max(1e-12).log10()
    }

    /// Najde frekvenci, kde přenos poprvé klesne pod `target_db`.
    fn cutoff_at(taps: &[Complex32], fs: f64, target_db: f64) -> f64 {
        let mut f = 0.0;
        while f < fs / 2.0 {
            if response_db(taps, f, fs) < target_db {
                return f;
            }
            f += 2.0;
        }
        fs / 2.0
    }

    /// Kolik procesoru sežere samotný DSP řetězec. Užitečné při úvahách,
    /// jestli to poběží i na slabším stroji.
    #[test]
    fn zmer_vykon_retezce() {
        let fs = 96_000.0;
        let secs = 10.0;
        let n = (fs * secs) as usize;
        let iq: Vec<Complex32> = (0..n)
            .map(|i| {
                let ph = 2.0 * PI * 10_000.0 * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();

        println!("\nDSP řetězec, {secs} s signálu na 96 kHz:");
        for (mode, bw, dec) in [
            (Mode::Am, 8000.0, Decoder::Off),
            (Mode::Cw, 500.0, Decoder::Off),
            (Mode::Cw, 500.0, Decoder::Cw),
            (Mode::Am, 8000.0, Decoder::Rtty),
        ] {
            let mut rx = Demod::new(fs, 2, bw, mode);
            rx.set_offset(10_000.0);
            rx.set_decoder(dec, RttyConfig::default(), 10.0);
            let mut out = Vec::with_capacity(n / 2);

            // Rozehřát cache, jinak první měření vyjde nesmyslně pomalé.
            rx.process(&iq[..n / 10], &mut out);
            out.clear();
            let _ = rx.take_text();

            let t0 = std::time::Instant::now();
            rx.process(&iq, &mut out);
            let el = t0.elapsed().as_secs_f64();
            println!(
                "  {:3} bw={:5.0} dek={:7}  {:.2} s CPU  ->  {:5.1}x realtime, {:4.1} % jádra",
                mode.label(),
                bw,
                dec.label(),
                el,
                secs / el,
                el / secs * 100.0
            );
        }
    }

    #[test]
    fn zmer_uzke_filtry() {
        // Kanálový filtr běží na výstupní vzorkovačce, ne na vstupní.
        let fs = 48_000.0;
        println!("\nkanálový filtr: {CHAN_TAPS} koef. @ {fs} Hz");
        println!(" šířka   žádaný -6dB   skutečný -6dB   -60dB");
        for bw in [100.0, 150.0, 200.0, 250.0, 300.0, 500.0, 800.0] {
            let taps = filter_taps(Mode::Cw, bw, fs, CHAN_TAPS);
            println!(
                "{:6.0} Hz {:8.0} Hz  {:11.0} Hz  {:7.0} Hz",
                bw,
                bw / 2.0,
                cutoff_at(&taps, fs, -6.0),
                cutoff_at(&taps, fs, -60.0)
            );
        }
    }

    #[test]
    fn lowpass_ma_jednotkovy_zisk_v_dc() {
        let taps = lowpass_taps(5000.0, 96000.0, CHAN_TAPS);
        let sum: f32 = taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "součet koeficientů = {sum}");
    }

    /// CW filtr musí být poctivý i v těch nejužších polohách - tam je to
    /// nejcennější a zároveň nejsnáz se to rozbije.
    #[test]
    fn uzky_cw_filtr_odpovida_stitku() {
        let fs = 48_000.0;
        let (min, max) = crate::radio::bandwidth_range(Mode::Cw);
        let mut bw = min;
        while bw <= max {
            let taps = filter_taps(Mode::Cw, bw, fs, CHAN_TAPS);
            let f6 = cutoff_at(&taps, fs, -6.0);
            assert!(
                (f6 - bw / 2.0).abs() <= 5.0,
                "CW {bw} Hz: -6 dB vyšlo na {f6} Hz místo {} Hz",
                bw / 2.0
            );
            bw += 50.0;
        }
    }

    /// V celém povoleném rozsahu musí -6 dB bod odpovídat tomu, co uživatel
    /// nastavil. Kdyby se snížil počet koeficientů nebo povolila užší mez,
    /// štítek by přestal platit a tenhle test spadne.
    #[test]
    fn sirka_pasma_am_odpovida_stitku() {
        // Kanálový filtr běží na výstupní vzorkovačce.
        let fs = 48_000.0;
        let (min, max) = crate::radio::bandwidth_range(Mode::Am);
        let mut bw = min;
        while bw <= max {
            let taps = filter_taps(Mode::Am, bw, fs, CHAN_TAPS);
            let f6 = cutoff_at(&taps, fs, -6.0);
            assert!(
                (f6 - bw / 2.0).abs() <= 100.0,
                "šířka {bw} Hz: -6 dB vyšlo na {f6} Hz místo {} Hz",
                bw / 2.0
            );
            bw += 1000.0;
        }
    }

    /// Při nejširším pásmu musí být stopband ještě pod Nyquistem po decimaci,
    /// jinak by se do zvuku složil aliasing.
    #[test]
    fn nejsirsi_pasmo_nealiasuje() {
        let fs = 48_000.0;
        let nyquist_po_decimaci = fs / 2.0;
        let (_, max) = crate::radio::bandwidth_range(Mode::Am);
        let taps = filter_taps(Mode::Am, max, fs, CHAN_TAPS);
        let f60 = cutoff_at(&taps, fs, -60.0);
        assert!(
            f60 < nyquist_po_decimaci,
            "stopband začíná až na {f60} Hz, Nyquist po decimaci je {nyquist_po_decimaci} Hz"
        );
    }

    /// Jádro SSB: filtr musí propustit svoji stranu spektra a potlačit
    /// tu druhou. Bez toho by USB i LSB zněly stejně.
    #[test]
    fn ssb_potlacuje_opacne_postranni_pasmo() {
        let fs = 48_000.0;
        let bw = 2700.0;
        for (mode, want, unwanted) in [
            (Mode::Usb, 1000.0, -1000.0),
            (Mode::Lsb, -1000.0, 1000.0),
        ] {
            let taps = filter_taps(mode, bw, fs, CHAN_TAPS);
            let pass = response_db(&taps, want, fs);
            let reject = response_db(&taps, unwanted, fs);
            assert!(
                pass > -3.0,
                "{:?}: vlastní pásmo na {want} Hz má být propuštěné, je {pass:.1} dB",
                mode
            );
            assert!(
                reject < -50.0,
                "{:?}: opačné pásmo na {unwanted} Hz má být potlačené, je {reject:.1} dB",
                mode
            );
        }
    }

    #[test]
    fn am_demoduluje_ton_na_offsetu() {
        // Nosná na +10 kHz modulovaná 1 kHz tónem, hloubka 50 %.
        let fs = 96000.0;
        let mut rx = Demod::new(fs, 2, 8000.0, Mode::Am);
        rx.set_offset(10_000.0);
        let mut iq = Vec::new();
        for n in 0..96000 {
            let t = n as f64 / fs;
            let m = 1.0 + 0.5 * (2.0 * PI * 1000.0 * t).sin();
            let ph = 2.0 * PI * 10_000.0 * t;
            iq.push(Complex32::new((m * ph.cos()) as f32, (m * ph.sin()) as f32));
        }
        let mut out = Vec::new();
        rx.process(&iq, &mut out);
        // Prokládané stereo: 48000 rámců = 96000 vzorků.
        assert_eq!(out.len(), 96000, "decimace /2 z 96k vzorků, prokládaně");
        // Mono režim musí mít oba kanály shodné.
        assert!(
            out.chunks_exact(2).all(|p| p[0] == p[1]),
            "AM je mono, kanály se musí rovnat"
        );
        // Po ustálení AGC musí být na výstupu znatelný signál.
        let tail = &out[48000..];
        let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms > 0.05, "RMS demodulovaného tónu = {rms}");
    }
}
