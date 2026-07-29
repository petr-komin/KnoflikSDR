//! Vlákno příjmu: zvukový vstup -> DSP -> audio ring + spektrum pro GUI.
//!
//! O to, odkud se vzorky berou a v jakém formátu jsou na drátě, se stará
//! [`crate::audio`]; sem už chodí hotové f32.

use crate::decode::{Decoder, RttyConfig};
use crate::dsp::{AgcMode, Demod, Mode};
use crate::settings::Settings;
use crate::source::{self, Tuner};
use anyhow::Result;
use std::sync::mpsc;
use num_complex::Complex32;
use rustfft::FftPlanner;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Cílová vzorkovací frekvence zvuku na výstupu.
pub const AUDIO_RATE: f64 = 48_000.0;
pub const FFT_SIZE: usize = 2048;
/// Výchozí šířka pásma pro AM (+-4 kHz od nosné).
pub const AM_BANDWIDTH_HZ: f64 = 8_000.0;
/// Výchozí šířka pásma pro SSB - obvyklá hodnota pro fonii.
pub const SSB_BANDWIDTH_HZ: f64 = 2_700.0;
/// Výchozí šířka pásma pro NFM - amatérský kanál 12,5 kHz s rezervou na zdvih.
pub const NFM_BANDWIDTH_HZ: f64 = 16_000.0;
/// Výchozí šířka VKV rozhlasového kanálu - plný Carson, tedy bez ořezu.
pub const WFM_BANDWIDTH_HZ: f64 = 180_000.0;

/// Meze šířky pásma podle režimu.
///
/// Dolní mez drží FIR: přechodové pásmo je ~300 Hz, pod ním by štítek
/// přestal odpovídat skutečnému -6 dB bodu. Horní mez u AM je s rezervou
/// pod Nyquistem po decimaci (48 kHz -> 24 kHz); pro AM je 24 kHz
/// (audio do 12 kHz) stejně víc než dost. U SSB nemá smysl jít nad
/// šířku fonického kanálu.
pub fn bandwidth_range(mode: Mode) -> (f64, f64) {
    match mode {
        Mode::Cw => (CW_MIN_BANDWIDTH_HZ, 2_000.0),
        Mode::Usb | Mode::Lsb => (400.0, 4_000.0),
        Mode::Am => (1_000.0, 24_000.0),
        // WFM: plný kanál je podle Carsona ~180 kHz, ale když sousední stanice
        // přebíjí, vyplatí se přiškrtit. Pod 120 kHz už ale filtr ukrajuje
        // i vlastní zdvih a zvuk začne být zkreslený, tak níž ne.
        Mode::Wfm => (120_000.0, 230_000.0),
        // NFM: od úzkého kanálu (~11 kHz) po širší amatérský (~20 kHz).
        Mode::Nfm => (8_000.0, 20_000.0),
    }
}

/// Meze posuvníku šumové brány v dBFS. Kryjí rozsah S-metru, aby šla čára
/// squelche postavit kamkoli mezi šumové dno a silný signál.
pub const SQUELCH_MIN_DB: f32 = -100.0;
pub const SQUELCH_MAX_DB: f32 = -20.0;
/// Výchozí práh šumové brány - kousek nad typickým šumovým dnem.
pub const DEFAULT_SQUELCH_DB: f32 = -70.0;

/// Meze ručního zisku při vypnuté AGC. Nahoře dost na slabé signály,
/// dole tak, aby šla utlumit silná místní stanice.
pub const AGC_MANUAL_MIN_DB: f32 = 0.0;
pub const AGC_MANUAL_MAX_DB: f32 = 60.0;
/// Výchozí ruční zisk - odpovídá zhruba tomu, co dělá AGC na středně silné stanici.
pub const DEFAULT_AGC_MANUAL_DB: f32 = 20.0;

/// Nejužší poctivý CW filtr. Změřeno: kanálový filtr na 48 kHz s 1023
/// koeficienty trefí -6 dB bod na hertz přesně až sem; při 100 Hz už
/// vyjde 58 místo 50 Hz.
pub const CW_MIN_BANDWIDTH_HZ: f64 = 150.0;
/// Výchozí šířka pro CW - obvyklá volba pro běžný provoz.
pub const CW_BANDWIDTH_HZ: f64 = 500.0;

