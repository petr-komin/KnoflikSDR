//! RDS - datový kanál FM rozhlasu na podnosné 57 kHz.
//!
//! Cesta signálu: multiplex × nosná 57 kHz (odvozená z pilotu, viz `dsp.rs`)
//! -> dolní propust -> obnova bitového taktu -> diferenciální dekódování ->
//! synchronizace bloků přes syndromy -> skupiny -> název stanice a RadioText.
//!
//! Data jdou 1187,5 bit/s (= 57 000 / 48) v biphase kódování a navíc
//! diferenciálně: nese se **změna** úrovně, ne úroveň sama. Díky tomu nevadí,
//! že BPSK demodulátor může být otočený o 180°.
//!
//! Tok je členěný na 26bitové bloky (16 dat + 10 kontrolních). Kontrolní bity
//! nejsou jen CRC - je do nich přičtené "offset word" podle pořadí bloku ve
//! skupině, takže ten samý výpočet zároveň hlídá chyby i říká, kde ve skupině
//! zrovna jsme. Na tom stojí celá synchronizace: rámce se nehledají podle
//! značky, ale podle toho, který offset zrovna sedí.

/// Bitová rychlost RDS. Přesně 57 000 / 48.
pub const RDS_BITRATE: f64 = 1_187.5;
/// Šířka propusti kolem podnosné - data zabírají ±2,4 kHz.
const RDS_LOWPASS_HZ: f64 = 2_400.0;

/// Jakosti stupňů Butterworthovy propusti 8. řádu.
///
/// Dva stupně nestačí. Po směšování na 57 kHz padne horní okraj stereo
/// podnosné (53 kHz) na 4 kHz, tedy těsně vedle RDS pásma - a stereo bývá
/// o dvacet dB silnější než RDS. Dvoustupňová propust tam dá jen ~18 dB,
/// takže stereo prolézalo rovnou do dat. Osmý řád dá přes 35 dB.
///
/// Jakosti musí být takhle odstupňované; kdyby se jen zopakovalo 0,707,
/// nevyjde maximálně plochá charakteristika a propustné pásmo se ohne.
const RDS_LP_Q: [f64; 4] = [0.5098, 0.6013, 0.8999, 2.5629];

/// Jak silně dorovnávat bitový takt.
///
/// Bitové hodiny vysílače a vzorkovačka přijímače nejedou stejně rychle -
/// liší se o ppm obou krystalů. Volnoběžný takt by se proto po chvíli rozešel
/// a synchronizace bloků by pořád padala.
///
/// Korekce se uplatní **jednou za bit**, ne na každý průchod nulou. Signál za
/// propustí 2,4 kHz má šumových průchodů kolem 4 800 za vteřinu, kdežto bitů
/// je 1 187 - většina průchodů tedy patří šumu a kdyby každý cukl fází,
/// vzorkovalo by se mimo optimum a chybovost by zůstala vysoko i na silné
/// stanici. Bere se proto jen ten průchod, který leží nejblíž středu bitu:
/// tam má biphase přechod vždycky, takže je to spolehlivá značka.
const RDS_TIMING_GAIN: f64 = 0.15;
/// Jak daleko od středu bitu ještě hledat přechod. Mimo tohle okno jsou
/// průchody nulou skoro jistě šum nebo hranice symbolu.
const RDS_TIMING_WINDOW: f64 = 0.25;

/// Jak rychle se dohledává fáze podnosné - v ustáleném stavu.
///
/// Pomalu: fáze se po zamčení pilotu nemění, tak ať odhad nerozhoupe šum.
const RDS_PHASE_GAIN: f32 = 0.0002;
/// Zisk fázového odhadu hned po rozběhu. Dokud fáze není nalezená, je průmět
/// do reálné osy skoro nulový a nemá se z čeho obnovovat takt - čekat na to
/// pomalým odhadem by zdrželo zachycení o celé vteřiny.
const RDS_PHASE_GAIN_FAST: f32 = 0.004;
/// Kolik vzorků jet rychlým odhadem, než se přeřadí na pomalý.
/// Při 336 kHz je to zhruba 60 ms - dost na ustálení.
const RDS_PHASE_FAST_SAMPLES: u32 = 20_000;

