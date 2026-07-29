//! KnoflikSDR - přijímač pro SoftRock.
//!
//! I/Q ze zvukové karty, ladění Si570 přes USB, panorama + vodopád,
//! režimy AM/USB/LSB a oblíbené stanice.

mod audio;
mod bandplan;
mod decode;
mod dsp;
mod radio;
mod rds;
mod record;
mod settings;
mod schedule;
mod si570;
mod source;

use settings::{Autosave, Settings, Station};

use eframe::egui;
use radio::{Shared, FFT_SIZE};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

const WF_HEIGHT: usize = 256;
/// Výška pruhu s frekvenční osou pod spektrem.
const AXIS_H: f32 = 16.0;
/// Mrtvá zóna kolem VFO (= DC). SoftRock tu má spur a nevyvážení I/Q,
/// takže se sem stanice ladit nemá. Vynechává ji i hledání nejsilnějšího signálu.
const DC_GUARD_HZ: f64 = 2_000.0;
/// Kam od VFO posadit stanici při skoku na oblíbenou. Musí to být mimo
/// mrtvou zónu kolem DC, jinak by ji sežral spur.
const PARK_OFFSET_HZ: f64 = 10_000.0;
/// Jak dlouho počkat po skoku, než se panorama ustálí a dá se v něm hledat.
const SNAP_DELAY_MS: u64 = 400;
/// Nejvyšší přiblížení panoramatu. Nad tím už je vidět jen rozmazaný jeden bin.
const MAX_ZOOM: f32 = 32.0;
/// O kolik se smí přeladit, aby přečtené RDS ještě platilo. VKV rozhlas má
/// kanály po 100 kHz, takže půlka rozestupu je bezpečná mez: doladění téže
/// stanice se do ní vejde, skok na jinou už ne.
const RDS_KEEP_KHZ: f64 = 50.0;

/// Jak daleko od naladěné frekvence hledat v rozpisu. Pokrývá nepřesnost
/// ladění i to, že se stanice od rozpisu občas o kousek liší.
const SCHEDULE_TOLERANCE_KHZ: f64 = 2.0;

/// Stav načítání rozpisu EiBi.
enum ScheduleState {
    Loading,
    Ready(schedule::Schedule),
    Failed(String),
}

fn main() -> eframe::Result {
    let shared = Arc::new(Shared::new());

    // Nastavení nese i zvuková zařízení, takže ho potřebujeme před vlákny.
    let saved = Settings::load();

    // Audio ring: ~0.5 s rezerva na 48 kHz. Zvuk teče jako prokládané dvojice
    // L, R, takže na jeden rámec padnou dva f32 - odtud dvojnásobek.
    let (audio_tx, audio_rx) = rtrb::RingBuffer::<f32>::new(48_000);

    audio::spawn(
        audio_rx,
        saved.playback_device.clone(),
        radio::AUDIO_RATE as u32,
        shared.running.clone(),
    );
    // Rádio otevírá DSP vlákno a ladicí půlku pošle sem přes tuner_tx.
    let (tuner_tx, tuner_rx) = mpsc::channel::<Box<dyn source::Tuner>>();
    let (tuner, gain_tx) = spawn_tuner(shared.clone(), tuner_rx);
    radio::spawn(saved.clone(), shared.clone(), audio_tx, tuner_tx);

    // Diagnostika bez GUI: ukáže, co si capture vyjednal a jestli teče signál.
    if std::env::args().any(|a| a == "--probe") {
        probe(&shared);
        shared.running.store(false, Ordering::Relaxed);
        return Ok(());
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([saved.window_w, saved.window_h]),
        ..Default::default()
    };
    let app_shared = shared.clone();
    let result = eframe::run_native(
        "KnoflikSDR",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(cc, app_shared, tuner, gain_tx, saved)))),
    );
    shared.running.store(false, Ordering::Relaxed);
    result
}

/// Vypíše po pár sekundách stav řetězce - k ověření bez spouštění GUI.
fn probe(shared: &Arc<Shared>) {
    for i in 0..5 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let status = shared.status.lock().unwrap().clone();
        let rate = shared.sample_rate.load(Ordering::Relaxed);
        let sp = shared.spectrum.lock().unwrap();
        // Úroveň v panoramatu: špička a medián napoví, jestli teče signál
        // nebo jen šum, případně nuly.
        let peak = sp.bins.iter().cloned().fold(f32::MIN, f32::max);
        let mut sorted = sp.bins.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];
        println!(
            "[{i}] rate={rate} Hz  FFT#{}  špička={peak:.1} dB  medián={median:.1} dB  | {status}",
            sp.generation
        );
    }
}

/// Načte rozpis EiBi na pozadí - z cache, nebo ze sítě. Start aplikace
/// na to nesmí čekat.
fn spawn_schedule_load() -> Arc<std::sync::Mutex<ScheduleState>> {
    let state = Arc::new(std::sync::Mutex::new(ScheduleState::Loading));
    let s = state.clone();
    std::thread::spawn(move || {
        let r = match schedule::load_or_fetch() {
            Ok(sch) => ScheduleState::Ready(sch),
            Err(e) => ScheduleState::Failed(format!("{e}")),
        };
        *s.lock().unwrap() = r;
    });
    state
}