/// Ovládací prvky, do kterých píše GUI a čte je DSP vlákno.
pub struct Controls {
    /// Odladění od středu (VFO) v Hz.
    pub offset_hz: f64,
    pub volume: f32,
    /// SoftRocky mívají I/Q prohozené - pak je spektrum zrcadlené.
    pub swap_iq: bool,
    /// Celková šířka propustného pásma demodulátoru.
    pub bandwidth_hz: f64,
    pub mode: Mode,
    pub decoder: Decoder,
    pub rtty: RttyConfig,
    /// Squelch CW v dB nad šumovým dnem.
    pub cw_squelch_db: f32,
    /// Práh šumové brány zvuku v dBFS, nebo `None` když je vypnutá.
    pub squelch_db: Option<f32>,
    /// Rychlost AGC, případně její vypnutí.
    pub agc: AgcMode,
    /// Ruční zisk v dB - uplatní se jen při vypnuté AGC.
    pub agc_manual_db: f32,
    /// Kmitočet ruční zádrže na audiu v Hz, nebo `None` když je vypnutá.
    pub notch_hz: Option<f64>,
    /// Přehrávat WFM ve stereu, když je slyšet pilot?
    pub stereo: bool,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            offset_hz: 0.0,
            volume: 0.5,
            swap_iq: false,
            bandwidth_hz: AM_BANDWIDTH_HZ,
            mode: Mode::Am,
            decoder: Decoder::Off,
            rtty: RttyConfig::default(),
            cw_squelch_db: crate::decode::CW_SQUELCH_DB,
            squelch_db: None,
            agc: AgcMode::default(),
            agc_manual_db: DEFAULT_AGC_MANUAL_DB,
            notch_hz: None,
            stereo: true,
        }
    }
}

/// Spektrum v dB, už prohozené tak, aby střed pole = DC = VFO.
pub struct Spectrum {
    pub bins: Vec<f32>,
    pub generation: u64,
}

impl Default for Spectrum {
    fn default() -> Self {
        Spectrum {
            bins: vec![-120.0; FFT_SIZE],
            generation: 0,
        }
    }
}

pub struct Shared {
    pub controls: Mutex<Controls>,
    pub spectrum: Mutex<Spectrum>,
    /// Stav zvukového vstupu.
    pub status: Mutex<String>,
    /// Stav rádia. Zvlášť od `status`, ať si to dvě vlákna nepřepisují.
    pub hw_status: Mutex<String>,
    /// Rozsah zisku otevřeného rádia, nebo `None`, když se zisk neřídí
    /// (SoftRock) nebo rádio ještě není. Plní ladicí vlákno, čte GUI.
    pub gain_range: Mutex<Option<crate::source::GainRange>>,
    /// Text z dekodéru, který si GUI průběžně vyzvedává.
    pub decoded: Mutex<String>,
    /// Skutečná vzorkovací frekvence vstupu = šířka panoramatu.
    pub sample_rate: AtomicU32,
    /// Úroveň naladěného signálu v dBFS před AGC, uložená jako bity f32.
    pub level_dbfs: AtomicU32,
    /// Odhadnuté tempo CW ve WPM (bity f32), 0 = neběží CW dekodér.
    pub cw_wpm: AtomicU32,
    /// Přeje si GUI nahrávat? DSP vlákno podle toho otevře nebo zavře soubor.
    pub recording: AtomicBool,
    /// Kam nahrávat. GUI to nastaví, než zvedne `recording`.
    pub record_path: Mutex<String>,
    /// Co nahrávání dělá - jméno souboru, chyba, nebo prázdno. Plní DSP vlákno.
    pub record_status: Mutex<String>,
    /// Délka běžící nahrávky v sekundách (bity f32).
    pub record_secs: AtomicU32,
    /// Hraje WFM zrovna stereo? (Uživatel ho chce a je slyšet pilot.)
    pub stereo_active: AtomicBool,
    /// Je chycený stereo pilot? Nezávisle na tom, jestli uživatel chce stereo -
    /// RDS na pilotu stojí tak jako tak.
    pub pilot_locked: AtomicBool,
    /// Naměřená amplituda pilotu (bity f32) - pro diagnostiku, proč RDS mlčí.
    pub pilot_level: AtomicU32,
    /// Kolik bloků RDS prošlo kontrolou a kolik ne - taky pro diagnostiku.
    pub rds_good: AtomicU32,
    pub rds_bad: AtomicU32,
    /// Co přečetl RDS - název stanice, RadioText a PI kód.
    pub rds: Mutex<crate::rds::RdsInfo>,
    /// GUI sem dá nastavení, se kterým se má rádio znovu otevřít (jiné rádio,
    /// vzorkovačka, zvukovka...), a zvedne `reopen`. DSP vlákno to převezme.
    pub reopen_config: Mutex<Option<Settings>>,
    /// Žádost o znovuotevření zdroje za běhu. DSP smyčka to hlídá každé čtení.
    pub reopen: AtomicBool,
    pub running: Arc<AtomicBool>,
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            controls: Mutex::new(Controls::default()),
            spectrum: Mutex::new(Spectrum::default()),
            status: Mutex::new("startuji...".to_string()),
            hw_status: Mutex::new("hledám rádio...".to_string()),
            gain_range: Mutex::new(None),
            decoded: Mutex::new(String::new()),
            sample_rate: AtomicU32::new(96_000),
            level_dbfs: AtomicU32::new((-120.0f32).to_bits()),
            cw_wpm: AtomicU32::new(0f32.to_bits()),
            recording: AtomicBool::new(false),
            record_path: Mutex::new(String::new()),
            record_status: Mutex::new(String::new()),
            record_secs: AtomicU32::new(0f32.to_bits()),
            stereo_active: AtomicBool::new(false),
            pilot_locked: AtomicBool::new(false),
            pilot_level: AtomicU32::new(0f32.to_bits()),
            rds_good: AtomicU32::new(0),
            rds_bad: AtomicU32::new(0),
            rds: Mutex::new(crate::rds::RdsInfo::default()),
            reopen_config: Mutex::new(None),
            reopen: AtomicBool::new(false),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn level_dbfs(&self) -> f32 {
        f32::from_bits(self.level_dbfs.load(Ordering::Relaxed))
    }