/// Po kolika bitech bez jediného platného bloku zkusit posun o půl symbolu.
/// ~1300 bitů je zhruba sekunda - dost na to, aby se při správném taktu
/// nějaký blok trefil, a zároveň to nezdržuje, když je fáze vedle.
const BITS_BEFORE_HALF_SHIFT: u32 = 1_300;

/// Generující polynom kontrolního součtu: x¹⁰+x⁸+x⁷+x⁵+x⁴+x³+1.
///
/// Bity odpovídají mocninám od x¹⁰ dolů. Hodnotu hlídá test
/// `polynom_odpovida_norme` - dokud tu byl překlep (0x6B9 místo 0x5B9),
/// nesedělo na skutečné vysílání nic a dekodér jen chytal falešné shody.
/// Kruhové testy to neodhalily: generovaly bloky týmž polynomem, kterým je
/// pak ověřovaly, takže si potvrzovaly vlastní chybu.
const RDS_POLY: u32 = 0b101_1011_1001;

/// Offsety A, B, C, D a C' - přičítají se ke kontrolním bitům podle pozice
/// bloku ve skupině. C' se používá místo C ve skupinách typu B.
const OFFSET_A: u16 = 0b00_1111_1100;
const OFFSET_B: u16 = 0b01_1001_1000;
const OFFSET_C: u16 = 0b01_0110_1000;
const OFFSET_CP: u16 = 0b11_0101_0000;
const OFFSET_D: u16 = 0b01_1011_0100;

/// Spočítá kontrolních 10 bitů k 16bitovému slovu.
///
/// Je to dělení polynomem nad GF(2): slovo se posune o 10 bitů a postupně
/// se od něj odečítá (XORuje) generující polynom, kdykoli přeteče.
fn syndrome(word: u16) -> u16 {
    let mut reg: u32 = (word as u32) << 10;
    for i in (10..26).rev() {
        if reg & (1 << i) != 0 {
            reg ^= RDS_POLY << (i - 10);
        }
    }
    (reg & 0x3FF) as u16
}

/// Ověří 26bitový blok proti danému offsetu; vrátí datové slovo, když sedí.
fn check_block(block: u32, offset: u16) -> Option<u16> {
    let data = (block >> 10) as u16;
    let check = (block & 0x3FF) as u16;
    if check ^ offset == syndrome(data) {
        Some(data)
    } else {
        None
    }
}

/// Kde ve skupině jsme.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlockPos {
    A,
    B,
    C,
    D,
}

impl BlockPos {
    fn next(self) -> BlockPos {
        match self {
            BlockPos::A => BlockPos::B,
            BlockPos::B => BlockPos::C,
            BlockPos::C => BlockPos::D,
            BlockPos::D => BlockPos::A,
        }
    }
}

/// Co se z RDS vyčetlo. Drží se to jako celek, ať GUI nemusí skládat kousky.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct RdsInfo {
    /// Název stanice (Program Service) - osm znaků, chodí po dvojicích.
    pub ps: String,
    /// RadioText - až 64 znaků, chodí po čtveřicích (2A) nebo dvojicích (2B).
    pub rt: String,
    /// Kód programu (PI) - jednoznačný identifikátor stanice.
    pub pi: Option<u16>,
}

