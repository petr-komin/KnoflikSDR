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
use num_complex::Complex32;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Koeficienty antialiasingové propusti před decimací. Je široká, takže
/// jich stačí málo.
const PRE_TAPS: usize = 127;
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
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Am => "AM",
            Mode::Usb => "USB",
            Mode::Lsb => "LSB",
            Mode::Cw => "CW",
        }
    }

    pub fn is_ssb(&self) -> bool {
        matches!(self, Mode::Usb | Mode::Lsb)
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
        // AM i CW jsou symetrické kolem nosné.
        Mode::Am | Mode::Cw => 0.0,
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

/// Jednoduchá AGC s rychlým náběhem a pomalým uvolněním.
pub struct Agc {
    env: f32,
    target: f32,
    attack: f32,
    decay: f32,
    max_gain: f32,
}

impl Agc {
    pub fn new(sample_rate: f32) -> Self {
        Agc {
            env: 0.0,
            target: 0.25,
            // ~5 ms náběh, ~500 ms uvolnění
            attack: 1.0 - (-1.0 / (0.005 * sample_rate)).exp(),
            decay: 1.0 - (-1.0 / (0.500 * sample_rate)).exp(),
            max_gain: 500.0,
        }
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
        let g = if self.env > 1e-9 {
            (self.target / self.env).min(self.max_gain)
        } else {
            1.0
        };
        (x * g).clamp(-1.0, 1.0)
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
        let pre: Vec<Complex32> = lowpass_taps(out_rate * 0.45, in_rate, PRE_TAPS)
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
    pub fn set_bandwidth(&mut self, bw_hz: f64) {
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

    /// Úroveň naladěného signálu v dBFS (před AGC). Pro S-metr.
    pub fn level_dbfs(&self) -> f32 {
        20.0 * self.agc.envelope().max(1e-9).log10()
    }

    fn refresh_taps(&mut self) {
        let rate = self.out_rate();
        self.chan
            .set_taps(filter_taps(self.mode, self.bandwidth_hz, rate, CHAN_TAPS));
    }

    /// Zpracuje blok I/Q vzorků a připojí audio na konec `out`.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
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
                };
                let audio = self.dc.push(detected);
                out.push(self.agc.push(audio));
            }
        }
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
        assert_eq!(out.len(), 48000, "decimace /2 z 96k vzorků");
        // Po ustálení AGC musí být na výstupu znatelný signál.
        let tail = &out[24000..];
        let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms > 0.05, "RMS demodulovaného tónu = {rms}");
    }
}