/// Ladicí vlákno. USB control transfer trvá jednotky ms, takže nesmí
/// běžet v GUI ani v audio cestě.
/// Vlákno ladění. Samo rádio neotevírá - [`radio::spawn`] otevře obě půlky
/// naráz a ladicí půlku sem pošle přes `tuner_rx`. Když se vstup po výpadku
/// otevře znovu, přijde nový `Tuner` a ten starý se zahodí.
///
/// Je to zvlášť kvůli tomu, že u SoftRocku je ladění zápis do Si570 po USB -
/// trvá jednotky ms a v DSP vlákně by cvakalo do zvuku.
fn spawn_tuner(
    shared: Arc<Shared>,
    tuner_rx: mpsc::Receiver<Box<dyn source::Tuner>>,
) -> (mpsc::Sender<f64>, mpsc::Sender<f64>) {
    let (freq_tx, freq_rx) = mpsc::channel::<f64>();
    let (gain_tx, gain_rx) = mpsc::channel::<f64>();
    std::thread::spawn(move || {
        let mut tuner: Option<Box<dyn source::Tuner>> = None;
        // Poslední přání od GUI, kdyby přišlo dřív než samo rádio.
        let mut want_freq: Option<f64> = None;
        let mut want_gain: Option<f64> = None;
        while shared.running.load(Ordering::Relaxed) {
            // Nové rádio? Převezmi ho a rovnou nalaď, kde jsme byli.
            while let Ok(t) = tuner_rx.try_recv() {
                *shared.gain_range.lock().unwrap() = t.gain_range();
                tuner = Some(t);
                if let Some(f) = want_freq {
                    apply(&mut tuner, &shared, |t| t.set_center(f));
                }
                if let Some(g) = want_gain {
                    apply(&mut tuner, &shared, |t| t.set_gain(g));
                }
            }
            while let Ok(f) = freq_rx.try_recv() {
                want_freq = Some(f);
                apply(&mut tuner, &shared, |t| t.set_center(f));
            }
            while let Ok(g) = gain_rx.try_recv() {
                want_gain = Some(g);
                apply(&mut tuner, &shared, |t| t.set_gain(g));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    (freq_tx, gain_tx)
}

/// Provede úkon na ladění, pokud nějaké máme, a chybu ukáže ve stavu.
fn apply(
    tuner: &mut Option<Box<dyn source::Tuner>>,
    shared: &Arc<Shared>,
    f: impl FnOnce(&mut Box<dyn source::Tuner>) -> anyhow::Result<()>,
) {
    let Some(t) = tuner else { return };
    if let Err(e) = f(t) {
        *shared.hw_status.lock().unwrap() = format!("{e}");
    }
}

/// Výčet zvukových zařízení pro nastavení.
struct Devices {
    capture: Vec<audio::DeviceInfo>,
    playback: Vec<audio::DeviceInfo>,
}

impl Devices {
    fn enumerate() -> Self {
        Devices {
            capture: audio::list_capture(),
            playback: audio::list_playback(),
        }
    }
}

/// To z nastavení, co se čte při otevření rádia (vstupní strana). Když se
/// tohle liší od stavu, se kterým rádio zrovna běží, nabídne okno „Použít".
/// Výstupní zařízení tu schválně není - to se přes reopen neřeší, mění se
/// až restartem.
#[derive(PartialEq, Clone)]
struct StartupConfig {
    hardware: source::Hardware,
    rsp1_rate_hz: f64,
    capture_device: String,
    depth: audio::Depth,
    si570_xtal_hz: f64,
    si570_i2c_addr: u16,
}

impl StartupConfig {
    fn of(s: &Settings) -> Self {
        StartupConfig {
            hardware: s.hardware,
            rsp1_rate_hz: s.rsp1_rate_hz,
            capture_device: s.capture_device.clone(),
            depth: s.depth,
            si570_xtal_hz: s.si570_xtal_hz,
            si570_i2c_addr: s.si570_i2c_addr,
        }
    }
}

struct App {
    shared: Arc<Shared>,
    tuner: mpsc::Sender<f64>,
    /// Zisk jde mimo `tuner` vlastním kanálem - projeví se hned, bez restartu.
    gain_tx: mpsc::Sender<f64>,
    /// Vše, co se ukládá, drží rovnou Settings - jediný zdroj pravdy.
    set: Settings,
    vfo_input: String,
    /// Textové pole pro přímé zadání naladěné frekvence. Ladit jen kolečkem
    /// nestačí - na normál se člověk musí trefit přesně, ne po krocích.
    tuned_input: String,
    /// Střed viditelného výřezu panoramatu v Hz od VFO. Odděleně od naladění
    /// (`offset_hz`), aby se při zoomu značka pohybovala a spektrum stálo,
    /// místo aby se pořád vycentrovávalo. Neukládá se.
    view_center_hz: f64,
    /// Táhne se zrovna hrana pásma? (Jinak by tažení ladilo.)
    drag_bw: bool,
    /// Je otevřené okno správy oblíbených? Neukládá se.
    show_manage: bool,
    /// Je otevřené okno nastavení? Neukládá se.
    show_options: bool,
    /// Zvuková zařízení. Výčet je pomalý (ALSA otvírá karty), takže se
    /// dělá jen při otevření nastavení, ne každý snímek.
    devices: Option<Devices>,
    /// Nastavení, se kterým se program nastartoval - podle něj poznáme,
    /// že se zvuk nebo Si570 změnily a je potřeba restart.
    startup: StartupConfig,
    /// Po skoku za roh se má doladit na nejsilnější stanici, ale až se
    /// panorama ustálí - proto až po tomto čase.
    snap_at: Option<std::time::Instant>,
    /// Text z dekodéru. Drží se v GUI, aby přežil vypnutí dekodéru.
    console: String,
    /// Rozpis EiBi. Načítá se na pozadí, ať start nečeká na síť.
    schedule: Arc<std::sync::Mutex<ScheduleState>>,
    /// RGBA buffer vodopádu, řádek 0 = nejnovější.
    wf_pixels: Vec<u8>,
    wf_tex: Option<egui::TextureHandle>,
    last_generation: u64,
    autosave: Autosave,
    /// Běžící skener, nebo `None`. Neukládá se - po startu se nikdy neskenuje.
    scan: Option<Scan>,
    /// Kde jsme byli, když se naposled sahalo na RDS. Podle změny se pozná,
    /// že jsme přeladili na jinou stanici a přečtený text už neplatí.
    rds_khz: f64,
    /// Skutečná frekvence normálu, proti kterému se kalibruje. Neukládá se.
    cal_true_khz: f64,

}

/// Co skener projíždí.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScanKind {
    /// Oblíbené stanice i s jejich režimem a šířkou.
    Favourites,
    /// Viditelný výřez panoramatu krokem po šířce kanálu. Přelaďuje se jen
    /// offset, ne VFO - je to tedy okamžité a funguje na obou rádiích.
    Window,
}

impl ScanKind {
    fn label(&self) -> &'static str {
        match self {
            ScanKind::Favourites => "oblíbené",
            ScanKind::Window => "výřez",
        }
    }
}

/// Jak dlouho se stojí na jednom kanálu, než se jde dál. Musí to stačit na
/// ustálení AGC i šumové brány, jinak by skener přelítl i otevřenou stanici.
const SCAN_DWELL_MS: u64 = 350;
/// Jak dlouho po ztichnutí stanice čekat, než se skenování rozjede zpátky.
/// Krátká pauza v řeči nesmí skener poslat pryč.
const SCAN_RESUME_MS: u64 = 2_000;

/// Stav skeneru. Zastavuje se na signálu, který otevře šumovou bránu.
struct Scan {
    kind: ScanKind,
    /// Pozice v seznamu oblíbených, nebo pořadí kanálu ve výřezu.
    idx: usize,
    /// Kdy skočit na další kanál (když se zrovna nestojí na signálu).
    next_step: std::time::Instant,
    /// Stojíme na signálu? A od kdy je zas ticho.
    holding: bool,
    quiet_since: Option<std::time::Instant>,
}

impl App {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        tuner: mpsc::Sender<f64>,
        gain_tx: mpsc::Sender<f64>,
        s: Settings,
    ) -> Self {
        // Naladit tam, kde uživatel posledně skončil.
        let _ = tuner.send(s.vfo_khz * 1000.0);
        App {
            shared,
            tuner,
            gain_tx,
            set: s.clone(),
            vfo_input: format!("{:.1}", s.vfo_khz),
            tuned_input: format!("{:.3}", s.vfo_khz + s.offset_hz / 1000.0),
            view_center_hz: s.offset_hz,
            drag_bw: false,
            show_manage: false,
            show_options: false,
            devices: None,
            startup: StartupConfig::of(&s),
            snap_at: None,
            console: String::new(),
            schedule: spawn_schedule_load(),
            wf_pixels: vec![0; FFT_SIZE * WF_HEIGHT * 4],
            wf_tex: None,
            last_generation: 0,
            autosave: Autosave::new(s),
            scan: None,
            rds_khz: 0.0,
            cal_true_khz: 4996.0,
        }
    }

    fn bandwidth_hz(&self) -> f64 {
        self.set.bandwidth()
    }

    /// Řekne DSP vláknu, ať znovu otevře rádio s aktuálním nastavením.
    /// Tím se přepne rádio, vzorkovačka nebo zvukovka za běhu, bez restartu.
    fn request_reopen(&mut self) {
        *self.shared.status.lock().unwrap() =
            format!("přepínám na {}...", self.set.hardware.label());
        // Config nejdřív, pak vlajka - DSP vlákno bere config až po ní.
        *self.shared.reopen_config.lock().unwrap() = Some(self.set.clone());
        self.shared.reopen.store(true, Ordering::Relaxed);
        // Od teď je tohle „stav při startu rádia", takže tlačítko Použít zmizí.
        self.startup = StartupConfig::of(&self.set);
    }

    /// Dekodér, který má opravdu běžet. Se zavřenou konzolí žádný -
    /// jinak by počítal text, který stejně nikdo neuvidí.
    fn active_decoder(&self) -> decode::Decoder {
        if self.set.show_console {
            self.set.decoder
        } else {
            decode::Decoder::Off
        }
    }

    fn set_bandwidth_hz(&mut self, bw: f64) {
        let (min, max) = radio::bandwidth_range(self.set.mode);
        self.set.set_bandwidth(bw.clamp(min, max));
    }

    /// Krajní frekvence propustného pásma (absolutní offsety od středu).
    /// AM je symetrické kolem nosné, SSB leží celé na jedné straně.
    fn band_edges(&self) -> (f64, f64) {
        let bw = self.bandwidth_hz();
        match self.set.mode {
            // FM (obě šířky) je taky symetrické kolem nosné.
            dsp::Mode::Am | dsp::Mode::Cw | dsp::Mode::Wfm | dsp::Mode::Nfm => {
                (self.set.offset_hz - bw / 2.0, self.set.offset_hz + bw / 2.0)
            }
            dsp::Mode::Usb => (self.set.offset_hz, self.set.offset_hz + bw),
            dsp::Mode::Lsb => (self.set.offset_hz - bw, self.set.offset_hz),
        }
    }

    /// Které hrany jde chytit a táhnout. U SSB je vnitřní hrana zároveň
    /// nosná, takže tažení nechává jen tu vnější.
    fn draggable_edges(&self) -> Vec<f64> {
        let (lo, hi) = self.band_edges();
        match self.set.mode {
            dsp::Mode::Am | dsp::Mode::Cw | dsp::Mode::Nfm => vec![lo, hi],
            dsp::Mode::Usb => vec![hi],
            dsp::Mode::Lsb => vec![lo],
            // WFM má pevnou šířku, hrany se netáhnou.
            dsp::Mode::Wfm => vec![],
        }
    }

    fn set_vfo(&mut self, khz: f64) {
        let (lo, hi) = self.set.hardware.tuning_range_khz();
        self.set.vfo_khz = khz.clamp(lo, hi);
        self.vfo_input = format!("{:.1}", self.set.vfo_khz);
        let _ = self.tuner.send(self.set.vfo_khz * 1000.0);
    }

    /// Viditelný výřez panoramatu jako (střed v Hz od VFO, šířka v Hz).
    ///
    /// Výřez je samostatný průzor se středem `view_center_hz` - značka ladění
    /// se v něm pohybuje a spektrum stojí; posune se, až když značka dojede ke
    /// kraji (řeší [`keep_offset_visible`]). U kraje zachyceného spektra se
    /// průzor zastaví, za ním nejsou data.
    fn view(&self, span_hz: f64) -> (f64, f64) {
        view_window(self.set.zoom, self.view_center_hz, span_hz)
    }

    /// Aktuální šířka zachyceného spektra (= vzorkovačka vstupu).
    fn span_hz(&self) -> f64 {
        self.shared.sample_rate.load(Ordering::Relaxed) as f64
    }

    fn set_zoom(&mut self, z: f32) {
        self.set.zoom = z.clamp(1.0, MAX_ZOOM);
        // Přibližuje se k naladěné stanici, ne k tomu, co je zrovna uprostřed.
        //
        // Dřív se jen hlídalo, aby značka nevypadla z obrazu - jenže to ji při
        // přiblížení posadilo těsně k okraji a stanice "ujížděla pryč".
        // Přiblížení má zvětšit to, co posloucháš, tak ať se to zvětšuje kolem
        // toho. U kraje zachyceného spektra se výřez zastaví (viz `view_window`),
        // takže se značka od středu odchýlí jen tam, kde dál data nejsou.
        self.center_view_on_tuned();
    }

    /// Posune průzor tak, aby naladěná frekvence (`offset_hz`) byla vidět.
    /// Spektrum se hne jen tehdy, když by značka jinak vyjela z okraje - jinak
    /// stojí. Tím se ladění chová stejně při zoomu i bez něj.
    fn keep_offset_visible(&mut self, span_hz: f64) {
        let vis = span_hz / self.set.zoom.clamp(1.0, MAX_ZOOM) as f64;
        self.view_center_hz = pan_view_center(self.set.offset_hz, self.view_center_hz, vis, span_hz);
    }

    /// Vycentruje průzor na naladěnou stanici. Volá se po velkých skocích
    /// (skok na pásmo, doladění na nejsilnější, tlačítka VFO), kde je čekané,
    /// že se pohled přesune za stanicí.
    fn center_view_on_tuned(&mut self) {
        self.view_center_hz = self.set.offset_hz;
        self.keep_offset_visible(self.span_hz());
    }

    /// Krok jemného ladění kolečkem a šipkami. Na hrubé skoky je Shift
    /// (desetinásobek) a tlačítka VFO.
    fn tune_step_hz(&self) -> f64 {
        if self.set.mode.is_wfm() {
            // FM stanice jsou po ~100 kHz, jemnější krok kolečka nemá smysl.
            100_000.0
        } else if self.set.mode.is_ssb() || self.set.mode == dsp::Mode::Cw {
            // SSB i CW se ladí do hertzů - u CW proto, že se zanáší na zázněj
            // a při kalibraci proti normálu je stovka hertzů úplně mimo.
            10.0
        } else {
            100.0
        }
    }

    /// Kroky tlačítek VFO v kHz. Na RSP1 (VKV/UHF) jsou potřeba i velké skoky,
    /// SoftRock zůstává jemný na krátkých vlnách.
    fn vfo_steps_khz(&self) -> &'static [f64] {
        match self.set.hardware {
            source::Hardware::SoftRock => &[-10.0, -1.0, 1.0, 10.0],
            source::Hardware::Rsp1 => &[-1000.0, -100.0, -10.0, 10.0, 100.0, 1000.0],
        }
    }

    /// Doladí o `delta_hz`. Když by se stanice dostala ke kraji okna,
    /// posune se za ní VFO - jinak by ladění narazilo na neviditelnou zeď.
    fn tune_by(&mut self, delta_hz: f64, span_hz: f64) {
        let mut off = self.set.offset_hz + delta_hz;
        let limit = span_hz * 0.45;
        if off.abs() > limit {
            // Okno posuneme tak, aby stanice skončila v jeho čtvrtině,
            // a offset o stejnou hodnotu srovnáme - frekvence se nehne.
            let shift_khz = (off - off.signum() * span_hz * 0.25) / 1000.0;
            let before = self.set.vfo_khz;
            self.set_vfo(self.set.vfo_khz + shift_khz);
            off -= (self.set.vfo_khz - before) * 1000.0;
        }
        self.set.offset_hz = off;
        // Jemné ladění posune spektrum jen u kraje průzoru, jinak stojí.
        self.keep_offset_visible(span_hz);
    }

    /// Krok VFO. Okno se posune do strany, ale zůstaneme naladění na stejné
    /// stanici - jinak by každý krok naladění shodil.
    fn step_vfo(&mut self, delta_khz: f64, span_hz: f64) {
        let before = self.set.vfo_khz;
        self.set_vfo(self.set.vfo_khz + delta_khz);
        // set_vfo si krok mohl zkrátit o meze rozsahu, tak počítáme se skutečným.
        let applied = self.set.vfo_khz - before;
        self.set.offset_hz = offset_after_vfo_step(self.set.offset_hz, applied, span_hz);
        self.center_view_on_tuned();
    }

    /// Posun VFO o celou šířku okna - ukáže kus pásma, na který odsud
    /// nevidíme. Naladění tím ztrácí smysl, tak se pak doladí samo.
    fn jump_window(&mut self, span_hz: f64, dir: f64) {
        self.set_vfo(self.set.vfo_khz + dir * span_hz / 1000.0);
        self.set.offset_hz = 0.0;
        self.center_view_on_tuned();
        self.snap_at =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(SNAP_DELAY_MS));
    }

    /// Zapamatuje si aktuální místo pro pásmo, na kterém zrovna jsme.
    /// Volá se průběžně, takže tlačítko pásma pak vrátí přesně sem.
    fn remember_band(&mut self) {
        let f = self.tuned_khz();
        if let Some(s) = bandplan::at(f) {
            self.set.band_memory.insert(
                s.band.to_string(),
                settings::BandMemory {
                    freq_khz: f,
                    mode: self.set.mode,
                    bandwidth_hz: self.bandwidth_hz(),
                },
            );
        }
    }

    /// Skok na pásmo: kam jsme se na něm naposled dostali, jinak doprostřed.
    fn goto_band(&mut self, band: &bandplan::Band) {
        if let Some(m) = self.set.band_memory.get(band.name).copied() {
            self.tune_to(m.freq_khz, m.mode, m.bandwidth_hz);
            return;
        }
        // Poprvé na tomhle pásmu: doprostřed a s obvyklým režimem.
        // VKV rozhlas je širokopásmová FM (jen na RSP1), krátkovlnný rozhlas
        // AM, amatérská pásma pod 10 MHz LSB, nad ním USB.
        let fm_rozhlas = band.from_khz >= 87_000.0
            && band.to_khz <= 108_500.0
            && self.set.hardware == source::Hardware::Rsp1;
        let mode = if fm_rozhlas {
            dsp::Mode::Wfm
        } else if band.is_broadcast() {
            dsp::Mode::Am
        } else if band.from_khz >= 144_000.0 {
            // 2 m a výš je doma úzkopásmová FM (simplex, převaděče).
            dsp::Mode::Nfm
        } else if band.from_khz < 10_000.0 {
            dsp::Mode::Lsb
        } else {
            dsp::Mode::Usb
        };
        let bw = match mode {
            dsp::Mode::Cw => radio::CW_BANDWIDTH_HZ,
            dsp::Mode::Usb | dsp::Mode::Lsb => radio::SSB_BANDWIDTH_HZ,
            dsp::Mode::Am => radio::AM_BANDWIDTH_HZ,
            dsp::Mode::Nfm => radio::NFM_BANDWIDTH_HZ,
            dsp::Mode::Wfm => radio::bandwidth_range(dsp::Mode::Wfm).0,
        };
        self.tune_to(band.middle_khz(), mode, bw);
    }

    /// Naladí konkrétní frekvenci i s režimem a šířkou. VFO se posadí tak,
    /// aby stanice padla mimo mrtvou zónu kolem DC - u širokého kanálu (WFM)
    /// mnohem dál než u úzkého, jinak by DC spur padl doprostřed kanálu.
    fn tune_to(&mut self, freq_khz: f64, mode: dsp::Mode, bandwidth_hz: f64) {
        self.set.mode = mode;
        self.set_bandwidth_hz(bandwidth_hz);
        let park = park_offset(bandwidth_hz, self.span_hz());
        self.set_vfo(freq_khz - park / 1000.0);
        self.set.offset_hz = park;
        self.center_view_on_tuned();
        self.snap_at = None; // ruční volba má přednost před hledáním
    }

    /// Naladí oblíbenou stanici i s jejím režimem a šířkou pásma.
    /// VFO se posadí tak, aby stanice padla mimo mrtvou zónu kolem DC.
    fn tune_station(&mut self, st: &Station) {
        self.tune_to(st.freq_khz, st.mode, st.bandwidth_hz);
    }

    fn tuned_khz(&self) -> f64 {
        self.set.vfo_khz + self.set.offset_hz / 1000.0
    }

    fn add_current_station(&mut self) {
        let f = self.tuned_khz();
        self.set.stations.push(Station {
            name: format!("{f:.1} kHz"),
            freq_khz: f,
            mode: self.set.mode,
            bandwidth_hz: self.bandwidth_hz(),
        });
        self.show_manage = true; // rovnou ať se dá pojmenovat
    }

    /// Doladí na nejsilnější stanici v panoramatu. Když tam žádná není,
    /// nechá ladění být.
    fn snap_to_strongest(&mut self, bins: &[f32], span_hz: f64) {
        if let Some(off) = strongest_offset(bins, span_hz) {
            self.set.offset_hz = off;
            self.center_view_on_tuned();
        }
    }

    fn push_controls(&self) {
        let mut c = self.shared.controls.lock().unwrap();
        c.offset_hz = self.set.offset_hz;
        c.volume = self.set.volume;
        c.swap_iq = self.set.swap_iq;
        c.bandwidth_hz = self.bandwidth_hz();
        c.mode = self.set.mode;
        c.decoder = self.active_decoder();
        c.rtty = decode::RttyConfig {
            reverse: self.set.rtty_reverse,
            ..Default::default()
        };
        c.cw_squelch_db = self.set.cw_squelch_db;
        c.squelch_db = if self.set.squelch_on {
            Some(self.set.squelch_db)
        } else {
            None
        };
        c.agc = self.set.agc;
        c.agc_manual_db = self.set.agc_manual_db;
        c.notch_hz = if self.set.notch_on {
            Some(self.set.notch_hz)
        } else {
            None
        };
        c.stereo = self.set.stereo;
    }

    /// Kalibrace stupnice RSP1.
    ///
    /// Krystal se u každého kusu trochu liší a z něj jede vzorkovačka
    /// i směšovací oscilátor. Chyba je proto relativní - jedna hodnota v ppm
    /// srovná stupnici na krátkých vlnách i na VKV.
    ///
    /// Měřit ji nemusíš podle žádného normálu: stereo pilot VKV rozhlasu je
    /// přesně 19 kHz z normálu vysílače a měříme ho až za demodulací, takže
    /// jeho zdánlivá odchylka závisí jen na tvé vzorkovačce - tedy přímo na
    /// tom krystalu.
    fn rsp1_calibration(&mut self, ui: &mut egui::Ui) {
        let zmereno = f32::from_bits(self.shared.pilot_ppm.load(Ordering::Relaxed));
        let mut zmena = false;
        egui::Grid::new("rsp1_grid")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("odchylka krystalu [ppm]:");
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::DragValue::new(&mut self.set.rsp1_ppm)
                            .speed(0.1)
                            .range(-100.0..=100.0)
                            .fixed_decimals(2),
                    );
                    // Ladicí vlákno drží ppm z doby otevření, takže se změna
                    // projeví až po znovuotevření. Nechává se to na konec
                    // tažení, ať se rádio nerestartuje na každý pixel.
                    if r.drag_stopped() || r.lost_focus() {
                        zmena = true;
                    }
                    if ui.button("vynulovat").clicked() {
                        self.set.rsp1_ppm = 0.0;
                        zmena = true;
                    }
                });
                ui.end_row();

                ui.label("změřeno z pilotu:");
                ui.horizontal(|ui| {
                    if zmereno.is_finite() {
                        ui.label(
                            egui::RichText::new(format!("{zmereno:+.2} ppm"))
                                .color(egui::Color32::from_rgb(80, 200, 90))
                                .strong(),
                        );
                        // Měření je relativní k současnému nastavení, takže
                        // se zbytková odchylka přičítá k tomu, co už platí.
                        if ui
                            .button("použít")
                            .on_hover_text("převzít naměřenou hodnotu do kalibrace")
                            .clicked()
                        {
                            self.set.rsp1_ppm += zmereno as f64;
                            zmena = true;
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("nalaď stereo stanici na VKV").weak(),
                        );
                    }
                });
                ui.end_row();

                // Poctivá metoda: proti normálu naladěnému v CW.
                //
                // Odečet z panoramatu by nestačil - bin je při 1,344 MSps
                // široký 656 Hz, což je na 10 MHz normálu 65 ppm, tedy víc
                // než měřená chyba. Proto se měří kmitočet audio tónu, kde
                // je rozlišení o tři řády jemnější.
                ui.label("podle normálu (v CW):");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.cal_true_khz)
                            .speed(1.0)
                            .range(100.0..=2_000_000.0)
                            .suffix(" kHz"),
                    )
                    .on_hover_text("skutečná frekvence normálu, na který jsi naladěný");
                    let bezi = self.shared.cal_request.load(Ordering::Relaxed);
                    let lze = self.set.mode == dsp::Mode::Cw;
                    ui.add_enabled_ui(lze && !bezi, |ui| {
                        if ui.button("změřit").clicked() {
                            self.shared
                                .cal_tone_hz
                                .store(f32::NAN.to_bits(), Ordering::Relaxed);
                            self.shared.cal_request.store(true, Ordering::Relaxed);
                        }
                    })
                    .response
                    .on_hover_text(if lze {
                        "změří kmitočet tónu a porovná s nastaveným pitchem"
                    } else {
                        "přepni do CW - nosná normálu pak leží přesně na tónu"
                    });
                    if bezi {
                        ui.label(egui::RichText::new("měřím…").weak());
                    }
                    let ton = f32::from_bits(self.shared.cal_tone_hz.load(Ordering::Relaxed));
                    if ton.is_finite() {
                        // Kde nosná doopravdy leží: v CW dá nosná na naladěné
                        // frekvenci tón přesně CW_PITCH_HZ, takže odchylka tónu
                        // je posun nosné proti stupnici.
                        //
                        // Do výpočtu jde **skutečná** naladěná frekvence, ne ta
                        // nominální. Kdyby se předpokládalo, že jsi trefil
                        // nominál na hertz přesně, promítlo by se každé
                        // nedoladění rovnou do kalibrace - a pár desítek hertzů
                        // je na 10 MHz několik ppm.
                        let dial_hz = self.tuned_khz() * 1000.0;
                        let nominal_hz = self.cal_true_khz * 1000.0;
                        let nosna_hz = dial_hz + (ton as f64 - dsp::CW_PITCH_HZ);
                        let ppm = ppm_z_normalu(dial_hz, nominal_hz, ton as f64);
                        ui.label(
                            egui::RichText::new(format!(
                                "nosná {:.1} Hz ({:+.1} Hz od nominálu) → {ppm:+.2} ppm",
                                nosna_hz,
                                nosna_hz - nominal_hz
                            ))
                            .color(egui::Color32::from_rgb(80, 200, 90)),
                        );
                        if ui.button("použít ").clicked() {
                            self.set.rsp1_ppm += ppm;
                            self.shared
                                .cal_tone_hz
                                .store(f32::NAN.to_bits(), Ordering::Relaxed);
                            zmena = true;
                        }
                    }
                });
                ui.end_row();
            });
        ui.label(
            egui::RichText::new(
                "Podle nosné je to poctivé: nalaď normál se zaručenou přesností (DCF77 na\n\
                 77,5 kHz, RWM na 4996/9996/14996 kHz, WWV, CHU), zadej jeho frekvenci\n\
                 a dej „změřit“. Přibliž si zoom, ať je špička odečtená přesně.\n\
                 \n\
                 Pilot 19 kHz je jen rychlá orientace: norma mu povoluje ±2 Hz, což je\n\
                 ±105 ppm - horší, než bývá chyba samotného krystalu. V praxi jsou vysílače\n\
                 navázané na GPS a drží mnohem líp, ale zaručené to není. Než se na pilot\n\
                 spolehneš, změř ho na několika stanicích: když se shodnou, sedí.\n\
                 \n\
                 Jedna hodnota platí pro KV i VKV - chyba krystalu je relativní.",
            )
            .weak(),
        );
        if zmena {
            self.request_reopen();
        }
    }

    /// Stereo u VKV rozhlasu. Název stanice a stav stereo se ukazují nahoře
    /// u naladěné frekvence, RadioText dole ve stavovém řádku - sem patří
    /// jen samotný přepínač.
    fn wfm_controls(&mut self, ui: &mut egui::Ui) {
        if !self.set.mode.is_wfm() {
            return;
        }
        ui.checkbox(&mut self.set.stereo, "stereo")
            .on_hover_text("zapne stereo, když je slyšet pilot 19 kHz\nbez pilotu hraje mono tak jako tak");
    }

    /// Kde v panoramatu leží tón, který notch zabíjí - offsety od VFO v Hz.
    ///
    /// Notch pracuje na audiu, ale uživatel se dívá na spektrum. Převod závisí
    /// na režimu: u SSB je audio kmitočet přímo odstup od nosné (u LSB dolů),
    /// u CW se do toho míchá BFO a u AM dává stejný tón nosná po obou stranách,
    /// takže se značí obě.
    fn notch_rf_offsets(&self) -> Vec<f64> {
        let f = self.set.notch_hz;
        let c = self.set.offset_hz;
        match self.set.mode {
            dsp::Mode::Usb => vec![c + f],
            dsp::Mode::Lsb => vec![c - f],
            // Obálkový detektor složí obě strany na stejný tón.
            dsp::Mode::Am => vec![c - f, c + f],
            // BFO posune nosnou z DC na CW_PITCH, tak jde tón zpátky o pitch.
            dsp::Mode::Cw => vec![c + f - dsp::CW_PITCH_HZ],
            // U FM je vztah audia a spektra nelineární, značka by lhala.
            dsp::Mode::Nfm | dsp::Mode::Wfm => vec![],
        }
    }

    /// Je na naladěném kanálu signál, který otevře šumovou bránu?
    ///
    /// Skener se rozhoduje podle stejné veličiny jako brána v DSP - úrovně
    /// před AGC proti prahu v dBFS. Díky tomu zastaví přesně tam, kde se
    /// ozve zvuk, a ne tam, kde jen povyskočí spektrum.
    fn signal_open(&self) -> bool {
        self.set.squelch_on && self.shared.level_dbfs() > self.set.squelch_db
    }

    /// Kolik kanálů má výřez při skenování krokem po šířce pásma.
    fn scan_window_steps(&self, span_hz: f64) -> usize {
        let (_, view_w) = self.view(span_hz);
        let step = self.bandwidth_hz().max(100.0);
        ((view_w / step).floor() as usize).max(1)
    }

    /// Přeladí na kanál s daným pořadím podle druhu skenování.
    fn scan_goto(&mut self, idx: usize, span_hz: f64) {
        match self.scan.as_ref().map(|s| s.kind) {
            Some(ScanKind::Favourites) => {
                if let Some(st) = self.set.stations.get(idx).cloned() {
                    self.tune_station(&st);
                }
            }
            Some(ScanKind::Window) => {
                let (view_c, view_w) = self.view(span_hz);
                let step = self.bandwidth_hz().max(100.0);
                let n = self.scan_window_steps(span_hz);
                // Kanály se kladou od levého kraje výřezu doprava, doprostřed
                // každého kroku - ať naladěná značka nesedí na hraně.
                let off = view_c - view_w / 2.0 + step * (idx as f64 + 0.5);
                if n > 0 {
                    self.set.offset_hz = off;
                }
            }
            None => {}
        }
    }

    /// Posune skener o krok dál a ohlídá, aby se zastavil na signálu.
    /// Volá se každý snímek, dokud skener běží.
    fn scan_tick(&mut self, span_hz: f64) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        // Bez zapnuté brány není podle čeho zastavovat - skener by jen
        // bezcílně přelaďoval, tak radši skončí.
        if !self.set.squelch_on {
            self.scan = None;
            return;
        }
        let now = std::time::Instant::now();
        let kind = scan.kind;
        let idx = scan.idx;
        let holding = scan.holding;
        let next_step = scan.next_step;
        let quiet_since = scan.quiet_since;
        let otevreno = self.signal_open();

        if holding {
            // Stojíme na stanici. Rozjedeme se, až bude chvíli ticho.
            if otevreno {
                if let Some(s) = self.scan.as_mut() {
                    s.quiet_since = None;
                }
                return;
            }
            let od = match quiet_since {
                Some(t) => t,
                None => {
                    if let Some(s) = self.scan.as_mut() {
                        s.quiet_since = Some(now);
                    }
                    return;
                }
            };
            if now.duration_since(od) < std::time::Duration::from_millis(SCAN_RESUME_MS) {
                return;
            }
            if let Some(s) = self.scan.as_mut() {
                s.holding = false;
                s.quiet_since = None;
                s.next_step = now;
            }
            return;
        }

        // Přelaďujeme. Když se na kanálu ozve signál, zastavíme.
        if otevreno {
            if let Some(s) = self.scan.as_mut() {
                s.holding = true;
                s.quiet_since = None;
            }
            return;
        }
        if now < next_step {
            return;
        }
        let pocet = match kind {
            ScanKind::Favourites => self.set.stations.len(),
            ScanKind::Window => self.scan_window_steps(span_hz),
        };
        if pocet == 0 {
            self.scan = None;
            return;
        }
        let dalsi = (idx + 1) % pocet;
        self.scan_goto(dalsi, span_hz);
        if let Some(s) = self.scan.as_mut() {
            s.idx = dalsi;
            s.next_step = now + std::time::Duration::from_millis(SCAN_DWELL_MS);
        }
    }

    /// Ovládání skeneru.
    fn scan_controls(&mut self, ui: &mut egui::Ui, span_hz: f64) {
        let bezi = self.scan.is_some();
        // Skener pozná stanici jedině podle brány - bez ní nemá kde zastavit.
        let lze = self.set.squelch_on;
        ui.add_enabled_ui(lze || bezi, |ui| {
            if bezi {
                let drzi = self.scan.as_ref().is_some_and(|s| s.holding);
                if ui
                    .button("⏹ stop")
                    .on_hover_text("ukončit skenování")
                    .clicked()
                {
                    self.scan = None;
                }
                ui.label(
                    egui::RichText::new(if drzi { "● stojí na signálu" } else { "⟳ hledám…" })
                        .color(if drzi {
                            egui::Color32::from_rgb(80, 200, 90)
                        } else {
                            egui::Color32::from_rgb(220, 200, 60)
                        }),
                );
            } else {
                ui.label("skenovat:");
                for kind in [ScanKind::Favourites, ScanKind::Window] {
                    // Skenovat prázdný seznam oblíbených nemá co dělat.
                    let ok = kind != ScanKind::Favourites || !self.set.stations.is_empty();
                    let resp = ui
                        .add_enabled_ui(ok, |ui| ui.button(kind.label()))
                        .inner;
                    if resp.clicked() {
                        self.scan = Some(Scan {
                            kind,
                            idx: 0,
                            next_step: std::time::Instant::now(),
                            holding: false,
                            quiet_since: None,
                        });
                        self.scan_goto(0, span_hz);
                    }
                    if !ok {
                        resp.on_hover_text("nejdřív si přidej nějakou oblíbenou stanici");
                    }
                }
            }
        })
        .response
        .on_hover_text(if lze || bezi {
            "projíždí kanály a zastaví, když signál otevře šumovou bránu\n\
             po ztichnutí stanice se za pár vteřin rozjede dál"
        } else {
            "skenování potřebuje zapnutou šumovou bránu - podle ní pozná stanici"
        });
    }

    /// Nahrávání demodulovaného zvuku do WAV.
    fn record_controls(&mut self, ui: &mut egui::Ui) {
        let bezi = self.shared.recording.load(Ordering::Relaxed);
        let popis = if bezi { "⏹ zastavit" } else { "⏺ nahrávat" };
        if ui
            .button(popis)
            .on_hover_text(if bezi {
                "dopsat hlavičku a soubor zavřít"
            } else {
                "uložit zvuk do WAV (48 kHz, 16 bit mono)\n\
                 nahrává se před hlasitostí, takže knoflík nahrávku neovlivní"
            })
            .clicked()
        {
            if bezi {
                self.shared.recording.store(false, Ordering::Relaxed);
            } else {
                // Jméno se skládá až tady, ať nese čas a frekvenci ze chvíle,
                // kdy jsi zmáčkl nahrávat.
                let path = record::default_dir()
                    .join(record::file_name(self.tuned_khz(), self.set.mode.label()));
                *self.shared.record_path.lock().unwrap() = path.to_string_lossy().to_string();
                self.shared.recording.store(true, Ordering::Relaxed);
            }
        }
        if bezi {
            let s = self.shared.record_secs();
            ui.label(
                egui::RichText::new(format!("● {:.0}:{:02.0}", s / 60.0, s % 60.0))
                    .color(egui::Color32::from_rgb(230, 90, 60))
                    .strong(),
            );
        }
        // Poslední hlášení (uloženo / chyba) - ať je vidět, kam to spadlo.
        let stav = self.shared.record_status.lock().unwrap().clone();
        if !stav.is_empty() {
            ui.label(egui::RichText::new(stav).weak())
                .on_hover_text(record::default_dir().to_string_lossy().to_string());
        }
    }

    /// Ruční zádrž na heterodynní pískot.
    fn notch_controls(&mut self, ui: &mut egui::Ui) {
        // U FM nemá zádrž na audiu smysl - heterodyn se u frekvenční
        // modulace neprojevuje jako stálý tón.
        let ma_notch = !matches!(self.set.mode, dsp::Mode::Nfm | dsp::Mode::Wfm);
        ui.add_enabled_ui(ma_notch, |ui| {
            ui.checkbox(&mut self.set.notch_on, "notch");
            ui.add_enabled_ui(self.set.notch_on, |ui| {
                ui.add(
                    egui::Slider::new(
                        &mut self.set.notch_hz,
                        dsp::NOTCH_MIN_HZ..=dsp::NOTCH_MAX_HZ,
                    )
                    .fixed_decimals(0)
                    .suffix(" Hz"),
                )
                .on_hover_text("kmitočet zádrže - najeď na pískot, až zmizí");
            });
        })
        .response
        .on_hover_text(if ma_notch {
            "úzká zádrž na heterodynní pískot od sousední nosné\n\
             běží před AGC, takže po odstranění tónu zisk nezůstane stažený"
        } else {
            "u FM nemá zádrž na audiu smysl"
        });
    }

    /// Konzole s textem z dekodéru.
    fn console_panel(&mut self, ui: &mut egui::Ui) {
        // Vyzvedneme, co dekodér mezitím přečetl.
        if let Ok(mut d) = self.shared.decoded.lock() {
            if !d.is_empty() {
                self.console.push_str(&d);
                d.clear();
            }
        }
        // Historii držíme na uzdě.
        if self.console.len() > 16_384 {
            let cut = self.console.len() - 8_192;
            // Řez musí padnout na hranici znaku, jinak by to panikařilo.
            let cut = (cut..self.console.len())
                .find(|&i| self.console.is_char_boundary(i))
                .unwrap_or(self.console.len());
            self.console = self.console[cut..].to_string();
        }

        egui::Panel::bottom("konzole")
            .resizable(true)
            .default_size(160.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("dekodér:");
                    for d in [decode::Decoder::Off, decode::Decoder::Rtty, decode::Decoder::Cw] {
                        ui.selectable_value(&mut self.set.decoder, d, d.label());
                    }
                    if self.set.decoder == decode::Decoder::Rtty {
                        ui.separator();
                        ui.checkbox(&mut self.set.rtty_reverse, "reverse")
                            .on_hover_text("prohodí mark a space - v éteru se běžně vyskytuje obojí");
                        ui.label(
                            egui::RichText::new("45,45 Bd · 170 Hz shift").weak(),
                        );
                    }
                    if self.set.decoder == decode::Decoder::Cw {
                        ui.separator();
                        ui.add(
                            egui::Slider::new(&mut self.set.cw_squelch_db, 3.0..=30.0)
                                .text("práh dekodéru [dB nad šumem]")
                                .fixed_decimals(0),
                        )
                        .on_hover_text(
                            "o kolik musí signál vyčnívat nad šum, aby se dekódoval\n\
                             níž = citlivější, ale víc nesmyslů ze šumu\n\
                             (netýká se šumové brány zvuku - ta má práh v dBFS u S-metru)",
                        );
                        let wpm = self.shared.cw_wpm();
                        ui.label(egui::RichText::new(format!("~{wpm:.0} WPM")).weak())
                            .on_hover_text("odhadnuté tempo, dekodér si ho odvozuje sám");
                    }
                    ui.separator();
                    if ui.button("smazat").clicked() {
                        self.console.clear();
                    }
                    if ui.button("zavřít").clicked() {
                        self.set.show_console = false;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let text = if self.console.is_empty() {
                            match self.set.decoder {
                                decode::Decoder::Off => "Dekodér je vypnutý.".to_string(),
                                decode::Decoder::Rtty => {
                                    "Nalaď tak, aby značka ladění byla mezi oběma tóny RTTY.\n                                     Když text vypadá jako nesmysl, zkus reverse."
                                        .to_string()
                                }
                                decode::Decoder::Cw => {
                                    "Nalaď na tón CW a přiškrť šířku pásma.".to_string()
                                }
                            }
                        } else {
                            self.console.clone()
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text).monospace(),
                            )
                            .wrap(),
                        );
                    });
            });
    }

    /// Řada tlačítek pásem. Barva odpovídá bandplanu, takže rozhlasová
    /// pásma jsou na první pohled poznat.
    fn band_buttons(&mut self, ui: &mut egui::Ui, tuned_khz: f64) {
        let bands = bandplan::bands();
        let here = bandplan::at(tuned_khz).map(|s| s.band);
        let mut go: Option<bandplan::Band> = None;

        let (tune_lo, tune_hi) = self.set.hardware.tuning_range_khz();
        ui.horizontal_wrapped(|ui| {
            ui.label("pásma:");
            for b in &bands {
                // Pásmo mimo dosah rádia zašedni - klik by stejně jen skočil
                // na kraj rozsahu (třeba FM na krátkovlnném SoftRocku).
                let reachable = b.middle_khz() >= tune_lo && b.middle_khz() <= tune_hi;
                let active = here == Some(b.name);
                let known = self.set.band_memory.contains_key(b.name);
                let (r, g, bl) = if b.is_broadcast() {
                    bandplan::Usage::Broadcast.color()
                } else {
                    bandplan::Usage::Phone.color()
                };
                let mut text = egui::RichText::new(b.name)
                    .color(egui::Color32::from_rgb(r, g, bl));
                if active {
                    text = text.strong();
                }
                let tip = if !reachable {
                    format!("{} je mimo dosah rádia {}", b.name, self.set.hardware.label())
                } else if known {
                    format!("zpět tam, kde jsi na {} naposled byl", b.name)
                } else {
                    format!("{} - doprostřed ({:.0} kHz)", b.name, b.middle_khz())
                };
                let resp = ui
                    .add_enabled_ui(reachable, |ui| ui.selectable_label(active, text))
                    .inner
                    .on_hover_text(tip);
                if resp.clicked() {
                    go = Some(*b);
                }
            }
        });

        if let Some(b) = go {
            // Než odskočíme, uložíme si, kde jsme na stávajícím pásmu byli.
            self.remember_band();
            self.goto_band(&b);
        }
    }

    /// Co by podle rozpisu mělo být slyšet na naladěné frekvenci.
    ///
    /// Jen pro AM - rozpis je rozhlasový, u SSB ani CW nedává smysl.
    fn schedule_section(&mut self, ui: &mut egui::Ui) {
        if self.set.mode != dsp::Mode::Am {
            return;
        }
        ui.label(egui::RichText::new("Podle rozpisu").strong());

        let tuned = self.tuned_khz();
        let state = self.schedule.lock().unwrap();
        match &*state {
            ScheduleState::Loading => {
                ui.label(egui::RichText::new("načítám rozpis...").weak());
            }
            ScheduleState::Failed(e) => {
                ui.label(egui::RichText::new("rozpis se nenačetl").weak())
                    .on_hover_text(e.clone());
            }
            ScheduleState::Ready(sch) => {
                let found = sch.lookup(tuned, SCHEDULE_TOLERANCE_KHZ);
                if found.is_empty() {
                    ui.label(
                        egui::RichText::new("na téhle frekvenci teď nic neplánují")
                            .weak(),
                    );
                } else {
                    // Zkratky rozepisujeme - "B" nebo "CLA" nikomu nic neřekne.
                    let explain = |code: &str, full: Option<&str>| -> String {
                        match full {
                            Some(f) if !code.is_empty() => format!("{f} ({code})"),
                            _ if code.is_empty() => "?".to_string(),
                            _ => code.to_string(),
                        }
                    };
                    for e in found.iter().take(6) {
                        let mut tip = format!(
                            "{:.0} kHz\n{:04}-{:04} UTC\nzemě: {}\njazyk: {}\ncíl: {}",
                            e.freq_khz,
                            e.start,
                            e.end,
                            explain(&e.country, sch.codes.country(&e.country)),
                            explain(&e.language, sch.codes.language(&e.language)),
                            explain(&e.target, sch.codes.target(&e.target)),
                        );
                        // Relay je pro identifikaci klíčový: odjinud se
                        // signál šíří úplně jinak.
                        if let Some(host) = e.relay_country() {
                            tip.push_str(&format!(
                                "\nvysíláno přes: {}",
                                explain(host, sch.codes.country(host))
                            ));
                        }
                        // Země a jazyk rovnou v seznamu: podle jazyka se
                        // nejsnáz pozná, kterou z kandidátek zrovna slyšíš.
                        let mut podtitul = Vec::new();
                        if !e.country.is_empty() {
                            podtitul
                                .push(sch.codes.country(&e.country).unwrap_or(&e.country).to_string());
                        }
                        if !e.language.is_empty() {
                            podtitul.push(
                                sch.codes
                                    .language_short(&e.language)
                                    .unwrap_or(&e.language)
                                    .to_string(),
                            );
                        }
                        if let Some(host) = e.relay_country() {
                            podtitul.push(format!(
                                "přes {}",
                                sch.codes.country(host).unwrap_or(host)
                            ));
                        }
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(&e.station);
                            if !podtitul.is_empty() {
                                // Záměrně bez .weak() - to je jen běžná barva
                                // vynásobená alfou a na malém písmu se ztrácí.
                                // Země a jazyk jsou přitom to hlavní, proč tu
                                // ta sekce je. Hierarchii drží velikost.
                                ui.label(egui::RichText::new(podtitul.join(" · ")).size(11.0));
                            }
                        })
                        .response
                        .on_hover_text(tip);
                        ui.add_space(3.0);
                    }
                    if found.len() > 6 {
                        ui.label(
                            egui::RichText::new(format!("...a dalších {}", found.len() - 6))
                                .weak(),
                        );
                    }
                }
                ui.label(
                    egui::RichText::new(format!("EiBi {}", sch.season))
                        .weak()
                        .size(9.0),
                )
                .on_hover_text("data z eibispace.de, čas v UTC");
            }
        }
    }

    /// S-metr: úroveň naladěného signálu před AGC.
    ///
    /// Ukazuje dBFS, ne S-jednotky - přijímač nemá absolutní kalibraci,
    /// takže by S-čísla byla vymyšlená.
    fn s_meter(&self, ui: &mut egui::Ui) {
        const LO: f32 = -100.0;
        const HI: f32 = -10.0;
        let db = self.shared.level_dbfs().clamp(LO, HI);
        let t = (db - LO) / (HI - LO);

        let (resp, painter) =
            ui.allocate_painter(egui::vec2(90.0, 14.0), egui::Sense::hover());
        let r = resp.rect;
        painter.rect_filled(r, 2.0, egui::Color32::from_gray(30));
        let filled = egui::Rect::from_min_size(r.min, egui::vec2(r.width() * t, r.height()));
        // Zelená -> žlutá -> červená podle síly.
        let col = if t < 0.6 {
            egui::Color32::from_rgb(80, 200, 90)
        } else if t < 0.85 {
            egui::Color32::from_rgb(220, 200, 60)
        } else {
            egui::Color32::from_rgb(230, 90, 60)
        };
        painter.rect_filled(filled, 2.0, col);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            format!("{db:.0} dBFS"),
            egui::FontId::proportional(10.0),
            egui::Color32::WHITE,
        );
        resp.on_hover_text("úroveň naladěného signálu před AGC");
    }

    /// Volba rychlosti AGC. Rychlá se hodí na CW, pomalá na SSB a AM;
    /// vypnutá dá ruční zisk pro případy, kdy AGC jen vytahuje šum v mezerách.
    fn agc_controls(&mut self, ui: &mut egui::Ui) {
        // WFM nemá v řetězci AGC (obálka je konstantní), tak ať to neslibuje
        // něco, co nic nedělá.
        let ma_agc = !self.set.mode.is_wfm();
        ui.add_enabled_ui(ma_agc, |ui| {
            ui.label("AGC:");
            for m in dsp::AgcMode::ALL {
                ui.selectable_value(&mut self.set.agc, m, m.label());
            }
            if self.set.agc == dsp::AgcMode::Off {
                ui.add(
                    egui::Slider::new(
                        &mut self.set.agc_manual_db,
                        radio::AGC_MANUAL_MIN_DB..=radio::AGC_MANUAL_MAX_DB,
                    )
                    .fixed_decimals(0)
                    .suffix(" dB"),
                )
                .on_hover_text("ruční zisk - AGC je vypnutá");
            }
        })
        .response
        .on_hover_text(if ma_agc {
            "jak rychle AGC pouští zisk zpátky nahoru\n\
             rychlá = CW, pomalá = SSB a AM (nepumpuje šum mezi slovy)"
        } else {
            "WFM nemá AGC - u FM nese hlasitost zdvih, ne síla signálu"
        });
    }

    /// Levý panel s oblíbenými stanicemi - jedno kliknutí = naladěno.
    fn favourites_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("oblibene")
            .resizable(true)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.heading("Oblíbené");
                    if ui
                        .button("⚙")
                        .on_hover_text("spravovat oblíbené")
                        .clicked()
                    {
                        self.show_manage = !self.show_manage;
                    }
                });
                ui.separator();

                if self.set.stations.is_empty() {
                    ui.label(
                        egui::RichText::new("Zatím nic.\nNalaď stanici a dej „+ přidat\u{00A0}aktuální“.")
                            .weak(),
                    );
                }

                // Klonujeme, ať jde uvnitř smyčky sáhnout na &mut self.
                let stations = self.set.stations.clone();
                let tuned = self.tuned_khz();
                let mut pick: Option<Station> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for st in &stations {
                            let active =
                                (tuned - st.freq_khz).abs() < 0.05 && self.set.mode == st.mode;
                            let text = format!(
                                "{}\n{:.1} kHz · {}",
                                st.name,
                                st.freq_khz,
                                st.mode.label()
                            );
                            if ui.selectable_label(active, text).clicked() {
                                pick = Some(st.clone());
                            }
                        }
                    });
                if let Some(st) = pick {
                    self.tune_station(&st);
                }

                ui.separator();
                self.schedule_section(ui);

                ui.separator();
                if ui
                    .button("+ přidat aktuální")
                    .on_hover_text("uloží aktuální frekvenci, režim i šířku pásma")
                    .clicked()
                {
                    self.add_current_station();
                }
                ui.add_space(4.0);
            });
    }

    /// Rozbalovací seznam zvukových zařízení. Necháváme i ruční zápis -
    /// výčet nemusí trefit všechno, co ALSA umí otevřít.
    fn device_picker(
        ui: &mut egui::Ui,
        id: &str,
        current: &mut String,
        list: &[audio::DeviceInfo],
    ) {
        let shown = list
            .iter()
            .find(|d| &d.id == current)
            .map_or_else(|| current.clone(), |d| d.label.clone());
        egui::ComboBox::from_id_salt(id)
            .selected_text(shown)
            .width(320.0)
            .show_ui(ui, |ui| {
                for d in list {
                    ui.selectable_value(current, d.id.clone(), &d.label);
                }
            });
        ui.add(egui::TextEdit::singleline(current).desired_width(180.0))
            .on_hover_text("název zařízení lze zapsat i ručně");
    }

    /// Okno nastavení: zvuk a SoftRock. Všechno tady se čte při startu vláken,
    /// takže se změny projeví až po restartu - a okno to říká nahlas.
    fn options_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_options;
        // Nastaví se uvnitř okna, provede až po jeho zavření - jinak by kolidoval
        // půjčený `self` (request_reopen chce celé &mut self).
        let mut reopen = false;
        egui::Window::new("Nastavení")
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.heading("Rádio");
                ui.label(
                    egui::RichText::new(format!(
                        "aktuálně: {} — rádio, zisk i vzorkovačka se ovládají v liště nahoře",
                        self.set.hardware.label()
                    ))
                    .weak(),
                );

                ui.add_space(8.0);
                ui.separator();
                let devices = self
                    .devices
                    .get_or_insert_with(Devices::enumerate);

                let softrock = self.set.hardware == source::Hardware::SoftRock;

                ui.heading("Zvuk");
                ui.label(
                    egui::RichText::new(format!(
                        "zvuková vrstva: {}",
                        audio::backend_name()
                    ))
                    .weak(),
                );
                ui.add_space(4.0);

                egui::Grid::new("zvuk_grid")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        // Vstup a hloubka jsou jen o zvukovce se SoftRockem;
                        // RSP1 si vzorky nese sám po USB.
                        ui.add_enabled(softrock, egui::Label::new("vstup (I/Q):"));
                        ui.add_enabled_ui(softrock, |ui| {
                            ui.horizontal(|ui| {
                                Self::device_picker(
                                    ui,
                                    "vstup",
                                    &mut self.set.capture_device,
                                    &devices.capture,
                                );
                            });
                        });
                        ui.end_row();

                        ui.label("výstup:");
                        ui.horizontal(|ui| {
                            Self::device_picker(
                                ui,
                                "vystup",
                                &mut self.set.playback_device,
                                &devices.playback,
                            );
                            ui.label(
                                egui::RichText::new("(až po restartu)").weak().small(),
                            )
                            .on_hover_text(
                                "výstupní zařízení se na rozdíl od zbytku mění jen restartem",
                            );
                        });
                        ui.end_row();

                        ui.add_enabled(softrock, egui::Label::new("hloubka:"));
                        ui.add_enabled_ui(softrock, |ui| {
                            ui.horizontal(|ui| {
                                for d in audio::Depth::ALL {
                                    ui.selectable_value(&mut self.set.depth, d, d.label());
                                }
                                ui.label(
                                    egui::RichText::new(format!("→ {}", self.set.depth.hint()))
                                        .weak(),
                                );
                            });
                        });
                        ui.end_row();
                    });
                if softrock {
                    ui.label(
                        egui::RichText::new(
                            "24 bit umí jen ALSA na Linuxu. Přes WASAPI a CoreAudio \
                             o formátu rozhoduje zvukový server, tam automatika cílí na 16 bit.",
                        )
                        .weak()
                        .small(),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(
                            "RSP1 jede na 1 344 kHz (= 48 kHz × 28) a vzorky si nese po USB, \
                             takže vstupní zvukovka ani hloubka se ho netýkají.",
                        )
                        .weak()
                        .small(),
                    );
                }

                if ui.button("↻ znovu prohledat zařízení").clicked() {
                    self.devices = None;
                }

                if self.set.hardware == source::Hardware::Rsp1 {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.heading("SDRplay RSP1");
                    self.rsp1_calibration(ui);
                }

                if self.set.hardware.uses_si570() {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.heading("SoftRock");
                    egui::Grid::new("sr_grid")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("krystal Si570 [Hz]:");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut self.set.si570_xtal_hz)
                                        .speed(1.0)
                                        .range(100_000_000.0..=130_000_000.0),
                                );
                                if ui.button("výchozí").clicked() {
                                    self.set.si570_xtal_hz = settings::SI570_XTAL_HZ;
                                }
                            });
                            ui.end_row();

                            ui.label("adresa I2C:");
                            ui.horizontal(|ui| {
                                let mut addr = self.set.si570_i2c_addr as u32;
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut addr)
                                            .speed(1.0)
                                            .range(0..=127)
                                            .hexadecimal(2, false, true),
                                    )
                                    .changed()
                                {
                                    self.set.si570_i2c_addr = addr as u16;
                                }
                                ui.label(
                                    egui::RichText::new("obvykle 0x55, u některých kusů 0x50")
                                        .weak()
                                        .small(),
                                );
                            });
                            ui.end_row();
                        });
                    ui.label(
                        egui::RichText::new(
                            "Krystal je kalibrace kus od kusu - špatná hodnota posune \
                             celou stupnici.",
                        )
                        .weak()
                        .small(),
                    );
                }

                ui.add_space(8.0);
                ui.separator();
                // Zvukovka, hloubka a Si570 se čtou při otevření rádia. Přepnutí
                // rádia a vzorkovačky se aplikuje hned samo; tyhle až na tlačítko,
                // ať se rádio nezavírá při každém ťuknutí do políčka.
                if StartupConfig::of(&self.set) != self.startup {
                    if ui
                        .button("↻ Použít změny")
                        .on_hover_text("znovu otevře rádio se změněnou zvukovkou / hloubkou / Si570")
                        .clicked()
                    {
                        reopen = true;
                    }
                }
                if let Some(p) = settings::config_path() {
                    ui.label(
                        egui::RichText::new(format!("config: {}", p.display()))
                            .weak()
                            .small(),
                    );
                }
            });
        self.show_options = open;
        if reopen {
            self.request_reopen();
        }
    }

    /// Okno pro správu oblíbených - přejmenování, úpravy, pořadí, mazání.
    fn manage_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_manage;
        egui::Window::new("Správa oblíbených stanic")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                if self.set.stations.is_empty() {
                    ui.label("Seznam je prázdný.");
                    return;
                }
                let mut remove: Option<usize> = None;
                let mut swap: Option<(usize, usize)> = None;
                let count = self.set.stations.len();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("stanice_grid")
                        .num_columns(6)
                        .striped(true)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("Název");
                            ui.label("kHz");
                            ui.label("Režim");
                            ui.label("Šířka [kHz]");
                            ui.label("Pořadí");
                            ui.label("");
                            ui.end_row();

                            for (i, st) in self.set.stations.iter_mut().enumerate() {
                                ui.add(
                                    egui::TextEdit::singleline(&mut st.name)
                                        .desired_width(140.0),
                                );
                                ui.add(
                                    egui::DragValue::new(&mut st.freq_khz)
                                        .speed(1.0)
                                        // Až do horní meze RSP1, ať jde uložit
                                        // i FM nebo jiná VKV/UHF stanice.
                                        .range(100.0..=2_000_000.0)
                                        .fixed_decimals(1),
                                );
                                egui::ComboBox::from_id_salt(("rezim", i))
                                    .selected_text(st.mode.label())
                                    .width(60.0)
                                    .show_ui(ui, |ui| {
                                        for m in [
                                            dsp::Mode::Am,
                                            dsp::Mode::Usb,
                                            dsp::Mode::Lsb,
                                            dsp::Mode::Cw,
                                            dsp::Mode::Nfm,
                                            dsp::Mode::Wfm,
                                        ] {
                                            ui.selectable_value(&mut st.mode, m, m.label());
                                        }
                                    });
                                let (bw_min, bw_max) = radio::bandwidth_range(st.mode);
                                let mut bw = st.bandwidth_hz / 1000.0;
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut bw)
                                            .speed(0.1)
                                            .range(bw_min / 1000.0..=bw_max / 1000.0)
                                            .fixed_decimals(1),
                                    )
                                    .changed()
                                {
                                    st.bandwidth_hz = bw * 1000.0;
                                }
                                ui.horizontal(|ui| {
                                    if ui.add_enabled(i > 0, egui::Button::new("↑")).clicked() {
                                        swap = Some((i - 1, i));
                                    }
                                    if ui
                                        .add_enabled(i + 1 < count, egui::Button::new("↓"))
                                        .clicked()
                                    {
                                        swap = Some((i, i + 1));
                                    }
                                });
                                if ui.button("🗑").on_hover_text("smazat").clicked() {
                                    remove = Some(i);
                                }
                                ui.end_row();
                            }
                        });
                });

                if let Some((a, b)) = swap {
                    self.set.stations.swap(a, b);
                }
                if let Some(i) = remove {
                    self.set.stations.remove(i);
                }
            });
        self.show_manage = open;
    }

    /// Ladění kliknutím a tažení hran propustného pásma. Používají to
    /// panorama i vodopád, ať se obě plochy chovají stejně.
    fn tune_interaction(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        rect: egui::Rect,
        span_hz: f64,
    ) {
        /// Jak blízko k hraně se musí trefit, aby se táhla.
        const GRAB_PX: f32 = 6.0;

        let (view_c, view_w) = self.view(span_hz);
        let hz_of_x = |x: f32| view_c + ((x - rect.center().x) / rect.width()) as f64 * view_w;
        let x_of_hz = |hz: f64| rect.center().x + ((hz - view_c) / view_w) as f32 * rect.width();

        // Kolečko ladí, s Ctrl přibližuje, se Shiftem ladí po desetinásobcích.
        //
        // Počítáme diskrétní cvaknutí z událostí, ne smooth_scroll_delta -
        // ta je vyhlazená a doznívá přes několik snímků, takže by jedno
        // cvaknutí naladilo o několik kroků najednou.
        if resp.hovered() {
            let (notches, shift, ctrl) = ui.input(|i| {
                let n: i32 = i
                    .events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::MouseWheel { delta, .. } if delta.y > 0.0 => Some(1),
                        egui::Event::MouseWheel { delta, .. } if delta.y < 0.0 => Some(-1),
                        _ => None,
                    })
                    .sum();
                (n, i.modifiers.shift, i.modifiers.ctrl)
            });
            if notches != 0 {
                if ctrl {
                    self.set_zoom(self.set.zoom * 1.25f32.powi(notches));
                } else {
                    let mult = if shift { 10.0 } else { 1.0 };
                    self.tune_by(notches as f64 * self.tune_step_hz() * mult, span_hz);
                }
            }
        }

        let edges = self.draggable_edges();
        let near_edge =
            |x: f32| edges.iter().any(|&e| (x - x_of_hz(e)).abs() <= GRAB_PX);

        if let Some(p) = resp.hover_pos() {
            if near_edge(p.x) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }

        if resp.drag_started() {
            self.drag_bw = resp.interact_pointer_pos().is_some_and(|p| near_edge(p.x));
        }
        if resp.drag_stopped() {
            self.drag_bw = false;
        }

        if let Some(p) = resp.interact_pointer_pos() {
            if resp.dragged() || resp.clicked() {
                if self.drag_bw {
                    let d = hz_of_x(p.x) - self.set.offset_hz;
                    // U AM řídí obě hrany totéž (pásmo je symetrické),
                    // u SSB je šířka rovnou vzdálenost hrany od nosné.
                    let bw = match self.set.mode {
                        dsp::Mode::Am | dsp::Mode::Cw | dsp::Mode::Nfm => d.abs() * 2.0,
                        dsp::Mode::Usb => d,
                        dsp::Mode::Lsb => -d,
                        // WFM hrany nejsou tažné (draggable_edges je prázdné),
                        // sem se nedostane; šířku nech být.
                        dsp::Mode::Wfm => self.bandwidth_hz(),
                    };
                    self.set_bandwidth_hz(bw);
                } else {
                    self.set.offset_hz = hz_of_x(p.x).round();
                }
            }
        }
    }

    /// Přidá nový řádek do vodopádu (posun dolů, nový nahoru).
    fn push_waterfall_row(&mut self, bins: &[f32]) {
        let row_bytes = FFT_SIZE * 4;
        self.wf_pixels
            .copy_within(0..(WF_HEIGHT - 1) * row_bytes, row_bytes);
        for (i, &db) in bins.iter().enumerate() {
            let t = ((db - self.set.db_min) / (self.set.db_max - self.set.db_min)).clamp(0.0, 1.0);
            let [r, g, b] = colormap(t);
            let p = i * 4;
            self.wf_pixels[p] = r;
            self.wf_pixels[p + 1] = g;
            self.wf_pixels[p + 2] = b;
            self.wf_pixels[p + 3] = 255;
        }
    }
}