/// Dekodér RDS. Bere multiplex a nosnou 57 kHz, vydává název stanice a text.
pub struct RdsDecoder {
    /// Dolní propust za směšovačem - zvlášť pro soufázovou a kvadraturní větev.
    lp_i: [Biquad; 4],
    lp_q: [Biquad; 4],
    /// Průměr z² pro odhad fáze. U BPSK je z = ±A·e^{jφ}, takže z² = A²·e^{j2φ}
    /// bez ohledu na to, jaká data se zrovna vysílají - průměrem tedy vyjde
    /// dvojnásobek hledaného úhlu a data se nevyruší.
    sq_i: f32,
    sq_q: f32,
    /// Kolik vzorků mezifrekvence připadá na jeden bit.
    samples_per_bit: f64,
    /// Kde jsme v bitu (0..samples_per_bit).
    bit_phase: f64,
    /// Integrace vzorků přes půlbity - z jejich rozdílu vyjde biphase bit.
    half_acc: [f32; 2],
    half_idx: usize,
    /// Předchozí vzorek za propustí - pro hledání průchodů nulou, podle
    /// kterých se dorovnává bitový takt.
    last_v: f32,
    /// Odchylka nalezeného přechodu od středu bitu (v podílu bitu).
    /// Sbírá se během bitu, uplatní se na jeho konci.
    mid_err: Option<f64>,
    /// Kolik vzorků uteklo od startu - podle toho se přeřazuje zisk
    /// fázového odhadu z rychlého na pomalý.
    warmup: u32,
    /// Předchozí bit pro diferenciální dekódování.
    last_bit: Option<bool>,
    /// Posuvný registr posledních 26 bitů.
    shift: u32,
    /// Kolik bitů už v registru je - dokud jich není 26, nemá cenu zkoušet.
    filled: u32,
    /// Jsme sesynchronizovaní? A na které pozici ve skupině.
    synced: bool,
    pos: BlockPos,
    /// Kolik bitů zbývá do konce právě očekávaného bloku.
    bits_to_block: u32,
    /// Rozečtená skupina (bloky A..D).
    group: [Option<u16>; 4],
    /// Kolik bloků po sobě neprošlo - po několika se sync pouští.
    bad_blocks: u32,
    /// Rozečtený název stanice a RadioText; do `info` se překlopí, až je celý.
    ps_buf: [u8; 8],
    rt_buf: [u8; 64],
    /// Poslední hodnota přepínače A/B u RadioTextu - jeho změna znamená
    /// nový text a starý se musí zahodit.
    rt_ab: Option<bool>,
    info: RdsInfo,
    /// Kolik bitů uteklo od posledního bloku, který prošel kontrolou.
    /// Podle toho se pozná zaseknutí na půlbitovém posunu (viz `feed_bit`).
    bits_since_good: u32,
    /// Kolik bloků prošlo a kolik neprošlo - jen pro diagnostiku v GUI.
    good_blocks: u32,
    bad_total: u32,
    /// Klouzavý průměr |raw| a raw² z rozhodování o bitech.
    ///
    /// Jejich poměr je bezrozměrná míra "otevření oka": u čistého BPSK je
    /// rozhodovací veličina skoro konstantní a poměr vyjde k 1, u signálu
    /// utopeného v šumu se blíží 0,80 (což je poměr pro Gaussův šum).
    /// Rozliší to dva úplně různé případy - rozmazané bity proti čistým
    /// bitům, které kazí až synchronizace.
    raw_abs: f32,
    raw_sq: f32,
    /// Kolikrát se povedlo chytit synchronizaci - časté chytání znamená,
    /// že se zas a znovu ztrácí.
    sync_count: u32,
}

use crate::dsp::Biquad;

impl RdsDecoder {
    pub fn new(if_rate: f64) -> Self {
        RdsDecoder {
            // Osmý řád - musí oddělit RDS od stereo podnosné, která leží hned
            // vedle a je o dvacet dB silnější (viz RDS_LP_Q).
            lp_i: std::array::from_fn(|k| Biquad::lowpass(RDS_LOWPASS_HZ, RDS_LP_Q[k], if_rate)),
            lp_q: std::array::from_fn(|k| Biquad::lowpass(RDS_LOWPASS_HZ, RDS_LP_Q[k], if_rate)),
            sq_i: 0.0,
            sq_q: 0.0,
            samples_per_bit: if_rate / RDS_BITRATE,
            bit_phase: 0.0,
            half_acc: [0.0; 2],
            half_idx: 0,
            last_v: 0.0,
            mid_err: None,
            warmup: 0,
            last_bit: None,
            shift: 0,
            filled: 0,
            synced: false,
            pos: BlockPos::A,
            bits_to_block: 26,
            group: [None; 4],
            bad_blocks: 0,
            ps_buf: [b' '; 8],
            rt_buf: [b' '; 64],
            rt_ab: None,
            info: RdsInfo::default(),
            bits_since_good: 0,
            good_blocks: 0,
            bad_total: 0,
            raw_abs: 0.0,
            raw_sq: 0.0,
            sync_count: 0,
        }
    }

    pub fn info(&self) -> &RdsInfo {
        &self.info
    }