    pub fn cw_wpm(&self) -> f32 {
        f32::from_bits(self.cw_wpm.load(Ordering::Relaxed))
    }

    pub fn record_secs(&self) -> f32 {
        f32::from_bits(self.record_secs.load(Ordering::Relaxed))
    }
}

/// Kolik vzorků si říct od zdroje najednou.
/// Kolik vzorků si říct od zdroje najednou.
///
/// Zkoušel jsem to škálovat podle vzorkovačky (~5 ms na blok), aby se omezily
/// hlášky "samples lost" z libmirisdr. Skončilo to pádem ovladače (SIGSEGV)
/// při přepínání vzorkovačky za běhu - SoapyMiri zjevně nesnese čtení po
/// blocích výrazně větších, než je jeho USB transfer. Zůstává tedy pevných
/// 1024; ztráty vzorků jsou věc ovladače a USB, ne velikosti bloku.
const READ_FRAMES: usize = 1024;

/// Jak často nejvýš přepočítat panorama. Na 96 kHz vychází FFT každých 2048
/// vzorků na ~47×/s, ale na 1,344 MSps z RSP1 by to bylo 656×/s - zbytečně,
/// GUI stejně kreslí ~60×/s. Bez tohohle by FFT sežrala jádro nadarmo.
const FFT_INTERVAL: Duration = Duration::from_millis(16);