/// Odchylka krystalu v ppm z měření nosné normálu.
///
/// `dial_hz` je frekvence, na které přijímač **stojí** (podle své stupnice),
/// `nominal_hz` skutečná frekvence normálu a `ton_hz` změřený kmitočet tónu
/// v CW. Nosná naladěná přesně dá tón `CW_PITCH_HZ`, takže odchylka tónu
/// říká, kde nosná proti stupnici leží.
///
/// Klíčové je, že se počítá proti skutečnému naladění, ne proti nominálu -
/// jinak by se každé nedoladění připočetlo k chybě krystalu. Pár desítek
/// hertzů vedle je na 10 MHz několik ppm, tedy víc než měřená veličina.
fn ppm_z_normalu(dial_hz: f64, nominal_hz: f64, ton_hz: f64) -> f64 {
    let nosna_hz = dial_hz + (ton_hz - dsp::CW_PITCH_HZ);
    (nominal_hz - nosna_hz) / dial_hz * 1e6
}

/// Ořízne text na danou délku a přidá výpustku. Řez padne na hranici znaku,
/// jinak by to na diakritice panikařilo.
fn zkrat(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let konec: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", konec.trim_end())
}

/// Úroveň v dB, nad kterou signál otevře práh CW dekodéru - k zakreslení do
/// spektra. Netýká se šumové brány zvuku ([`dsp::Squelch`]), ta má práh rovnou
/// v dBFS a kreslí se bez přepočtu.
///
/// Není to prosté „šum + práh". Dekodér počítá odstup v šířce svého
/// kanálového filtru, kdežto spektrum ukazuje úroveň na jeden bin FFT.
/// Šumu je v širším filtru víc, takže je potřeba přepočet `10*log10(bw/bin)`;
/// bez něj by čára ležela u 500Hz filtru asi o 10 dB níž, než odpovídá
/// skutečnosti, a slibovala by dekódování signálů, které squelch neotevřou.
///
/// Šumové dno se odhaduje mediánem binů - ten je odolný vůči několika
/// silným stanicím v okně.
fn squelch_line_db(bins: &[f32], span_hz: f64, bandwidth_hz: f64, squelch_db: f32) -> Option<f32> {
    if bins.len() < 16 {
        return None;
    }
    let mut sorted = bins.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_db = sorted[sorted.len() / 2];

    // Šumová šířka jednoho binu; 1.5 je činitel Hannova okna.
    let bin_bw = span_hz / bins.len() as f64 * 1.5;
    let correction = 10.0 * (bandwidth_hz / bin_bw).max(1.0).log10();
    Some(noise_db + squelch_db + correction as f32)
}