    /// Zahodí všechno rozečtené a začne od nuly.
    ///
    /// Volá se při přeladění na jinou stanici: název i RadioText patří té
    /// předchozí a nechat je viset by lhalo. Koeficienty filtrů zůstávají,
    /// mění se jen stav - vzorkovačka je pořád stejná.
    pub fn reset(&mut self) {
        let if_rate = self.samples_per_bit * RDS_BITRATE;
        *self = RdsDecoder::new(if_rate);
    }

    /// Kolik bloků prošlo kontrolou a kolik ne. Pro diagnostiku: když
    /// neprochází vůbec nic, je chyba v demodulaci nebo v taktu; když
    /// prochází část, jde jen o slabý signál.
    pub fn block_stats(&self) -> (u32, u32) {
        (self.good_blocks, self.bad_total)
    }

    /// Otevření oka: mean|raw| / sqrt(mean raw²). Blízko 1 = čisté bity,
    /// kolem 0,8 = rozhodování utopené v šumu.
    pub fn eye(&self) -> f32 {
        let rms = self.raw_sq.max(1e-20).sqrt();
        self.raw_abs / rms
    }

    /// Kolikrát dekodér chytal synchronizaci a jestli ji drží teď.
    pub fn sync_stats(&self) -> (u32, bool) {
        (self.sync_count, self.synced)
    }

    /// Jeden vzorek multiplexu s oběma fázemi nosné 57 kHz.
    ///
    /// Obě fáze jsou potřeba, protože skutečná podnosná je proti nosné
    /// odvozené z pilotu natočená o neznámý úhel. Dekodér si ho dopočítá
    /// a signál srovná - jinak by při nešťastné fázi vyšla z demodulace
    /// skoro nula a data by se neobjevila vůbec.
    pub fn push(&mut self, mpx: f32, cos57: f32, sin57: f32) {
        // Směšování na základní pásmo - obě větve zvlášť.
        let mut i = mpx * cos57;
        let mut q = mpx * sin57;
        for f in self.lp_i.iter_mut() {
            i = f.push(i);
        }
        for f in self.lp_q.iter_mut() {
            q = f.push(q);
        }

        // Odhad fáze: průměr z². Data (±1) se umocněním vyruší, zbude 2φ.
        // Po rozběhu se jede rychleji, ať se fáze najde hned; pak se přeřadí
        // na pomalý zisk, který je odolnější proti šumu.
        let g = if self.warmup < RDS_PHASE_FAST_SAMPLES {
            self.warmup += 1;
            RDS_PHASE_GAIN_FAST
        } else {
            RDS_PHASE_GAIN
        };
        self.sq_i += ((i * i - q * q) - self.sq_i) * g;
        self.sq_q += ((2.0 * i * q) - self.sq_q) * g;
        let phi = 0.5 * self.sq_q.atan2(self.sq_i);

        // Otočení zpět a průmět do reálné osy - tam leží data.
        let (sin_phi, cos_phi) = phi.sin_cos();
        let v = i * cos_phi + q * sin_phi;

        // Hledání přechodu uprostřed bitu. Zaznamená se jen ten průchod nulou,
        // který leží nejblíž středu - tam má biphase přechod vždycky, takže
        // je to spolehlivá značka. Korekce se z toho udělá až na konci bitu,
        // jednou; kdyby fází cukal každý průchod, řídil by takt hlavně šum.
        let p = self.bit_phase / self.samples_per_bit;
        if (self.last_v < 0.0) != (v < 0.0) && (p - 0.5).abs() < RDS_TIMING_WINDOW {
            let err = p - 0.5;
            if self.mid_err.is_none_or(|e: f64| err.abs() < e.abs()) {
                self.mid_err = Some(err);
            }
        }
        self.last_v = v;

        // Biphase: každý bit má dvě poloviny s opačným znaménkem, takže se
        // integrují zvlášť a rozhoduje jejich rozdíl. Zároveň to potlačí
        // stejnosměrnou složku, která by jinak rozhodování posunula.
        let half = self.samples_per_bit / 2.0;
        self.half_acc[self.half_idx] += v;
        self.bit_phase += 1.0;

        if self.half_idx == 0 && self.bit_phase >= half {
            self.half_idx = 1;
        } else if self.bit_phase >= self.samples_per_bit {
            let raw = self.half_acc[0] - self.half_acc[1];
            self.half_acc = [0.0; 2];
            self.half_idx = 0;
            self.bit_phase -= self.samples_per_bit;
            // Jedna korekce za bit, z nejdůvěryhodnějšího přechodu.
            if let Some(err) = self.mid_err.take() {
                self.bit_phase -= err * self.samples_per_bit * RDS_TIMING_GAIN;
            }
            // Statistika rozhodovací veličiny - viz `eye`.
            self.raw_abs += (raw.abs() - self.raw_abs) * 0.002;
            self.raw_sq += (raw * raw - self.raw_sq) * 0.002;
            self.feed_bit(raw > 0.0);
        }
    }