pub fn spawn(
    set: Settings,
    shared: Arc<Shared>,
    audio_tx: rtrb::Producer<f32>,
    tuner_tx: mpsc::Sender<Box<dyn Tuner>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut audio_tx = audio_tx;
        let mut set = set;
        while shared.running.load(Ordering::Relaxed) {
            // Nové nastavení od GUI (přepnutí rádia, vzorkovačky, zvukovky)?
            if let Some(cfg) = shared.reopen_config.lock().unwrap().take() {
                set = cfg;
            }
            shared.reopen.store(false, Ordering::Relaxed);

            match run(&set, &shared, &mut audio_tx, &tuner_tx) {
                // Ok = buď se končí (running=false), nebo přišla žádost o
                // přepnutí; o tom rozhodne vnější while, ne break.
                Ok(()) => {}
                Err(e) => {
                    *shared.status.lock().unwrap() = format!("chyba vstupu: {e}");
                    // Krátké kroky, ať přepnutí z rozbitého rádia není líné.
                    for _ in 0..20 {
                        if !shared.running.load(Ordering::Relaxed)
                            || shared.reopen.load(Ordering::Relaxed)
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    })
}

fn run(
    set: &Settings,
    shared: &Arc<Shared>,
    audio_tx: &mut rtrb::Producer<f32>,
    tuner_tx: &mpsc::Sender<Box<dyn Tuner>>,
) -> Result<()> {
    // Obě půlky rádia se otevírají naráz; ladění pak předáme svému vláknu,
    // ať nás zápis po USB nebrzdí v téhle smyčce.
    let (mut src, tuner) = source::open(set.hardware, set)?;
    *shared.hw_status.lock().unwrap() = tuner.label();
    let _ = tuner_tx.send(tuner);

    let actual_rate = src.rate();
    shared
        .sample_rate
        .store(actual_rate as u32, Ordering::Relaxed);
    *shared.status.lock().unwrap() = src.label();

    // Decimace na ~48 kHz audio.
    let decim = ((actual_rate / AUDIO_RATE).round() as usize).max(1);

    let mut rx = Demod::new(actual_rate, decim, AM_BANDWIDTH_HZ, Mode::Am);
    let mut raw = vec![Complex32::new(0.0, 0.0); READ_FRAMES];
    let mut last_fft = std::time::Instant::now();

    let mut audio: Vec<f32> = Vec::with_capacity(1024);
    let mut iq: Vec<Complex32> = Vec::with_capacity(1024);

    // Skutečná vzorkovačka zvuku - pod ní se ukládá WAV.
    let out_rate = actual_rate / decim as f64;
    let mut recorder: Option<crate::record::WavWriter> = None;
    // Mono směs pro nahrávání; drží se mimo smyčku, ať se nealokuje pořád dokola.
    let mut mono: Vec<f32> = Vec::with_capacity(1024);
    let mut rds_log = RdsLog::new();

    // FFT pro panorama
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (FFT_SIZE - 1) as f32).cos())
        .collect();
    let mut fft_buf: Vec<Complex32> = Vec::with_capacity(FFT_SIZE);
    let mut smoothed = vec![-120.0f32; FFT_SIZE];
    let mut generation = 0u64;

    while shared.running.load(Ordering::Relaxed) && !shared.reopen.load(Ordering::Relaxed) {
        // Nula znamená, že se vstup po zádrhelu zotavil a data přijdou příště.
        let frames = src.read(&mut raw)?;
        if frames == 0 {
            continue;
        }

        let (
            offset,
            volume,
            swap,
            bandwidth,
            mode,
            decoder,
            rtty,
            squelch,
            audio_squelch,
            agc,
            agc_manual_db,
            notch_hz,
            stereo,
        ) = {
            let c = shared.controls.lock().unwrap();
            (
                c.offset_hz,
                c.volume,
                c.swap_iq,
                c.bandwidth_hz,
                c.mode,
                c.decoder,
                c.rtty,
                c.cw_squelch_db,
                c.squelch_db,
                c.agc,
                c.agc_manual_db,
                c.notch_hz,
                c.stereo,
            )
        };
        rx.set_offset(offset);
        rx.set_mode(mode);
        rx.set_bandwidth(bandwidth);
        rx.set_decoder(decoder, rtty, squelch);
        rx.set_squelch(audio_squelch);
        rx.set_agc(agc, agc_manual_db);
        rx.set_notch(notch_hz);
        rx.set_stereo(stereo);

        iq.clear();
        iq.extend(raw[..frames].iter().map(|c| {
            // SoftRocky mívají I/Q prohozené - pak je spektrum zrcadlené.
            if swap {
                Complex32::new(c.im, c.re)
            } else {
                *c
            }
        }));

        // Panorama
        for &s in &iq {
            fft_buf.push(s);
            if fft_buf.len() == FFT_SIZE {
                // Na vysoké vzorkovačce by okno došlo mnohem častěji, než má
                // GUI co kreslit. Přeskočené vzorky nevadí - panorama je
                // průběžný pohled, ne souvislý záznam.
                if last_fft.elapsed() < FFT_INTERVAL {
                    fft_buf.clear();
                    continue;
                }
                last_fft = std::time::Instant::now();
                let mut scratch: Vec<Complex32> =
                    fft_buf.iter().zip(&window).map(|(s, w)| s * *w).collect();
                fft.process(&mut scratch);
                let norm = 1.0 / FFT_SIZE as f32;
                let half = FFT_SIZE / 2;
                for i in 0..FFT_SIZE {
                    // fftshift: DC doprostřed
                    let src = (i + half) % FFT_SIZE;
                    let mag = (scratch[src] * norm).norm().max(1e-12);
                    let db = 20.0 * mag.log10();
                    smoothed[i] = smoothed[i] * 0.6 + db * 0.4;
                }
                fft_buf.clear();
                generation += 1;
                if let Ok(mut sp) = shared.spectrum.try_lock() {
                    sp.bins.copy_from_slice(&smoothed);
                    sp.generation = generation;
                }
            }
        }

        // Demodulace
        audio.clear();
        rx.process(&iq, &mut audio);
        shared
            .level_dbfs
            .store(rx.level_dbfs().to_bits(), Ordering::Relaxed);

        shared.cw_wpm.store(
            (rx.cw_wpm().unwrap_or(0.0) as f32).to_bits(),
            Ordering::Relaxed,
        );

        // Stav stereo a RDS pro GUI. RDS se přepisuje jen když se změnil,
        // ať se GUI nebere zámek na každém bloku vzorků zbytečně.
        shared
            .stereo_active
            .store(rx.stereo_active(), Ordering::Relaxed);
        shared
            .pilot_locked
            .store(rx.pilot_locked(), Ordering::Relaxed);
        shared
            .pilot_level
            .store(rx.pilot_level().to_bits(), Ordering::Relaxed);
        let (rds_ok, rds_bad) = rx.rds_blocks();
        shared.rds_good.store(rds_ok, Ordering::Relaxed);
        shared.rds_bad.store(rds_bad, Ordering::Relaxed);

        // Diagnostický záznam WFM/RDS. Zapíná se proměnnou prostředí, ať
        // konzoli nezaplavuje - ta je od ovladače hlučná i bez nás.
        if let Some(log) = rds_log.as_mut() {
            log.tick(&rx, mode);
        }
        if mode.is_wfm() {
            let info = rx.rds();
            if let Ok(mut r) = shared.rds.try_lock() {
                if *r != *info {
                    *r = info.clone();
                }
            }
        }

        // Přečtený text předáme GUI. Kdyby si ho nikdo nebral, necháme
        // ho useknout, ať paměť neroste donekonečna.
        let text = rx.take_text();
        if !text.is_empty() {
            if let Ok(mut d) = shared.decoded.lock() {
                d.push_str(&text);
                if d.len() > 8192 {
                    let cut = d.len() - 4096;
                    *d = d[cut..].to_string();
                }
            }
        }
        // Nahrávání. Ukládá se zvuk před regulací hlasitosti, ať nahrávka
        // nezávisí na tom, jak je zrovna stažený knoflík. WAV je mono, tak
        // se stereo dvojice smíchá; u mono režimů jsou oba kanály stejné,
        // takže se nic nezmění.
        if shared.recording.load(Ordering::Relaxed) || recorder.is_some() {
            mono.clear();
            mono.extend(audio.chunks_exact(2).map(|p| (p[0] + p[1]) * 0.5));
            run_recorder(&shared, &mut recorder, out_rate, &mono);
        }

        // Zvuk chodí jako prokládané dvojice L, R. Do ringu se strkají celé
        // dvojice: kdyby se vešla jen půlka, prohodily by se od té chvíle
        // kanály natrvalo.
        for pair in audio.chunks_exact(2) {
            if audio_tx.slots() < 2 {
                break; // ring plný - zbytek zahodíme, výstup si drží tempo sám
            }
            let _ = audio_tx.push(pair[0] * volume);
            let _ = audio_tx.push(pair[1] * volume);
        }
    }
    // Když se vlákno končí (zavřené okno, přepnutí rádia), nahrávku pořádně
    // uzavřeme - jinak by v hlavičce zůstaly nuly a soubor by byl nepoužitelný.
    if let Some(w) = recorder.take() {
        let _ = w.finish();
    }
    Ok(())
}