/// Viditelný výřez panoramatu: (střed v Hz od VFO, šířka v Hz).
///
/// Výřez se drží naladěné stanice, ale zastaví se u kraje zachyceného
/// spektra - za ním nejsou data, tak nemá smysl tam koukat.
fn view_window(zoom: f32, center_hz: f64, span_hz: f64) -> (f64, f64) {
    let zoom = zoom.clamp(1.0, MAX_ZOOM) as f64;
    let vis = span_hz / zoom;
    let limit = (span_hz - vis) / 2.0;
    (center_hz.clamp(-limit, limit), vis)
}

/// Nový střed výřezu tak, aby naladěná frekvence `offset_hz` byla vidět.
///
/// Dokud je značka uvnitř prostředních ~90 % průzoru (`vis` široký), střed se
/// nehne - spektrum stojí a značka se po něm posouvá. Teprve když by značka
/// vyjela, střed ji dožene právě k okraji. Za zachycené spektrum se nepustí.
/// Právě tohle drží ladění při zoomu stejně srozumitelné jako bez něj.
fn pan_view_center(offset_hz: f64, view_center_hz: f64, vis: f64, span_hz: f64) -> f64 {
    let margin = vis * 0.45;
    let mut c = view_center_hz;
    if offset_hz < c - margin {
        c = offset_hz + margin;
    } else if offset_hz > c + margin {
        c = offset_hz - margin;
    }
    let limit = ((span_hz - vis) / 2.0).max(0.0);
    c.clamp(-limit, limit)
}