    /// Diferenciální dekódování a posun do registru.
    fn feed_bit(&mut self, level: bool) {
        // Nese se změna úrovně, ne úroveň - proto se BPSK smí otočit o 180°
        // a data z toho vyjdou stejná.
        let Some(prev) = self.last_bit.replace(level) else {
            return;
        };
        let bit = prev != level;

        self.shift = ((self.shift << 1) | bit as u32) & 0x3FF_FFFF;
        self.filled = (self.filled + 1).min(26);
        if self.filled < 26 {
            return;
        }

        // Půlbitová nejednoznačnost: v biphase kódování leží přechod uprostřed
        // symbolu i na jeho hranici, takže se takt může zamknout o půl bitu
        // vedle. Integrační okno pak leží přes dva sousední symboly, bity jsou
        // nesmysl a synchronizace nenaskočí nikdy. Průchody nulou to samy
        // nerozliší - obojí je pro ně platný bod. Když tedy dlouho neprojde
        // ani jeden blok, posuneme fázi o půl symbolu a zkusíme to z druhé strany.
        self.bits_since_good += 1;
        if self.bits_since_good > BITS_BEFORE_HALF_SHIFT {
            self.bit_phase += self.samples_per_bit / 2.0;
            self.half_acc = [0.0; 2];
            self.half_idx = 0;
            self.synced = false;
            self.filled = 0;
            self.bits_since_good = 0;
            return;
        }

        if self.synced {
            self.bits_to_block -= 1;
            if self.bits_to_block == 0 {
                self.take_block();
                self.bits_to_block = 26;
            }
        } else {
            self.try_sync();
        }
    }

    /// Hledá začátek rámce: zkouší, jestli posledních 26 bitů sedí na některý
    /// z offsetů. Blok A je jednoznačný začátek skupiny, tak se chytáme jeho.
    fn try_sync(&mut self) {
        if let Some(pi) = check_block(self.shift, OFFSET_A) {
            self.synced = true;
            self.pos = BlockPos::A;
            self.group = [Some(pi), None, None, None];
            self.bits_to_block = 26;
            self.bad_blocks = 0;
            self.pos = BlockPos::B;
            self.info.pi = Some(pi);
            // Chycený blok A je důkaz, že takt sedí - půlbitový posun odpadá.
            self.bits_since_good = 0;
            self.good_blocks = self.good_blocks.saturating_add(1);
            self.sync_count = self.sync_count.saturating_add(1);
        }
    }

    /// Vyhodnotí právě dočtený blok podle toho, kde ve skupině má být.
    fn take_block(&mut self) {
        let offset = match self.pos {
            BlockPos::A => OFFSET_A,
            BlockPos::B => OFFSET_B,
            // Skupiny typu B mají místo C variantu C'; zkouší se obě.
            BlockPos::C => OFFSET_C,
            BlockPos::D => OFFSET_D,
        };
        let mut data = check_block(self.shift, offset);
        if data.is_none() && self.pos == BlockPos::C {
            data = check_block(self.shift, OFFSET_CP);
        }

        let idx = match self.pos {
            BlockPos::A => 0,
            BlockPos::B => 1,
            BlockPos::C => 2,
            BlockPos::D => 3,
        };
        self.group[idx] = data;

        if data.is_none() {
            self.bad_blocks += 1;
            self.bad_total = self.bad_total.saturating_add(1);
            // Když se sype několik bloků po sobě, sync je nejspíš falešný.
            if self.bad_blocks >= 6 {
                self.synced = false;
                self.bad_blocks = 0;
            }
        } else {
            self.bad_blocks = 0;
            self.good_blocks = self.good_blocks.saturating_add(1);
            // Platný blok znamená, že takt sedí - půlbitový posun se nekoná.
            self.bits_since_good = 0;
            if self.pos == BlockPos::A {
                self.info.pi = data;
            }
        }

        if self.pos == BlockPos::D {
            self.decode_group();
            self.group = [None; 4];
        }
        self.pos = self.pos.next();
    }