/// Diagnostický záznam stavu WFM a RDS.
///
/// Ladit RDS "podle toho, co je vidět v okně" nejde - ukazatel řekne jen
/// "hledám" a člověk pak hádá, jestli je problém v pilotu, v nosné, v taktu
/// nebo v síle signálu. Tenhle záznam píše jednou za vteřinu tvrdá čísla.
///
/// Zapíná se proměnnou `KNOFLIK_RDS_LOG`: buď cesta k souboru, nebo `1`
/// pro výpis na chybový výstup. Ve výchozím stavu neběží vůbec.
struct RdsLog {
    out: Option<Box<dyn std::io::Write + Send>>,
    last: std::time::Instant,
    /// Počty z minulého záznamu, aby šlo spočítat přírůstek za vteřinu.
    prev_ok: u32,
    prev_bad: u32,
}

impl RdsLog {
    fn new() -> Option<Self> {
        let kam = std::env::var("KNOFLIK_RDS_LOG").ok()?;
        let out: Box<dyn std::io::Write + Send> = if kam == "1" {
            Box::new(std::io::stderr())
        } else {
            match std::fs::File::create(&kam) {
                Ok(f) => Box::new(std::io::BufWriter::new(f)),
                Err(e) => {
                    eprintln!("RDS log: {kam} nejde otevřít: {e}");
                    return None;
                }
            }
        };
        eprintln!("RDS log zapnut -> {kam}");
        Some(RdsLog {
            out: Some(out),
            last: std::time::Instant::now(),
            prev_ok: 0,
            prev_bad: 0,
        })
    }

    fn tick(&mut self, rx: &Demod, mode: Mode) {
        if !mode.is_wfm() || self.last.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last = std::time::Instant::now();
        let Some(out) = self.out.as_mut() else {
            return;
        };
        let (ok, bad) = rx.rds_blocks();
        let (d_ok, d_bad) = (ok.wrapping_sub(self.prev_ok), bad.wrapping_sub(self.prev_bad));
        self.prev_ok = ok;
        self.prev_bad = bad;
        let info = rx.rds();
        let (oko, syncu, drzi) = rx.rds_quality();
        // Podíl prošlých bloků za poslední vteřinu je to hlavní číslo:
        // pod ~50 % se název stanice prakticky nesloží, protože skupina
        // potřebuje platné bloky A, B i D najednou.
        let celkem = d_ok + d_bad;
        let podil = if celkem > 0 {
            d_ok as f32 / celkem as f32 * 100.0
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "{} | signál {:6.1} dBFS | pilot {:.4} {} | stereo {} | bloky {:3}/{:3} ({:5.1} %) | oko {:.2} | sync {:3}{} | PI {} | PS {:?} | RT {:?}",
            chrono::Local::now().format("%H:%M:%S"),
            rx.level_dbfs(),
            rx.pilot_level(),
            if rx.pilot_locked() { "OK " } else { "-- " },
            if rx.stereo_active() { "ANO" } else { "ne " },
            d_ok,
            celkem,
            podil,
            oko,
            syncu,
            if drzi { "*" } else { " " },
            match info.pi {
                Some(pi) => format!("{pi:04X}"),
                None => "----".to_string(),
            },
            info.ps,
            info.rt,
        );
        let _ = out.flush();
    }
}