/// Nový offset po kroku VFO tak, aby naladění zůstalo na stejné absolutní
/// frekvenci - okno se posune do strany, stanice zůstane, kde byla.
///
/// Když by stanice vyjela z okna, offset se zarazí na jeho kraji; dál už ji
/// udržet nejde, protože mimo zachycené spektrum není co demodulovat.
fn offset_after_vfo_step(offset_hz: f64, applied_khz: f64, span_hz: f64) -> f64 {
    let limit = span_hz * 0.48;
    (offset_hz - applied_khz * 1000.0).clamp(-limit, limit)
}

/// O kolik od VFO (DC) posadit stanici při skoku na oblíbenou.
///
/// Celý kanál [offset ± šířka/2] musí být mimo mrtvou zónu kolem DC, jinak by
/// spur padl dovnitř. U úzkých režimů stačí drobných 10 kHz. U širokého FM
/// (kanál ±90 kHz) je to málo - tam stanici posadíme do poloviny mezi VFO
/// a okraj okna, ať má DC spur i okraj z obou stran rezervu.
fn park_offset(bandwidth_hz: f64, span_hz: f64) -> f64 {
    let needs = bandwidth_hz / 2.0 + DC_GUARD_HZ;
    if needs <= PARK_OFFSET_HZ {
        return PARK_OFFSET_HZ;
    }
    // Půl cesty ke kraji, ale ať se kanál pořád vejde do zachyceného spektra.
    let max_off = (span_hz * 0.45 - bandwidth_hz / 2.0).max(needs);
    (span_hz / 4.0).clamp(needs, max_off)
}

/// Najde nejsilnější stanici v panoramatu a vrátí její offset od středu v Hz.
///
/// Vynechává okolí DC, kde má SoftRock spur (jinak by to skákalo pořád na něj),
/// a okraje, kde padá filtr. Když z šumu nic výrazného nevyčnívá, vrátí None -
/// na prázdném pásmu nemá smysl se ladit na náhodný šum.
fn strongest_offset(bins: &[f32], span_hz: f64) -> Option<f64> {
    let n = bins.len();
    if n < 64 {
        return None;
    }
    let center = n / 2;
    let dc_guard = ((DC_GUARD_HZ / span_hz) * n as f64).round() as usize;
    let edge = n / 20; // 5 % na každé straně

    let mut best: Option<(usize, f32)> = None;
    for i in edge..n - edge {
        if i.abs_diff(center) <= dc_guard {
            continue;
        }
        if best.is_none_or(|(_, b)| bins[i] > b) {
            best = Some((i, bins[i]));
        }
    }
    let (idx, peak) = best?;

    // Musí to vyčnívat nad šumové pozadí, jinak nejde o stanici.
    let mut sorted: Vec<f32> = bins.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[n / 2];
    if peak < median + 6.0 {
        return None;
    }

    Some(((idx as f64 - center as f64) / n as f64 * span_hz).round())
}