    /// Vytáhne ze skupiny to, co umíme: název stanice (0A/0B) a RadioText (2A/2B).
    fn decode_group(&mut self) {
        let (Some(_a), Some(b)) = (self.group[0], self.group[1]) else {
            return;
        };
        let typ = (b >> 12) & 0xF;
        let verze_b = (b >> 11) & 1 == 1;

        match (typ, verze_b) {
            // 0A i 0B nesou název stanice po dvou znacích.
            (0, _) => {
                let idx = (b & 0x3) as usize * 2;
                if let Some(d) = self.group[3] {
                    self.ps_buf[idx] = (d >> 8) as u8;
                    self.ps_buf[idx + 1] = (d & 0xFF) as u8;
                    self.info.ps = to_text(&self.ps_buf);
                }
            }
            // 2A: čtyři znaky textu v blocích C a D.
            (2, false) => {
                let ab = (b >> 4) & 1 == 1;
                self.reset_rt_if_changed(ab);
                let idx = (b & 0xF) as usize * 4;
                if let Some(c) = self.group[2] {
                    self.put_rt(idx, (c >> 8) as u8);
                    self.put_rt(idx + 1, (c & 0xFF) as u8);
                }
                if let Some(d) = self.group[3] {
                    self.put_rt(idx + 2, (d >> 8) as u8);
                    self.put_rt(idx + 3, (d & 0xFF) as u8);
                }
                self.info.rt = to_text(&self.rt_buf);
            }
            // 2B: dva znaky, jen v bloku D.
            (2, true) => {
                let ab = (b >> 4) & 1 == 1;
                self.reset_rt_if_changed(ab);
                let idx = (b & 0xF) as usize * 2;
                if let Some(d) = self.group[3] {
                    self.put_rt(idx, (d >> 8) as u8);
                    self.put_rt(idx + 1, (d & 0xFF) as u8);
                }
                self.info.rt = to_text(&self.rt_buf);
            }
            _ => {}
        }
    }

    /// Přepnutí příznaku A/B znamená nový text - ten starý se musí zahodit,
    /// jinak by se nové znaky mísily se zbytky předchozí zprávy.
    fn reset_rt_if_changed(&mut self, ab: bool) {
        if self.rt_ab != Some(ab) {
            self.rt_buf = [b' '; 64];
            self.rt_ab = Some(ab);
        }
    }

    fn put_rt(&mut self, idx: usize, ch: u8) {
        if idx < self.rt_buf.len() {
            self.rt_buf[idx] = ch;
        }
    }
}