/// Otevře, krmí nebo zavře nahrávku podle toho, co si přeje GUI.
///
/// Běží v DSP vlákně, takže se chyba nesmí propsat výš - zápis do souboru
/// nesmí shodit příjem. Když se něco nepovede, řekne se to do stavu a
/// nahrávání se vypne.
fn run_recorder(
    shared: &Arc<Shared>,
    recorder: &mut Option<crate::record::WavWriter>,
    out_rate: f64,
    audio: &[f32],
) {
    let chce = shared.recording.load(Ordering::Relaxed);
    match (chce, recorder.is_some()) {
        // Začít nahrávat.
        (true, false) => {
            let path = shared.record_path.lock().unwrap().clone();
            match crate::record::WavWriter::create(&path, out_rate as u32) {
                Ok(w) => {
                    *shared.record_status.lock().unwrap() = format!(
                        "nahrávám {}",
                        w.path()
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or(path.clone())
                    );
                    *recorder = Some(w);
                }
                Err(e) => {
                    *shared.record_status.lock().unwrap() = format!("nahrávání selhalo: {e}");
                    shared.recording.store(false, Ordering::Relaxed);
                }
            }
        }
        // Dopsat a zavřít.
        (false, true) => {
            if let Some(w) = recorder.take() {
                let secs = w.seconds();
                match w.finish() {
                    Ok(p) => {
                        *shared.record_status.lock().unwrap() = format!(
                            "uloženo {} ({:.0} s)",
                            p.file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default(),
                            secs
                        )
                    }
                    Err(e) => {
                        *shared.record_status.lock().unwrap() = format!("uložení selhalo: {e}")
                    }
                }
            }
            shared.record_secs.store(0f32.to_bits(), Ordering::Relaxed);
        }
        // Nahrávat dál.
        (true, true) => {
            if let Some(w) = recorder.as_mut() {
                if let Err(e) = w.write(audio) {
                    *shared.record_status.lock().unwrap() = format!("zápis selhal: {e}");
                    shared.recording.store(false, Ordering::Relaxed);
                    *recorder = None;
                } else {
                    shared
                        .record_secs
                        .store(w.seconds().to_bits(), Ordering::Relaxed);
                }
            }
        }
        (false, false) => {}
    }
}

#[cfg(all(test, feature = "rsp1"))]
mod switch_tests {
    use super::*;

    /// Přepnutí rádia za běhu, celou cestou přes skutečný hardware. Vyžaduje
    /// připojený SoftRock i RSP1, proto `#[ignore]` - `cargo test` bez rádia
    /// by na tom padal. Cesta ke zvukovce SoftRocku se bere z env
    /// `KNOFLIK_TEST_CAPTURE`; bez ní se test přeskočí.
    ///
    /// Spustit: `KNOFLIK_TEST_CAPTURE=hw:CARD=HD,DEV=0 cargo test --release \
    ///           prepnuti_radia_za_behu -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn prepnuti_radia_za_behu() {
        let Ok(capture) = std::env::var("KNOFLIK_TEST_CAPTURE") else {
            eprintln!("KNOFLIK_TEST_CAPTURE není nastaveno, přeskakuji");
            return;
        };