/// Vybere krok mřížky tak, aby čar bylo přibližně 6-10.
fn nice_db_step(range: f32) -> f32 {
    for c in [5.0, 10.0, 20.0, 25.0, 50.0] {
        if range / c <= 10.0 {
            return c;
        }
    }
    100.0
}

fn nice_khz_step(span_khz: f64) -> f64 {
    for c in [1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0] {
        if span_khz / c <= 12.0 {
            return c;
        }
    }
    200.0
}

/// Modrá -> azurová -> žlutá -> červená.
fn colormap(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t < 0.33 {
        let u = t / 0.33;
        (0.0, u * 0.7, 0.3 + u * 0.7)
    } else if t < 0.66 {
        let u = (t - 0.33) / 0.33;
        (u, 0.7 + u * 0.3, 1.0 - u)
    } else {
        let u = (t - 0.66) / 0.34;
        (1.0, 1.0 - u * 0.9, 0.0)
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        let (bins, generation) = {
            let sp = self.shared.spectrum.lock().unwrap();
            (sp.bins.clone(), sp.generation)
        };
        if generation != self.last_generation {
            self.last_generation = generation;
            self.push_waterfall_row(&bins);
        }

        // Šířka panoramatu = skutečná vzorkovačka, kterou capture vyjednal.
        let span_hz = self.shared.sample_rate.load(Ordering::Relaxed) as f64;

        // Ladění šipkami. Jen když se needituje text, ať se nekradly klávesy
        // z políčka pro VFO.
        if !ctx.egui_wants_keyboard_input() {
            let (left, right, shift) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.modifiers.shift,
                )
            });
            let mult = if shift { 10.0 } else { 1.0 };
            if left {
                self.tune_by(-self.tune_step_hz() * mult, span_hz);
            }
            if right {
                self.tune_by(self.tune_step_hz() * mult, span_hz);
            }
        }

        // Po skoku za roh počkáme, až se panorama ustálí, a doladíme.
        if self.snap_at.is_some_and(|t| std::time::Instant::now() >= t) {
            self.snap_at = None;
            self.snap_to_strongest(&bins, span_hz);
        }

        // Skener přelaďuje sám; musí dostat slovo dřív, než se vykreslí panel,
        // ať je hned vidět, kde stojí.
        self.scan_tick(span_hz);

        // Přeladili jsme jinam? Pak přečtené RDS patří předchozí stanici a
        // musí pryč - jinak by u nové svítil cizí název a cizí text.
        // Práh je půl rozestupu kanálů: menší posun je doladění téže stanice.
        let ted_khz = self.set.vfo_khz + self.set.offset_hz / 1000.0;
        if (ted_khz - self.rds_khz).abs() > RDS_KEEP_KHZ {
            self.rds_khz = ted_khz;
            self.shared.rds_reset.store(true, Ordering::Relaxed);
            // Ať to zmizí hned, ne až doběhne DSP vlákno.
            if let Ok(mut r) = self.shared.rds.lock() {
                *r = rds::RdsInfo::default();
            }
        }

        let tuned_khz = self.set.vfo_khz + self.set.offset_hz / 1000.0;

        egui::Panel::top("ovladani").show(ui, |ui| {
            ui.add_space(4.0);
            // Zalamovací: do téhle řádky se vešlo ladění, režimy, S-metr,
            // squelch i název stanice z RDS. Bez zalamování se to při užším
            // okně uřízne vpravo a co nepasuje, není vidět vůbec.
            ui.horizontal_wrapped(|ui| {
                ui.label("VFO [kHz]:");
                let resp =
                    ui.add(egui::TextEdit::singleline(&mut self.vfo_input).desired_width(90.0));
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(v) = self.vfo_input.trim().replace(',', ".").parse::<f64>() {
                        self.set_vfo(v);
                        self.center_view_on_tuned();
                    }
                }
                if ui
                    .button("◀ za roh")
                    .on_hover_text(format!(
                        "posun o celé okno ({:.0} kHz) a doladění na nejsilnější stanici",
                        span_hz / 1000.0
                    ))
                    .clicked()
                {
                    self.jump_window(span_hz, -1.0);
                }
                for &d in self.vfo_steps_khz() {
                    // Velké skoky ukazuj v MHz, ať tlačítko není "+1000 k".
                    let label = if d.abs() >= 1000.0 {
                        format!("{:+.0} M", d / 1000.0)
                    } else {
                        format!("{d:+.0} k")
                    };
                    if ui
                        .button(label)
                        .on_hover_text("posune okno, naladěná stanice zůstane")
                        .clicked()
                    {
                        self.step_vfo(d, span_hz);
                    }
                }
                if ui
                    .button("za roh ▶")
                    .on_hover_text(format!(
                        "posun o celé okno ({:.0} kHz) a doladění na nejsilnější stanici",
                        span_hz / 1000.0
                    ))
                    .clicked()
                {
                    self.jump_window(span_hz, 1.0);
                }
                ui.separator();
                // Naladěná frekvence jde přímo přepsat. Ladit jen kolečkem
                // nestačí: na normál se člověk musí trefit přesně, ne po
                // krocích, a naladěno = VFO + offset, takže to zadání VFO
                // neumí.
                ui.label(egui::RichText::new("naladěno").size(18.0).strong());
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.tuned_input)
                        .desired_width(100.0)
                        .font(egui::TextStyle::Heading),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(v) = self.tuned_input.trim().replace(',', ".").parse::<f64>() {
                        let bw = self.bandwidth_hz();
                        self.tune_to(v, self.set.mode, bw);
                    }
                }
                // Dokud v poli nepíšeš, ukazuje aktuální ladění.
                if !resp.has_focus() {
                    self.tuned_input = format!("{tuned_khz:.3}");
                }
                ui.label(egui::RichText::new("kHz").size(18.0).strong());
                // Název stanice z RDS patří rovnou k naladěné frekvenci - je
                // to totéž, co u AM ukazuje rozpis EiBi, jen přímo z éteru.
                //
                // Ukazuje se i když se ještě nic nepřečetlo: jinak by nebylo
                // poznat, jestli stanice RDS nevysílá, nebo je jen slabá -
                // a hlavně by uživatel nevěděl, kam se vůbec dívat.
                if self.set.mode.is_wfm() {
                    let rds = self.shared.rds.lock().unwrap().clone();
                    let stereo = self.shared.stereo_active.load(Ordering::Relaxed);
                    // Pozor: pilot ≠ stereo. Stereo se dá vypnout přepínačem,
                    // ale RDS na pilotu stojí pořád - proto se ptáme na pilot.
                    let pilot = self.shared.pilot_locked.load(Ordering::Relaxed);
                    let pilot_lvl = f32::from_bits(
                        self.shared.pilot_level.load(Ordering::Relaxed),
                    );
                    if !rds.ps.is_empty() {
                        ui.label(
                            egui::RichText::new(&rds.ps)
                                .size(18.0)
                                .strong()
                                .color(egui::Color32::from_rgb(120, 200, 255)),
                        )
                        .on_hover_text(match rds.pi {
                            Some(pi) => format!("název stanice z RDS (PI {pi:04X})"),
                            None => "název stanice z RDS".to_string(),
                        });
                    } else {
                        // Pilot je předpoklad RDS - když není, nemá cenu čekat.
                        // Součástí hlášky je naměřená úroveň pilotu: bez ní by
                        // nebylo poznat, jestli pilot chybí, nebo jen těsně
                        // nedosáhl na práh.
                        let (text, tip) = if pilot {
                            // Počty bloků rovnou v hlášce: bez nich by nešlo
                            // poznat, jestli demodulace nefunguje vůbec, nebo
                            // jen občas propadne slabý signál.
                            let ok = self.shared.rds_good.load(Ordering::Relaxed);
                            let bad = self.shared.rds_bad.load(Ordering::Relaxed);
                            (
                                format!("RDS: hledám… ({ok}/{})", ok + bad),
                                format!(
                                    "pilot je chycený, čekám na datové bloky\n\
                                     bloků prošlo {ok}, neprošlo {bad}\n\
                                     když neprojde nic, je chyba v demodulaci nebo taktu"
                                ),
                            )
                        } else {
                            (
                                format!("RDS: bez pilotu ({pilot_lvl:.3})"),
                                format!(
                                    "pilot 19 kHz nedosáhl na práh {:.3} - naměřeno {pilot_lvl:.4}\n\
                                     bez pilotu se nedá odvodit nosná 57 kHz, na které RDS jede",
                                    dsp::FM_PILOT_LOCK
                                ),
                            )
                        };
                        ui.label(egui::RichText::new(text).weak()).on_hover_text(tip);
                    }
                    if stereo {
                        ui.label(
                            egui::RichText::new("◉ stereo")
                                .color(egui::Color32::from_rgb(80, 200, 90))
                                .strong(),
                        )
                        .on_hover_text("pilot 19 kHz je slyšet, hraje se dvoukanálově");
                    }
                }
                // V jakém úseku pásma zrovna jsme.
                if let Some(s) = bandplan::at(tuned_khz) {
                    let (r, g, b) = s.usage.color();
                    ui.label(
                        egui::RichText::new(format!("{} · {}", s.band, s.usage.label()))
                            .color(egui::Color32::from_rgb(r, g, b)),
                    );
                }
                ui.separator();
                for m in [
                    dsp::Mode::Am,
                    dsp::Mode::Usb,
                    dsp::Mode::Lsb,
                    dsp::Mode::Cw,
                    dsp::Mode::Nfm,
                    dsp::Mode::Wfm,
                ] {
                    // WFM má smysl jen na širokopásmovém vstupu (RSP1); na
                    // krátkovlnném SoftRocku by z 96 kHz FM nešla, tak ho zašedni.
                    let ok = !m.is_wfm() || self.set.hardware == source::Hardware::Rsp1;
                    let resp = ui
                        .add_enabled_ui(ok, |ui| {
                            ui.selectable_value(&mut self.set.mode, m, m.label())
                        })
                        .inner;
                    if m.is_wfm() && !ok {
                        resp.on_hover_text("WFM potřebuje širokopásmové rádio (RSP1)");
                    }
                }
                ui.separator();
                self.s_meter(ui);
                // Šumová brána hned u S-metru - práh je ve stejné stupnici (dBFS),
                // takže je vidět, kam vůči síle signálu čáru squelche stavíš.
                ui.checkbox(&mut self.set.squelch_on, "squelch")
                    .on_hover_text("umlčí zvuk, když signál klesne pod práh - ať mezi stanicemi nesyčí");
                ui.add_enabled_ui(self.set.squelch_on, |ui| {
                    ui.add(
                        egui::Slider::new(
                            &mut self.set.squelch_db,
                            radio::SQUELCH_MIN_DB..=radio::SQUELCH_MAX_DB,
                        )
                        .fixed_decimals(0)
                        .suffix(" dBFS"),
                    )
                    .on_hover_text("práh brány (oranžová čára v panoramatu) - výš = otevře jen silnější signál");
                });
                ui.separator();
                if ui
                    .button("⌖ nejsilnější")
                    .on_hover_text("doladit na nejsilnější stanici v okně")
                    .clicked()
                {
                    self.snap_to_strongest(&bins, span_hz);
                }
            });
            ui.add_space(2.0);
            // Zalamovací - na RSP1 sem přibude zisk a vzorkovačka, ať se to
            // na užším okně přelije na druhý řádek místo useknutí.
            ui.horizontal_wrapped(|ui| {
                // Přepínač rádia - přepne se hned za běhu, bez restartu.
                ui.label("rádio:");
                let mut switch_hw = None;
                egui::ComboBox::from_id_salt("hw_top")
                    .selected_text(self.set.hardware.label())
                    .show_ui(ui, |ui| {
                        for hw in source::Hardware::ALL {
                            let vybrano = self.set.hardware == hw;
                            if hw.available() {
                                if ui.selectable_label(vybrano, hw.label()).clicked() && !vybrano {
                                    switch_hw = Some(hw);
                                }
                            } else {
                                ui.add_enabled_ui(false, |ui| {
                                    let _ = ui.selectable_label(vybrano, hw.label());
                                })
                                .response
                                .on_hover_text("není v tomhle sestavení - chce Linux a feature `rsp1`");
                            }
                        }
                    });
                if let Some(hw) = switch_hw {
                    self.set.hardware = hw;
                    // Nové rádio má jiný rozsah - přeladit do něj, ať SoftRock
                    // nezůstane viset na kmitočtu, kam dosáhne jen RSP1.
                    self.set_vfo(self.set.vfo_khz);
                    // SoftRock WFM neumí - přepni na AM, ať nezní šum z prázdné FM.
                    if self.set.mode.is_wfm() && hw != source::Hardware::Rsp1 {
                        self.set.mode = dsp::Mode::Am;
                    }
                    self.request_reopen();
                }

                // Ovládání specifické pro RSP1 patří na panel, ne do nastavení -
                // zisk se ladí za poslechu, vzorkovačka mění šířku panoramatu.
                if self.set.hardware == source::Hardware::Rsp1 {
                    ui.separator();
                    if let Some(g) = *self.shared.gain_range.lock().unwrap() {
                        ui.label("zisk:");
                        if ui
                            .add(
                                egui::Slider::new(&mut self.set.rsp1_gain_db, g.min..=g.max)
                                    .fixed_decimals(1)
                                    .suffix(" dB"),
                            )
                            .on_hover_text("zesílení LNA - projeví se hned")
                            .changed()
                        {
                            let _ = self.gain_tx.send(self.set.rsp1_gain_db);
                        }
                    }
                    let puvodni = self.set.rsp1_rate_hz;
                    egui::ComboBox::from_id_salt("rsp1_rate_top")
                        .selected_text(format!("{:.3} MHz", puvodni / 1e6))
                        .show_ui(ui, |ui| {
                            for &r in source::RSP1_RATES_HZ {
                                ui.selectable_value(
                                    &mut self.set.rsp1_rate_hz,
                                    r,
                                    format!("{:.3} MHz · panorama {:.0} kHz", r / 1e6, r / 1000.0),
                                );
                            }
                        });
                    if self.set.rsp1_rate_hz != puvodni {
                        self.request_reopen();
                    }
                }

                ui.separator();
                ui.label("hlasitost:");
                ui.add(egui::Slider::new(&mut self.set.volume, 0.0..=1.0).show_value(false));
                ui.separator();
                ui.checkbox(&mut self.set.swap_iq, "prohodit I/Q");
                ui.checkbox(&mut self.set.show_bandplan, "bandplan")
                    .on_hover_text("podbarvení úseků pásem (IARU R1)");
                ui.checkbox(&mut self.set.show_console, "konzole")
                    .on_hover_text("dekódovaný text z RTTY a CW");
                if ui
                    .button("⚙ nastavení")
                    .on_hover_text("zvuková zařízení, bitová hloubka, Si570")
                    .clicked()
                {
                    self.show_options = !self.show_options;
                }
                ui.separator();
                {
                    let (bw_min, bw_max) = radio::bandwidth_range(self.set.mode);
                    let mut bw_khz = self.bandwidth_hz() / 1000.0;
                    // U WFM jsou to stovky kHz, tam desetiny nedávají smysl.
                    let wfm = self.set.mode.is_wfm();
                    let resp = ui.add(
                        egui::Slider::new(&mut bw_khz, bw_min / 1000.0..=bw_max / 1000.0)
                            .text("šířka [kHz]")
                            .fixed_decimals(if wfm { 0 } else { 1 }),
                    );
                    if resp.changed() {
                        self.set_bandwidth_hz(bw_khz * 1000.0);
                    }
                    if wfm {
                        resp.on_hover_text(
                            "šířka mezifrekvenční propusti\n\
                             úžeji = odolnější proti sousední stanici, ale zkreslenější zvuk",
                        );
                    }
                    ui.separator();
                }
                ui.label("zoom:");
                if ui.button("−").clicked() {
                    self.set_zoom(self.set.zoom / 2.0);
                }
                ui.label(format!("{:.0}×", self.set.zoom));
                if ui.button("+").clicked() {
                    self.set_zoom(self.set.zoom * 2.0);
                }
                if ui
                    .button("celé")
                    .on_hover_text("oddálit na celou vzorkovačku (nebo Ctrl+kolečko)")
                    .clicked()
                {
                    self.set_zoom(1.0);
                }
                ui.separator();
                ui.label("dB rozsah:");
                ui.add(egui::Slider::new(&mut self.set.db_min, -140.0..=-40.0).text("min"));
                ui.add(egui::Slider::new(&mut self.set.db_max, -60.0..=0.0).text("max"));
            });
            ui.add_space(2.0);
            // Zvuk a skenování zvlášť, a navíc sbalitelně - ovládání je toho
            // hodně a při běžném poslechu se do většiny z něj nesahá.
            let audio_row = egui::CollapsingHeader::new("zvuk a skenování")
                .default_open(self.set.show_audio_row)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        self.agc_controls(ui);
                        ui.separator();
                        self.notch_controls(ui);
                        ui.separator();
                        self.record_controls(ui);
                        ui.separator();
                        self.scan_controls(ui, span_hz);
                        // Stereo jen ve WFM - jinde by přepínač nic nedělal.
                        if self.set.mode.is_wfm() {
                            ui.separator();
                            self.wfm_controls(ui);
                        }
                    });
                });
            // Jestli je sekce rozbalená, si pamatujeme do příště.
            self.set.show_audio_row = audio_row.openness > 0.5;
            ui.add_space(2.0);
            self.band_buttons(ui, tuned_khz);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("stav").show(ui, |ui| {
            let status = self.shared.status.lock().unwrap().clone();
            let hw = self.shared.hw_status.lock().unwrap().clone();
            // Taky zalamovací - RadioText bývá dlouhý a jinak by se uřízl.
            ui.horizontal_wrapped(|ui| {
                ui.label(status);
                ui.separator();
                ui.label(hw);
                // RadioText bývá dlouhý a průběžně se mění - do horního panelu
                // by nešel, tady má místo a nikomu nepřekáží.
                if self.set.mode.is_wfm() {
                    let rt = self.shared.rds.lock().unwrap().rt.clone();
                    if !rt.is_empty() {
                        ui.separator();
                        ui.label(egui::RichText::new(format!("RDS: {}", zkrat(&rt, 70))).weak())
                            .on_hover_text(&rt);
                    }
                }
            });
        });

        self.favourites_panel(ui);
        self.manage_window(&ctx);
        self.options_window(&ctx);
        // Konzole až po stavovém řádku, ať sedí nad ním.
        if self.set.show_console {
            self.console_panel(ui);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let spec_h = full.height() * 0.35;

            // Viditelný výřez; při zoomu 1 je to celá vzorkovačka.
            let (view_c, view_w) = self.view(span_hz);
            // Převod frekvenčního offsetu na x - společné pro všechny plochy.
            let x_of = |rect: &egui::Rect, hz: f64| -> f32 {
                rect.center().x + ((hz - view_c) / view_w) as f32 * rect.width()
            };
            let (band_lo, band_hi) = self.band_edges();

            // --- Panorama ---
            let (resp, painter) = ui.allocate_painter(
                egui::vec2(full.width(), spec_h),
                egui::Sense::click_and_drag(),
            );
            let rect = resp.rect;
            painter.rect_filled(rect, 0.0, egui::Color32::from_gray(16));

            // Bandplan: podbarvení úseků podle druhu provozu. Kreslí se jako
            // první, ať je pod mřížkou i signálem.
            if self.set.show_bandplan {
                let lo_khz = self.set.vfo_khz + (view_c - view_w / 2.0) / 1000.0;
                let hi_khz = self.set.vfo_khz + (view_c + view_w / 2.0) / 1000.0;
                for s in bandplan::overlapping(lo_khz, hi_khz) {
                    let x0 = x_of(&rect, (s.from_khz - self.set.vfo_khz) * 1000.0)
                        .max(rect.left());
                    let x1 =
                        x_of(&rect, (s.to_khz - self.set.vfo_khz) * 1000.0).min(rect.right());
                    if x1 <= x0 {
                        continue;
                    }
                    let (r, g, b) = s.usage.color();
                    painter.rect_filled(
                        egui::Rect::from_x_y_ranges(x0..=x1, rect.y_range()),
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(r, g, b, s.usage.fill_alpha()),
                    );
                    // Popisek u levého kraje úseku, ne doprostřed: široký
                    // úsek má střed přesně tam, kde je ryska VFO, a lezlo
                    // by to jedno přes druhé.
                    if x1 - x0 > 50.0 {
                        painter.text(
                            egui::pos2(x0 + 4.0, rect.top() + 2.0),
                            egui::Align2::LEFT_TOP,
                            format!("{} {}", s.band, s.usage.label()),
                            egui::FontId::proportional(12.0),
                            egui::Color32::from_rgba_unmultiplied(r, g, b, 230),
                        );
                    }
                }
            }

            // Vodorovná mřížka po dB
            let db_step = nice_db_step(self.set.db_max - self.set.db_min);
            let first = (self.set.db_min / db_step).ceil() * db_step;
            let mut db = first;
            while db <= self.set.db_max {
                let t = (db - self.set.db_min) / (self.set.db_max - self.set.db_min);
                let y = rect.bottom() - rect.height() * t;
                painter.line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(45)),
                );
                painter.text(
                    egui::pos2(rect.left() + 3.0, y),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{db:.0} dB"),
                    egui::FontId::proportional(10.0),
                    egui::Color32::from_gray(130),
                );
                db += db_step;
            }

            // Svislá mřížka po kHz, popisky v absolutní frekvenci.
            // Krok se počítá z viditelné šířky, ať mřížka při zoomu zhoustne.
            let khz_step = nice_khz_step(view_w / 1000.0);
            let lo_khz = (view_c - view_w / 2.0) / 1000.0;
            let hi_khz = (view_c + view_w / 2.0) / 1000.0;
            let mut k = (lo_khz / khz_step).ceil() * khz_step;
            let mut grid_lines: Vec<(f32, f64)> = Vec::new();
            while k <= hi_khz {
                grid_lines.push((x_of(&rect, k * 1000.0), self.set.vfo_khz + k));
                k += khz_step;
            }
            // Jen čáry; čísla jdou do vlastního pruhu pod spektrem, jinak by
            // je překreslil signál.
            for &(x, _) in &grid_lines {
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(45)),
                );
            }

            // Propustné pásmo demodulátoru
            let bw_rect = egui::Rect::from_x_y_ranges(
                x_of(&rect, band_lo)..=x_of(&rect, band_hi),
                rect.y_range(),
            );
            painter.rect_filled(
                bw_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(90, 160, 255, 40),
            );

            // Práh CW dekodéru: nad touhle čárou signál dekodér otevře. Je to
            // něco jiného než šumová brána zvuku níž - ta se kreslí zvlášť a
            // má práh v absolutních dBFS, kdežto tahle čára stojí nad šumem.
            if self.active_decoder() == decode::Decoder::Cw {
                if let Some(thr) = squelch_line_db(
                    &bins,
                    span_hz,
                    self.bandwidth_hz(),
                    self.set.cw_squelch_db,
                ) {
                    let t = ((thr - self.set.db_min) / (self.set.db_max - self.set.db_min))
                        .clamp(0.0, 1.0);
                    let y = rect.bottom() - rect.height() * t;
                    painter.line_segment(
                        [
                            egui::pos2(bw_rect.left(), y),
                            egui::pos2(bw_rect.right(), y),
                        ],
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 210, 60)),
                    );
                    painter.text(
                        egui::pos2(bw_rect.right() - 2.0, y - 1.0),
                        egui::Align2::RIGHT_BOTTOM,
                        "dekodér",
                        egui::FontId::proportional(9.0),
                        egui::Color32::from_rgb(255, 210, 60),
                    );
                }
            }

            // Šumová brána zvuku: pod touhle čárou se zvuk umlčí. Práh je v
            // dBFS - stejná stupnice jako panorama i S-metr - tak leží přímo
            // na dB ose. Čára jde přes propustné pásmo, kterého se squelch týká.
            if self.set.squelch_on {
                let t = ((self.set.squelch_db - self.set.db_min)
                    / (self.set.db_max - self.set.db_min))
                    .clamp(0.0, 1.0);
                let y = rect.bottom() - rect.height() * t;
                painter.line_segment(
                    [egui::pos2(bw_rect.left(), y), egui::pos2(bw_rect.right(), y)],
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 130, 40)),
                );
                painter.text(
                    egui::pos2(bw_rect.left() + 2.0, y - 1.0),
                    egui::Align2::LEFT_BOTTOM,
                    "squelch",
                    egui::FontId::proportional(9.0),
                    egui::Color32::from_rgb(255, 130, 40),
                );
            }

            // Notch: svislá značka tam, kde leží umlčený tón. Notch pracuje na
            // audiu, tohle je jeho obraz ve spektru - vidíš, kterou nosnou bereš.
            if self.set.notch_on {
                for off in self.notch_rf_offsets() {
                    let x = x_of(&rect, off);
                    if x < rect.left() || x > rect.right() {
                        continue;
                    }
                    painter.line_segment(
                        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(230, 90, 200)),
                    );
                    painter.text(
                        egui::pos2(x + 3.0, rect.top() + 2.0),
                        egui::Align2::LEFT_TOP,
                        "notch",
                        egui::FontId::proportional(9.0),
                        egui::Color32::from_rgb(230, 90, 200),
                    );
                }
            }

            // Kreslíme jen biny uvnitř výřezu - jinak by se při zoomu počítaly
            // tisíce bodů mimo obrazovku.
            let n = bins.len().max(2);
            let idx_of_hz = |hz: f64| ((hz / span_hz + 0.5) * n as f64).round() as isize;
            let i0 = idx_of_hz(view_c - view_w / 2.0).clamp(0, n as isize - 1) as usize;
            let i1 = idx_of_hz(view_c + view_w / 2.0).clamp(0, n as isize - 1) as usize;
            let pts: Vec<egui::Pos2> = (i0..=i1)
                .map(|i| {
                    let hz = (i as f64 / n as f64 - 0.5) * span_hz;
                    let db = bins[i];
                    let t = ((db - self.set.db_min) / (self.set.db_max - self.set.db_min))
                        .clamp(0.0, 1.0);
                    egui::pos2(x_of(&rect, hz), rect.bottom() - rect.height() * t)
                })
                .collect();
            painter.add(egui::Shape::line(
                pts,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 220, 120)),
            ));

            // Mrtvá zóna kolem VFO: uprostřed je DC se spurem a nevyvážením
            // I/Q, takže se sem stanice ladit nemá.
            let dead = egui::Rect::from_x_y_ranges(
                x_of(&rect, -DC_GUARD_HZ)..=x_of(&rect, DC_GUARD_HZ),
                rect.y_range(),
            );
            painter.rect_filled(
                dead,
                0.0,
                egui::Color32::from_rgba_unmultiplied(255, 140, 0, 30),
            );
            painter.line_segment(
                [
                    egui::pos2(x_of(&rect, 0.0), rect.top()),
                    egui::pos2(x_of(&rect, 0.0), rect.bottom()),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 170, 40)),
            );
            // Popisek VFO je níž, ať se nepere s popisky pásem u horní hrany.
            painter.text(
                egui::pos2(x_of(&rect, 0.0) + 3.0, rect.top() + 18.0),
                egui::Align2::LEFT_TOP,
                "VFO",
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(255, 170, 40),
            );

            // Značka naladění
            let mark_x = x_of(&rect, self.set.offset_hz);
            painter.line_segment(
                [
                    egui::pos2(mark_x, rect.top()),
                    egui::pos2(mark_x, rect.bottom()),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 80, 80)),
            );

            self.tune_interaction(ui, &resp, rect, span_hz);

            // --- Pruh s frekvenční osou ---
            // Vlastní plocha, ať se popisky nepraly se signálem ve spektru.
            let (axis_resp, axis_painter) =
                ui.allocate_painter(egui::vec2(full.width(), AXIS_H), egui::Sense::hover());
            let ar = axis_resp.rect;
            axis_painter.rect_filled(ar, 0.0, egui::Color32::from_gray(28));
            for &(x, abs_khz) in &grid_lines {
                axis_painter.line_segment(
                    [egui::pos2(x, ar.top()), egui::pos2(x, ar.top() + 3.0)],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                );
                axis_painter.text(
                    egui::pos2(x, ar.top() + 3.0),
                    egui::Align2::CENTER_TOP,
                    format!("{abs_khz:.0}"),
                    egui::FontId::proportional(10.0),
                    egui::Color32::from_gray(190),
                );
            }
            // Kde jsme naladěni, ať se to dá odečíst i z osy.
            axis_painter.line_segment(
                [
                    egui::pos2(x_of(&ar, self.set.offset_hz), ar.top()),
                    egui::pos2(x_of(&ar, self.set.offset_hz), ar.bottom()),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 80, 80)),
            );

            // --- Vodopád ---
            let img =
                egui::ColorImage::from_rgba_unmultiplied([FFT_SIZE, WF_HEIGHT], &self.wf_pixels);
            match &mut self.wf_tex {
                Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.wf_tex =
                        Some(ctx.load_texture("waterfall", img, egui::TextureOptions::LINEAR));
                }
            }
            if let Some(tex) = &self.wf_tex {
                // Zoom vodopádu = výřez z textury přes UV, historie zůstane.
                let u0 = ((view_c - view_w / 2.0) / span_hz + 0.5) as f32;
                let u1 = ((view_c + view_w / 2.0) / span_hz + 0.5) as f32;
                let wf_resp = ui.add(
                    egui::Image::new(tex)
                        .uv(egui::Rect::from_min_max(
                            egui::pos2(u0, 0.0),
                            egui::pos2(u1, 1.0),
                        ))
                        .fit_to_exact_size(egui::vec2(full.width(), full.bottom() - ar.bottom()))
                        .sense(egui::Sense::click_and_drag()),
                );
                let wr = wf_resp.rect;
                let over = ui.painter_at(wr);

                // Mřížka i pásmo se kreslí přes vodopád, ať jsou obě plochy zarovnané.
                for &(x, _) in &grid_lines {
                    over.line_segment(
                        [egui::pos2(x, wr.top()), egui::pos2(x, wr.bottom())],
                        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 25)),
                    );
                }
                // Mrtvá zóna kolem VFO i tady, ať je vidět, kam neladit.
                over.rect_filled(
                    egui::Rect::from_x_y_ranges(
                        x_of(&wr, -DC_GUARD_HZ)..=x_of(&wr, DC_GUARD_HZ),
                        wr.y_range(),
                    ),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(255, 140, 0, 25),
                );
                over.line_segment(
                    [
                        egui::pos2(x_of(&wr, 0.0), wr.top()),
                        egui::pos2(x_of(&wr, 0.0), wr.bottom()),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 170, 40, 130)),
                );
                for edge in [band_lo, band_hi] {
                    let x = x_of(&wr, edge);
                    over.line_segment(
                        [egui::pos2(x, wr.top()), egui::pos2(x, wr.bottom())],
                        egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgba_unmultiplied(120, 180, 255, 110),
                        ),
                    );
                }
                over.line_segment(
                    [
                        egui::pos2(x_of(&wr, self.set.offset_hz), wr.top()),
                        egui::pos2(x_of(&wr, self.set.offset_hz), wr.bottom()),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 80, 80, 140)),
                );

                self.tune_interaction(ui, &wf_resp, wr, span_hz);
            }
        });

        // Velikost okna si bereme z egui plochy, ať se uloží i po ručním resize.
        let size = ctx.input(|i| i.viewport_rect().size());
        if size.x > 0.0 && size.y > 0.0 {
            self.set.window_w = size.x;
            self.set.window_h = size.y;
        }

        // Průběžně si pamatujeme, kde na pásmu zrovna jsme, ať se tam
        // tlačítko pásma umí vrátit i po restartu.
        self.remember_band();

        self.push_controls();
        self.autosave.tick(self.set.clone());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.autosave.flush();
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPAN: f64 = 96_000.0;

    /// Dokud je značka uvnitř průzoru, spektrum se nesmí hnout - to byl přesně
    /// ten zmatek: při zoomu se výřez pořád vycentrovával a spektrum trhalo.
    #[test]
    fn vyrez_stoji_dokud_je_znacka_uvnitr() {
        let vis = SPAN / 8.0; // zoom 8
        let center = 0.0;
        // Malé doladění blízko středu nesmí pohnout výřezem.
        assert_eq!(pan_view_center(500.0, center, vis, SPAN), center);
        assert_eq!(pan_view_center(-500.0, center, vis, SPAN), center);
        // Až u kraje (nad 45 % z poloviny šířky) se dožene.
        let margin = vis * 0.45;
        assert_eq!(pan_view_center(margin, center, vis, SPAN), center);
        let far = margin + 1_000.0;
        let moved = pan_view_center(far, center, vis, SPAN);
        assert!(moved > center, "výřez se u kraje musí posunout za značkou");
        // A posune se právě tak, aby značka seděla na okraji.
        assert!((far - moved - margin).abs() < 1e-6);
    }

    /// Při zoomu 1 je výřez celé spektrum, takže jeho střed je vždy 0 -
    /// značka se pohybuje po stojícím spektru (chování bez zoomu).
    #[test]
    fn bez_zoomu_vyrez_stoji_na_stredu() {
        assert_eq!(pan_view_center(30_000.0, 0.0, SPAN, SPAN), 0.0);
        assert_eq!(pan_view_center(-40_000.0, 10_000.0, SPAN, SPAN), 0.0);
    }

    /// Úzké režimy parkují blízko (10 kHz), ale široký FM kanál musí skočit
    /// dál od DC, jinak by spur padl doprostřed kanálu a nehrálo by to.
    #[test]
    fn park_offset_podle_sirky_kanalu() {
        let rsp1 = 1_344_000.0;
        // SSB/AM: drobný posun zůstává.
        assert_eq!(park_offset(2_700.0, rsp1), PARK_OFFSET_HZ);
        assert_eq!(park_offset(8_000.0, rsp1), PARK_OFFSET_HZ);
        // WFM (180 kHz): skočí do poloviny mezi VFO a okraj.
        let wfm = park_offset(180_000.0, rsp1);
        assert_eq!(wfm, rsp1 / 4.0, "WFM má skočit do čtvrtiny šířky = půl k okraji");
        // A celý kanál je mimo DC spur i uvnitř zachyceného spektra.
        assert!(wfm - 90_000.0 > DC_GUARD_HZ, "kanál zasahuje do DC zóny");
        assert!(wfm + 90_000.0 < rsp1 * 0.5, "kanál vyčnívá ze spektra");
    }

    /// Střed výřezu se nikdy nedostane za zachycené spektrum.
    #[test]
    fn pan_stredu_nevyjede_ze_spektra() {
        let vis = SPAN / 4.0;
        let limit = (SPAN - vis) / 2.0;
        // Značka až u kraje spektra: střed se zarazí na mezi, ne dál.
        let c = pan_view_center(SPAN / 2.0, 0.0, vis, SPAN);
        assert!(c <= limit + 1e-6, "střed {c} přesáhl mez {limit}");
    }

    /// Panorama ze samého šumu s volitelnými špičkami na daných offsetech.
    fn bins_with(peaks: &[(f64, f32)]) -> Vec<f32> {
        let n = FFT_SIZE;
        let mut bins = vec![-110.0f32; n];
        for &(hz, db) in peaks {
            let idx = (n as f64 / 2.0 + hz / SPAN * n as f64).round() as usize;
            bins[idx] = db;
        }
        bins
    }

    /// Přiblížení se musí soustředit na naladěnou stanici.
    ///
    /// Dřív se jen hlídalo, že značka nevypadne z obrazu, což ji při zoomu
    /// posadilo těsně k okraji - stanice pak "ujížděla pryč". Tady se ověřuje,
    /// že po vycentrování zůstane naladění uvnitř výřezu, a to i u kraje
    /// spektra, kde se průzor zaráží a doprostřed ho posadit nejde.
    #[test]
    fn zoom_soustredi_pohled_na_ladeni() {
        const SPAN: f64 = 1_344_000.0;
        for zoom in [1.0f32, 2.0, 4.0, 8.0, 16.0, 32.0] {
            for offset in [0.0f64, 1_000.0, -50_000.0, 300_000.0, -660_000.0, 671_000.0] {
                // Vycentrování na naladění, pak výřez se zarážkou u kraje.
                let (c, vis) = view_window(zoom, offset, SPAN);
                let lo = c - vis / 2.0;
                let hi = c + vis / 2.0;
                // Naladění musí být vidět - s drobnou rezervou na kraj spektra.
                let uvnitr_spektra = offset.abs() <= SPAN / 2.0;
                if uvnitr_spektra {
                    assert!(
                        offset >= lo - 1.0 && offset <= hi + 1.0,
                        "zoom {zoom}x, offset {offset}: výřez {lo}..{hi} naladění nezahrnuje"
                    );
                }
            }
        }
    }

    /// Kalibrace nesmí záviset na tom, jestli jsi trefil nominál na hertz.
    ///
    /// Tohle byla skutečná vada: vzorec předpokládal ladění přesně na
    /// nominál, takže se každé nedoladění připočetlo k chybě krystalu.
    /// Dvě měření téhož rádia na 10 MHz a 9996 kHz pak vyšla o 2,4 ppm jinak.
    #[test]
    fn kalibrace_nezavisi_na_presnosti_naladeni() {
        // Rádio má chybu -12 ppm: nosnou vidí o 12 ppm výš, než doopravdy je.
        let ppm_skutecne = -12.0;
        let nominal = 9_996_000.0f64;

        // Zkusíme se naladit přesně, o 30 Hz níž i o 80 Hz výš.
        for odchylka_ladeni in [0.0f64, -30.0, 80.0] {
            let dial = nominal + odchylka_ladeni;
            // Kde se nosná objeví na stupnici a jaký z toho bude tón.
            let nosna_na_stupnici = nominal - dial * ppm_skutecne * 1e-6;
            let ton = dsp::CW_PITCH_HZ + (nosna_na_stupnici - dial);

            let zmereno = ppm_z_normalu(dial, nominal, ton);
            assert!(
                (zmereno - ppm_skutecne).abs() < 0.01,
                "naladěno o {odchylka_ladeni} Hz vedle: vyšlo {zmereno:+.3} ppm, \
                 čekáno {ppm_skutecne:+.3} ppm"
            );
        }
    }

    /// Znaménko: nosná pod nominálem znamená, že rádio ladí vysoko.
    #[test]
    fn kalibrace_ma_spravne_znamenko() {
        let nominal = 10_000_000.0;
        // Tón nižší než pitch = nosná leží pod naladěním = rádio ladí výš.
        let vyssi = ppm_z_normalu(nominal, nominal, dsp::CW_PITCH_HZ - 100.0);
        assert!(vyssi > 0.0, "mělo vyjít kladné, vyšlo {vyssi}");
        assert!((vyssi - 10.0).abs() < 0.01, "100 Hz na 10 MHz = 10 ppm, vyšlo {vyssi}");

        let nizsi = ppm_z_normalu(nominal, nominal, dsp::CW_PITCH_HZ + 100.0);
        assert!((nizsi + 10.0).abs() < 0.01, "mělo být -10 ppm, vyšlo {nizsi}");
    }

    /// Čára squelche musí ležet nad šumem přesně o práh plus přepočet
    /// na šířku filtru - jinak by slibovala dekódování tam, kde se mlčí.
    #[test]
    fn cara_squelche_sedi_nad_sumem() {
        let bins = vec![-110.0f32; FFT_SIZE];
        // Bin má při 96 kHz a 2048 binech šumovou šířku 47*1.5 = 70 Hz.
        // Filtr 700 Hz je tedy 10x širší -> korekce +10 dB.
        let thr = squelch_line_db(&bins, SPAN, 703.125, 10.0).unwrap();
        assert!(
            (thr - (-110.0 + 10.0 + 10.0)).abs() < 0.5,
            "čára na {thr} dB, čekáno -90 dB (šum -110, squelch 10, korekce 10)"
        );
    }

    #[test]
    fn cara_squelche_reaguje_na_prah_i_sirku() {
        let bins = vec![-100.0f32; FFT_SIZE];
        let a = squelch_line_db(&bins, SPAN, 500.0, 10.0).unwrap();
        let b = squelch_line_db(&bins, SPAN, 500.0, 20.0).unwrap();
        assert!((b - a - 10.0).abs() < 0.01, "zvýšení prahu o 10 dB má čáru zvednout o 10");
        // Dvojnásobná šířka filtru = dvojnásobek šumu = o 3 dB výš.
        let c = squelch_line_db(&bins, SPAN, 1000.0, 10.0).unwrap();
        assert!((c - a - 3.0).abs() < 0.1, "dvojnásobná šířka má čáru zvednout o ~3 dB");
    }

    /// Silné stanice v okně nesmí odhad šumu vytáhnout nahoru.
    #[test]
    fn odhad_sumu_odola_stanicim() {
        let mut bins = vec![-110.0f32; FFT_SIZE];
        for i in 0..FFT_SIZE / 4 {
            bins[i] = -20.0;
        }
        let thr = squelch_line_db(&bins, SPAN, 500.0, 10.0).unwrap();
        assert!(thr < -80.0, "medián se nechal vytáhnout stanicemi: {thr} dB");
    }

    #[test]
    fn zoom_1_ukazuje_cele_spektrum() {
        let (c, w) = view_window(1.0, 0.0, SPAN);
        assert_eq!(w, SPAN);
        assert_eq!(c, 0.0, "při zoomu 1 nemá být kam posouvat");
    }

    #[test]
    fn zoom_zuzuje_vyrez_a_sleduje_ladeni() {
        let (c, w) = view_window(4.0, 10_000.0, SPAN);
        assert_eq!(w, SPAN / 4.0);
        assert_eq!(c, 10_000.0, "výřez se má vycentrovat na naladěnou stanici");
    }

    /// Výřez nesmí ukazovat mimo zachycené spektrum - tam nejsou data.
    #[test]
    fn vyrez_nevyjede_ze_spektra() {
        for zoom in [1.0, 2.0, 4.0, 8.0, 32.0] {
            for off in [-48_000.0, -40_000.0, 0.0, 40_000.0, 48_000.0] {
                let (c, w) = view_window(zoom, off, SPAN);
                assert!(
                    c - w / 2.0 >= -SPAN / 2.0 - 1e-6 && c + w / 2.0 <= SPAN / 2.0 + 1e-6,
                    "zoom {zoom}, offset {off}: výřez {}..{} je mimo +-{}",
                    c - w / 2.0,
                    c + w / 2.0,
                    SPAN / 2.0
                );
            }
        }
    }

    /// Jádro chování: krok VFO posune okno, ale naladěná absolutní
    /// frekvence se nesmí hnout.
    #[test]
    fn krok_vfo_drzi_naladenou_stanici() {
        let vfo_khz = 7300.0;
        let offset = 12_000.0;
        let tuned = vfo_khz + offset / 1000.0;
        for step in [1.0, -1.0, 10.0, -10.0] {
            let new_off = offset_after_vfo_step(offset, step, SPAN);
            let new_tuned = (vfo_khz + step) + new_off / 1000.0;
            assert!(
                (new_tuned - tuned).abs() < 1e-6,
                "krok {step:+} kHz: naladěno {new_tuned} místo {tuned}"
            );
        }
    }

    #[test]
    fn offset_nevyjede_z_okna() {
        // Opakovanými kroky jedním směrem stanice nakonec z okna vyjede;
        // offset se musí zarazit na kraji, ne utéct mimo spektrum.
        let mut off = 0.0;
        for _ in 0..20 {
            off = offset_after_vfo_step(off, 10.0, SPAN);
        }
        assert!(
            off.abs() <= SPAN * 0.48 + 1.0,
            "offset {off} Hz utekl mimo okno +-{} Hz",
            SPAN * 0.48
        );
    }

    #[test]
    fn najde_nejsilnejsi_stanici() {
        let bins = bins_with(&[(-20_000.0, -70.0), (12_000.0, -50.0), (30_000.0, -80.0)]);
        let off = strongest_offset(&bins, SPAN).expect("stanici mělo najít");
        assert!(
            (off - 12_000.0).abs() < 100.0,
            "našlo {off} Hz místo 12000 Hz"
        );
    }

    #[test]
    fn ignoruje_spur_na_dc() {
        // Spur uprostřed je silnější než stanice - přesto se má vybrat stanice.
        let bins = bins_with(&[(0.0, -30.0), (15_000.0, -60.0)]);
        let off = strongest_offset(&bins, SPAN).expect("stanici mělo najít");
        assert!(
            (off - 15_000.0).abs() < 100.0,
            "skočilo na {off} Hz, nejspíš na DC spur"
        );
    }

    #[test]
    fn na_prazdnem_pasmu_nic_nevraci() {
        // Samý šum bez špičky - nemá smysl se ladit na náhodné místo.
        let bins = vec![-110.0f32; FFT_SIZE];
        assert!(strongest_offset(&bins, SPAN).is_none());
    }

    #[test]
    fn ignoruje_okraje_panoramatu() {
        // Špička úplně na kraji je artefakt filtru, ne stanice.
        let bins = bins_with(&[(-47_000.0, -20.0), (8_000.0, -60.0)]);
        let off = strongest_offset(&bins, SPAN).expect("stanici mělo najít");
        assert!((off - 8_000.0).abs() < 100.0, "vzalo okrajový artefakt: {off} Hz");
    }
}