/// Převod bajtů z RDS na čitelný text.
///
/// Znaková sada RDS není ASCII, ale pro běžné názvy a texty se v prvních 128
/// hodnotách kryjí. Znak 0x0D ukončuje text, netisknutelné se nahradí mezerou,
/// ať se do GUI nedostane nesmysl.
fn to_text(buf: &[u8]) -> String {
    let konec = buf.iter().position(|&c| c == 0x0D).unwrap_or(buf.len());
    buf[..konec]
        .iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                ' '
            }
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Vyrobí posloupnost úrovní pro skupiny 0A s daným PI a názvem stanice -
/// tedy to, co jde po diferenciálním kódování na biphase modulátor.
///
/// Sdílené mezi testy RDS a testem celého WFM řetězce v `dsp.rs`, aby se
/// generátor nemusel psát dvakrát.
#[cfg(test)]
pub(crate) fn testovaci_urovne(pi: u16, jmeno: &[u8; 8], skupin: usize) -> Vec<bool> {
    let mut bits: Vec<bool> = Vec::new();
    for _ in 0..skupin {
        for i in 0..4u16 {
            let d_blok = ((jmeno[i as usize * 2] as u16) << 8) | jmeno[i as usize * 2 + 1] as u16;
            for (slovo, off) in [
                (pi, OFFSET_A),
                (i, OFFSET_B),
                (0x0000, OFFSET_C),
                (d_blok, OFFSET_D),
            ] {
                let blok = ((slovo as u32) << 10) | ((syndrome(slovo) ^ off) as u32);
                for k in (0..26).rev() {
                    bits.push(blok & (1 << k) != 0);
                }
            }
        }
    }
    // Diferenciální kódování: vysílá se změna, ne úroveň.
    let mut level = false;
    let mut levels = Vec::with_capacity(bits.len() + 1);
    levels.push(level);
    for b in &bits {
        if *b {
            level = !level;
        }
        levels.push(level);
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generující polynom musí sedět na normu, ne jen sám na sebe.
    ///
    /// Tenhle test tu je proto, že ostatní testy RDS jsou nutně kruhové:
    /// generují bloky stejným polynomem, kterým je pak ověřují, takže projdou
    /// i s úplně vymyšlenou hodnotou. Přesně tak tu žil překlep 0x6B9 místo
    /// 0x5B9 - na papíře všechno zelené, v éteru se nedekódovalo nic.
    /// Proto se hodnota skládá nezávisle ze seznamu mocnin podle normy.
    #[test]
    fn polynom_odpovida_norme() {
        // g(x) = x¹⁰ + x⁸ + x⁷ + x⁵ + x⁴ + x³ + 1
        let mocniny = [10u32, 8, 7, 5, 4, 3, 0];
        let podle_normy: u32 = mocniny.iter().map(|&k| 1u32 << k).sum();
        assert_eq!(
            RDS_POLY, podle_normy,
            "polynom je 0x{RDS_POLY:03X}, podle normy má být 0x{podle_normy:03X}"
        );
    }

    /// Kontrolní součet musí odpovídat normě. Kdyby byl polynom špatně,
    /// nesedělo by vůbec nic a dekodér by mlčel - tohle to chytí hned.
    #[test]
    fn syndrom_je_konzistentni() {
        // Blok složený jako data + (syndrom ^ offset) musí projít kontrolou.
        for data in [0x0000u16, 0xFFFF, 0x1234, 0xABCD, 0x5A5A] {
            for off in [OFFSET_A, OFFSET_B, OFFSET_C, OFFSET_CP, OFFSET_D] {
                let block = ((data as u32) << 10) | ((syndrome(data) ^ off) as u32);
                assert_eq!(check_block(block, off), Some(data), "data {data:04X}");
            }
        }
    }

    /// Blok s překlepnutým bitem nesmí projít - jinak by se do textu dostávaly
    /// zkomolené znaky.
    #[test]
    fn poskozeny_blok_neprojde() {
        let data = 0x1234u16;
        let block = ((data as u32) << 10) | ((syndrome(data) ^ OFFSET_A) as u32);
        for bit in 0..26 {
            assert_eq!(
                check_block(block ^ (1 << bit), OFFSET_A),
                None,
                "chyba na bitu {bit} prošla"
            );
        }
    }

    /// Špatný offset nesmí blok pustit - na tom stojí rozpoznání pozice
    /// bloku ve skupině.
    #[test]
    fn blok_nesedi_na_cizi_offset() {
        let data = 0x4321u16;
        let block = ((data as u32) << 10) | ((syndrome(data) ^ OFFSET_B) as u32);
        assert_eq!(check_block(block, OFFSET_B), Some(data));
        assert_eq!(check_block(block, OFFSET_A), None);
        assert_eq!(check_block(block, OFFSET_D), None);
    }

    /// Text se musí useknout na 0x0D a netisknutelné znaky nahradit mezerou.
    #[test]
    fn prevod_textu_ceka_ukoncovac() {
        assert_eq!(to_text(b"RADIO 1\r_______"), "RADIO 1");
        assert_eq!(to_text(b"AB\x01CD"), "AB CD");
        assert_eq!(to_text(b"        "), "");
    }

    /// Nakrmí dekodér vyrobeným RDS signálem s názvem stanice.
    ///
    /// `ppm` rozladí bitové hodiny "vysílače" proti vzorkovačce přijímače -
    /// přesně to, co v éteru dělají dva různé krystaly.
    fn posli_nazev(
        d: &mut RdsDecoder,
        if_rate: f64,
        pi: u16,
        jmeno: &[u8; 8],
        skupin: usize,
        ppm: f64,
        faze: f64,
    ) {
        let levels = testovaci_urovne(pi, jmeno, skupin);

        // Biphase: první půlka bitu jedno znaménko, druhá opačné.
        let spb = if_rate / (RDS_BITRATE * (1.0 + ppm * 1e-6));
        let mut start = 0.0f64;
        for lv in levels {
            let amp = if lv { 1.0f32 } else { -1.0 };
            let konec = start + spb;
            let stred = start + spb / 2.0;
            let mut i = start.ceil() as usize;
            while (i as f64) < konec {
                let s = if (i as f64) < stred { amp } else { -amp };
                let ph = 2.0 * std::f64::consts::PI * 57_000.0 * i as f64 / if_rate;
                // Vysílač modulovaný na podnosnou natočenou o `faze` - tak to
                // v éteru vypadá, protože nosná odvozená z pilotu je proti
                // skutečné posunutá o skupinové zpoždění filtrů.
                let vysilana = (ph + faze).cos() as f32;
                // Dekodér dostane obě fáze nosné odvozené z pilotu (bez posunu).
                let c = ph.cos() as f32;
                let sn = ph.sin() as f32;
                d.push(s * vysilana, c, sn);
                i += 1;
            }
            start = konec;
        }
    }

    /// Celá cesta: vyrobím RDS skupiny 0A s názvem stanice, nasyntetizuji
    /// z nich signál na 57 kHz a ověřím, že dekodér přečte název.
    ///
    /// Tohle je jediný test, který ověří všechny části dohromady - obnovu
    /// taktu, diferenciální dekódování i synchronizaci bloků.
    #[test]
    fn precte_nazev_stanice_z_vyrobeneho_signalu() {
        let if_rate = 336_000.0; // jako RSP1 na 1,344 MSps
        let mut d = RdsDecoder::new(if_rate);
        posli_nazev(&mut d, if_rate, 0x1234, b"KNOFLIK ", 10, 0.0, 0.0);
        assert_eq!(d.info().pi, Some(0x1234), "PI kód");
        assert_eq!(d.info().ps, "KNOFLIK", "název stanice");
    }

    /// Podnosná bývá proti nosné odvozené z pilotu natočená o libovolný úhel:
    /// úzká propust na pilot má skupinové zpoždění a norma navíc připouští
    /// kvadraturu. Dekodér si fázi musí dohledat sám - jinak při nešťastném
    /// úhlu vyjde z demodulace skoro nula a nepřečte se nic.
    ///
    /// Tohle je přesně ta chyba, kvůli které RDS na skutečné stanici mlčelo,
    /// i když na papíře všechno sedělo.
    #[test]
    fn precte_nazev_pri_libovolne_fazi_podnosne() {
        let if_rate = 336_000.0;
        let ctvrt = std::f64::consts::FRAC_PI_2;
        for (popis, faze) in [
            ("kvadratura", ctvrt),
            ("opačná", std::f64::consts::PI),
            ("šikmo", 0.7),
            ("záporně šikmo", -1.2),
        ] {
            let mut d = RdsDecoder::new(if_rate);
            posli_nazev(&mut d, if_rate, 0x9ABC, b"KNOFLIK ", 16, 0.0, faze);
            assert_eq!(
                d.info().ps,
                "KNOFLIK",
                "název se nepřečetl při fázi {popis} ({faze:.2} rad)"
            );
        }
    }

    /// Hodiny vysílače a přijímače nikdy nejdou úplně stejně. Bez dorovnávání
    /// taktu by se bity po chvíli rozešly a synchronizace by pořád padala -
    /// tohle je ten rozdíl mezi "funguje na papíře" a "funguje v éteru".
    #[test]
    fn precte_nazev_i_pri_rozladenych_hodinach() {
        let if_rate = 336_000.0;
        // ±100 ppm je víc, než dá běžný krystal dohromady.
        for ppm in [-100.0, -30.0, 30.0, 100.0] {
            let mut d = RdsDecoder::new(if_rate);
            posli_nazev(&mut d, if_rate, 0x5678, b"KNOFLIK ", 12, ppm, 0.0);
            assert_eq!(
                d.info().ps,
                "KNOFLIK",
                "název se nepřečetl při rozladění {ppm} ppm"
            );
        }
    }
}