        let shared = Arc::new(Shared::new());
        let (audio_tx, _audio_rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let (tuner_tx, _tuner_rx) = mpsc::channel::<Box<dyn Tuner>>();

        let mut set = Settings {
            hardware: crate::source::Hardware::SoftRock,
            capture_device: capture,
            ..Settings::default()
        };
        let h = spawn(set.clone(), shared.clone(), audio_tx, tuner_tx);

        // Počká, až se ve stavu objeví hledaný text, nebo to po `secs` vzdá.
        let wait_for = |slovo: &str, secs: u64| -> bool {
            let konec = std::time::Instant::now() + Duration::from_secs(secs);
            while std::time::Instant::now() < konec {
                if shared.status.lock().unwrap().contains(slovo) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        };

        let switch = |s: &mut Settings, hw| {
            s.hardware = hw;
            *shared.reopen_config.lock().unwrap() = Some(s.clone());
            shared.reopen.store(true, Ordering::Relaxed);
        };

        assert!(wait_for("ALSA", 6), "SoftRock nenaběhl: {}", shared.status.lock().unwrap());
        let r_soft = shared.sample_rate.load(Ordering::Relaxed);
        eprintln!("SoftRock: {} @ {r_soft} Hz", shared.status.lock().unwrap());

        switch(&mut set, crate::source::Hardware::Rsp1);
        assert!(wait_for("RSP1", 8), "nepřepnulo na RSP1: {}", shared.status.lock().unwrap());
        let r_rsp = shared.sample_rate.load(Ordering::Relaxed);
        assert_eq!(r_rsp, 1_344_000, "RSP1 běží na jiné vzorkovačce");
        eprintln!("RSP1: {} @ {r_rsp} Hz", shared.status.lock().unwrap());

        switch(&mut set, crate::source::Hardware::SoftRock);
        assert!(wait_for("ALSA", 8), "nepřepnulo zpět na SoftRock: {}", shared.status.lock().unwrap());
        eprintln!("zpět SoftRock: {} @ {} Hz", shared.status.lock().unwrap(), shared.sample_rate.load(Ordering::Relaxed));

        shared.running.store(false, Ordering::Relaxed);
        let _ = h.join();
    }

    /// WFM demodulace ze skutečné FM stanice. Naladí RSP1 na frekvenci z env
    /// `KNOFLIK_TEST_FM_KHZ` (bez ní se přeskočí) a ověří, že z toho leze
    /// programový zvuk, ne šum: po deemfázi má řeč/hudba drtivou většinu
    /// energie v nízkém pásmu, kdežto neuzamčený FM šum je širokopásmový.
    ///
    /// Spustit: `KNOFLIK_TEST_FM_KHZ=98000 cargo test --release \
    ///           wfm_ze_stanice -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn wfm_ze_stanice() {
        let Ok(khz) = std::env::var("KNOFLIK_TEST_FM_KHZ") else {
            eprintln!("KNOFLIK_TEST_FM_KHZ není nastaveno, přeskakuji");
            return;
        };
        let freq_hz: f64 = khz.parse::<f64>().unwrap() * 1000.0;

        let set = Settings {
            hardware: crate::source::Hardware::Rsp1,
            ..Settings::default()
        };
        let (mut src, mut tuner) = crate::source::open(set.hardware, &set).unwrap();
        tuner.set_center(freq_hz).unwrap();

        let rate = src.rate();
        let decim = (rate / AUDIO_RATE).round() as usize;

        let mut iq = vec![Complex32::new(0.0, 0.0); 8192];

        // FM stanice nesedí přesně na středu okna, tak si nejsilnější nosnou
        // najdi ve spektru (mimo DC spur RSP1) a dolaď se na ni offsetem.
        let nfft = 4096;
        let mut acc = vec![0.0f32; nfft];
        let mut fftbuf = vec![Complex32::new(0.0, 0.0); nfft];
        let mut fills = 0;
        while fills < 8 {
            let n = src.read(&mut iq).unwrap();
            if n < nfft {
                continue;
            }
            fftbuf.copy_from_slice(&iq[..nfft]);
            FftPlanner::new().plan_fft_forward(nfft).process(&mut fftbuf);
            for (a, c) in acc.iter_mut().zip(&fftbuf) {
                *a += c.norm_sqr();
            }
            fills += 1;
        }
        // Nejsilnější bin, ale ne kolem DC (±60 kHz), kde je spur.
        let guard_bins = (60_000.0 / rate * nfft as f64) as usize;
        let mut best = 0usize;
        let mut best_e = 0.0f32;
        for (i, &e) in acc.iter().enumerate() {
            let from_dc = i.min(nfft - i);
            if from_dc < guard_bins {
                continue;
            }
            if e > best_e {
                best_e = e;
                best = i;
            }
        }
        // Bin -> offset v Hz (kladné i záporné).
        let offset = if best <= nfft / 2 {
            best as f64
        } else {
            best as f64 - nfft as f64
        } * rate
            / nfft as f64;
        eprintln!("nejsilnější nosná na offsetu {:.0} kHz od {khz} kHz", offset / 1000.0);

        let mut demod = Demod::new(rate, decim, 180_000.0, Mode::Wfm);
        demod.set_offset(offset);

        let mut audio: Vec<f32> = Vec::new();
        // Zahoď první půlvteřinu (ustálení filtrů), pak sbírej ~2 s.
        let mut zahozeno = 0usize;
        while audio.len() < 96_000 {
            let n = src.read(&mut iq).unwrap();
            if n == 0 {
                continue;
            }
            let mut a = Vec::new();
            demod.process(&iq[..n], &mut a);
            if zahozeno < 24_000 {
                zahozeno += a.len();
            } else {
                audio.extend_from_slice(&a);
            }
        }

        let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
        // Spektrum zvuku: poměr energie v řečovém pásmu (0,3-4 kHz) k výškám
        // (8-15 kHz). Program má nízké pásmo mnohem silnější, šum ne.
        let m = 32768.min(audio.len() & !1);
        let mut buf: Vec<Complex32> =
            audio[..m].iter().map(|&s| Complex32::new(s, 0.0)).collect();
        FftPlanner::new().plan_fft_forward(m).process(&mut buf);
        let bin = |hz: f64| (hz / 48_000.0 * m as f64) as usize;
        let energy = |lo: f64, hi: f64| -> f32 {
            buf[bin(lo)..bin(hi)].iter().map(|c| c.norm_sqr()).sum()
        };
        let low = energy(300.0, 4_000.0);
        let high = energy(8_000.0, 15_000.0).max(1e-12);
        let ratio = low / high;

        eprintln!(
            "FM {khz} kHz: audio RMS {rms:.4}, energie nízké/vysoké = {ratio:.1}×"
        );
        assert!(rms > 0.01, "zvuk je prakticky ticho (RMS {rms}) - stanice tam není?");
        assert!(
            ratio > 4.0,
            "energie není soustředěná v programovém pásmu ({ratio:.1}×) - vypadá to jako šum, ne demodulovaná stanice"
        );
    }

    /// NFM ze skutečného vysílání na 2 m/70 cm. Naladí na nejsilnější nosnou
    /// v okně kolem `KNOFLIK_TEST_NFM_KHZ` a demoduluje ji úzkopásmovou FM.
    /// Shovívavý: na amatérských pásmech nemusí zrovna nikdo mluvit, takže jen
    /// ověří, že to běží a vypíše charakteristiku - obsah posoudí člověk uchem.
    ///
    /// Spustit: `KNOFLIK_TEST_NFM_KHZ=145000 cargo test --release \
    ///           nfm_z_vysilani -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn nfm_z_vysilani() {
        let Ok(khz) = std::env::var("KNOFLIK_TEST_NFM_KHZ") else {
            eprintln!("KNOFLIK_TEST_NFM_KHZ není nastaveno, přeskakuji");
            return;
        };
        let freq_hz: f64 = khz.parse::<f64>().unwrap() * 1000.0;

        let set = Settings {
            hardware: crate::source::Hardware::Rsp1,
            rsp1_gain_db: 40.0,
            ..Settings::default()
        };
        let (mut src, mut tuner) = crate::source::open(set.hardware, &set).unwrap();
        tuner.set_center(freq_hz).unwrap();
        let rate = src.rate();
        let decim = (rate / AUDIO_RATE).round() as usize;

        let mut iq = vec![Complex32::new(0.0, 0.0); 8192];
        // Najdi nejsilnější nosnou v okně mimo DC spur.
        let nfft = 4096;
        let mut acc = vec![0.0f32; nfft];
        let mut fb = vec![Complex32::new(0.0, 0.0); nfft];
        for _ in 0..8 {
            let n = src.read(&mut iq).unwrap();
            if n < nfft {
                continue;
            }
            fb.copy_from_slice(&iq[..nfft]);
            FftPlanner::new().plan_fft_forward(nfft).process(&mut fb);
            for (a, c) in acc.iter_mut().zip(&fb) {
                *a += c.norm_sqr();
            }
        }
        let guard = (30_000.0 / rate * nfft as f64) as usize;
        let mut best = 0;
        let mut be = 0.0f32;
        for (i, &e) in acc.iter().enumerate() {
            if i.min(nfft - i) < guard {
                continue;
            }
            if e > be {
                be = e;
                best = i;
            }
        }
        let offset = if best <= nfft / 2 { best as f64 } else { best as f64 - nfft as f64 }
            * rate
            / nfft as f64;

        let mut demod = Demod::new(rate, decim, NFM_BANDWIDTH_HZ, Mode::Nfm);
        demod.set_offset(offset);
        let mut audio: Vec<f32> = Vec::new();
        let mut skip = 0usize;
        while audio.len() < 96_000 {
            let n = src.read(&mut iq).unwrap();
            if n == 0 {
                continue;
            }
            let mut a = Vec::new();
            demod.process(&iq[..n], &mut a);
            if skip < 24_000 {
                skip += a.len();
            } else {
                audio.extend_from_slice(&a);
            }
        }
        let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
        let sig_dbfs = demod.level_dbfs();
        eprintln!(
            "NFM u {khz} kHz: nosná na offsetu {:.0} kHz, síla {sig_dbfs:.0} dBFS, audio RMS {rms:.4}",
            offset / 1000.0
        );
        assert!(rms.is_finite() && rms >= 0.0, "audio je rozbité (RMS {rms})");
    }
}
