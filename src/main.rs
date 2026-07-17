//! KnoflikSDR - přijímač pro SoftRock.
//!
//! I/Q ze zvukové karty, ladění Si570 přes USB, panorama + vodopád,
//! režimy AM/USB/LSB a oblíbené stanice.

mod audio;
mod bandplan;
mod decode;
mod dsp;
mod radio;
mod settings;
mod schedule;
mod si570;

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

    // Audio ring: ~0.5 s rezerva na 48 kHz.
    let (audio_tx, audio_rx) = rtrb::RingBuffer::<f32>::new(24_000);

    audio::spawn(
        audio_rx,
        saved.playback_device.clone(),
        radio::AUDIO_RATE as u32,
        shared.running.clone(),
    );
    radio::spawn(
        saved.capture_device.clone(),
        saved.depth,
        shared.clone(),
        audio_tx,
    );
    let tuner = spawn_tuner(shared.clone(), saved.si570_xtal_hz, saved.si570_i2c_addr);

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
        Box::new(move |cc| Ok(Box::new(App::new(cc, app_shared, tuner, saved)))),
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
fn spawn_tuner(shared: Arc<Shared>, xtal_hz: f64, i2c_addr: u16) -> mpsc::Sender<f64> {
    let (tx, rx) = mpsc::channel::<f64>();
    std::thread::spawn(move || {
        let mut si = match si570::Si570::open(xtal_hz, i2c_addr) {
            Ok(mut s) => {
                let ver = s.version().unwrap_or_else(|_| "?".into());
                *shared.hw_status.lock().unwrap() = format!("SoftRock fw {ver}");
                s
            }
            Err(e) => {
                *shared.hw_status.lock().unwrap() = format!("{e}");
                return;
            }
        };
        for freq in rx {
            if let Err(e) = si.set_freq(freq) {
                *shared.hw_status.lock().unwrap() = format!("ladění selhalo: {e}");
            }
        }
    });
    tx
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

/// To z nastavení, co se čte jen při startu vláken. Změna se projeví
/// až po restartu, tak ať to umíme uživateli říct.
#[derive(PartialEq, Clone)]
struct StartupConfig {
    capture_device: String,
    playback_device: String,
    depth: audio::Depth,
    si570_xtal_hz: f64,
    si570_i2c_addr: u16,
}

impl StartupConfig {
    fn of(s: &Settings) -> Self {
        StartupConfig {
            capture_device: s.capture_device.clone(),
            playback_device: s.playback_device.clone(),
            depth: s.depth,
            si570_xtal_hz: s.si570_xtal_hz,
            si570_i2c_addr: s.si570_i2c_addr,
        }
    }
}

struct App {
    shared: Arc<Shared>,
    tuner: mpsc::Sender<f64>,
    /// Vše, co se ukládá, drží rovnou Settings - jediný zdroj pravdy.
    set: Settings,
    vfo_input: String,
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
}

impl App {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        tuner: mpsc::Sender<f64>,
        s: Settings,
    ) -> Self {
        // Naladit tam, kde uživatel posledně skončil.
        let _ = tuner.send(s.vfo_khz * 1000.0);
        App {
            shared,
            tuner,
            set: s.clone(),
            vfo_input: format!("{:.1}", s.vfo_khz),
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
        }
    }

    fn bandwidth_hz(&self) -> f64 {
        self.set.bandwidth()
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
            dsp::Mode::Am | dsp::Mode::Cw => {
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
            dsp::Mode::Am | dsp::Mode::Cw => vec![lo, hi],
            dsp::Mode::Usb => vec![hi],
            dsp::Mode::Lsb => vec![lo],
        }
    }

    fn set_vfo(&mut self, khz: f64) {
        self.set.vfo_khz = khz.clamp(100.0, 60_000.0);
        self.vfo_input = format!("{:.1}", self.set.vfo_khz);
        let _ = self.tuner.send(self.set.vfo_khz * 1000.0);
    }

    /// Viditelný výřez panoramatu jako (střed v Hz od VFO, šířka v Hz).
    ///
    /// Výřez sleduje naladěnou stanici, ale nikdy nevyjede ze zachyceného
    /// spektra - u kraje se prostě zastaví.
    fn view(&self, span_hz: f64) -> (f64, f64) {
        view_window(self.set.zoom, self.set.offset_hz, span_hz)
    }

    fn set_zoom(&mut self, z: f32) {
        self.set.zoom = z.clamp(1.0, MAX_ZOOM);
    }

    /// Krok jemného ladění kolečkem a šipkami. Na hrubé skoky je Shift
    /// (desetinásobek) a tlačítka VFO.
    fn tune_step_hz(&self) -> f64 {
        if self.set.mode.is_ssb() {
            10.0
        } else {
            100.0
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
    }

    /// Krok VFO. Okno se posune do strany, ale zůstaneme naladění na stejné
    /// stanici - jinak by každý krok naladění shodil.
    fn step_vfo(&mut self, delta_khz: f64, span_hz: f64) {
        let before = self.set.vfo_khz;
        self.set_vfo(self.set.vfo_khz + delta_khz);
        // set_vfo si krok mohl zkrátit o meze rozsahu, tak počítáme se skutečným.
        let applied = self.set.vfo_khz - before;
        self.set.offset_hz = offset_after_vfo_step(self.set.offset_hz, applied, span_hz);
    }

    /// Posun VFO o celou šířku okna - ukáže kus pásma, na který odsud
    /// nevidíme. Naladění tím ztrácí smysl, tak se pak doladí samo.
    fn jump_window(&mut self, span_hz: f64, dir: f64) {
        self.set_vfo(self.set.vfo_khz + dir * span_hz / 1000.0);
        self.set.offset_hz = 0.0;
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
        // Rozhlas je AM, amatérská pásma pod 10 MHz LSB, nad ním USB.
        let mode = if band.is_broadcast() {
            dsp::Mode::Am
        } else if band.from_khz < 10_000.0 {
            dsp::Mode::Lsb
        } else {
            dsp::Mode::Usb
        };
        let bw = match mode {
            dsp::Mode::Cw => radio::CW_BANDWIDTH_HZ,
            dsp::Mode::Usb | dsp::Mode::Lsb => radio::SSB_BANDWIDTH_HZ,
            dsp::Mode::Am => radio::AM_BANDWIDTH_HZ,
        };
        self.tune_to(band.middle_khz(), mode, bw);
    }

    /// Naladí konkrétní frekvenci i s režimem a šířkou. VFO se posadí tak,
    /// aby stanice padla mimo mrtvou zónu kolem DC.
    fn tune_to(&mut self, freq_khz: f64, mode: dsp::Mode, bandwidth_hz: f64) {
        self.set.mode = mode;
        self.set_bandwidth_hz(bandwidth_hz);
        self.set_vfo(freq_khz - PARK_OFFSET_HZ / 1000.0);
        self.set.offset_hz = PARK_OFFSET_HZ;
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
                                .text("squelch [dB]")
                                .fixed_decimals(0),
                        )
                        .on_hover_text(
                            "o kolik musí signál vyčnívat nad šum, aby se dekódoval\n\
                             níž = citlivější, ale víc nesmyslů ze šumu",
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

        ui.horizontal_wrapped(|ui| {
            ui.label("pásma:");
            for b in &bands {
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
                let tip = if known {
                    format!("zpět tam, kde jsi na {} naposled byl", b.name)
                } else {
                    format!("{} - doprostřed ({:.0} kHz)", b.name, b.middle_khz())
                };
                if ui.selectable_label(active, text).on_hover_text(tip).clicked() {
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
        egui::Window::new("Nastavení")
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                let devices = self
                    .devices
                    .get_or_insert_with(Devices::enumerate);

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
                        ui.label("vstup (I/Q):");
                        ui.horizontal(|ui| {
                            Self::device_picker(
                                ui,
                                "vstup",
                                &mut self.set.capture_device,
                                &devices.capture,
                            );
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
                        });
                        ui.end_row();

                        ui.label("hloubka:");
                        ui.horizontal(|ui| {
                            for d in audio::Depth::ALL {
                                ui.selectable_value(&mut self.set.depth, d, d.label());
                            }
                            ui.label(
                                egui::RichText::new(format!("→ {}", self.set.depth.hint()))
                                    .weak(),
                            );
                        });
                        ui.end_row();
                    });
                ui.label(
                    egui::RichText::new(
                        "24 bit umí jen ALSA na Linuxu. Přes WASAPI a CoreAudio \
                         o formátu rozhoduje zvukový server, tam automatika cílí na 16 bit.",
                    )
                    .weak()
                    .small(),
                );

                if ui.button("↻ znovu prohledat zařízení").clicked() {
                    self.devices = None;
                }

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
                        "Krystal je kalibrace kus od kusu - špatná hodnota posune celou stupnici.",
                    )
                    .weak()
                    .small(),
                );

                ui.add_space(8.0);
                ui.separator();
                if StartupConfig::of(&self.set) != self.startup {
                    ui.label(
                        egui::RichText::new(
                            "⚠ Zvuk i Si570 se čtou při startu - změny se projeví \
                             až po restartu programu.",
                        )
                        .color(egui::Color32::from_rgb(230, 180, 80)),
                    );
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
                                        .range(100.0..=60_000.0)
                                        .fixed_decimals(1),
                                );
                                egui::ComboBox::from_id_salt(("rezim", i))
                                    .selected_text(st.mode.label())
                                    .width(60.0)
                                    .show_ui(ui, |ui| {
                                        for m in [dsp::Mode::Am, dsp::Mode::Usb, dsp::Mode::Lsb, dsp::Mode::Cw] {
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
                        dsp::Mode::Am | dsp::Mode::Cw => d.abs() * 2.0,
                        dsp::Mode::Usb => d,
                        dsp::Mode::Lsb => -d,
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

/// Úroveň v dB, nad kterou signál otevře CW squelch - k zakreslení do spektra.
///
/// Není to prosté „šum + squelch". Dekodér počítá odstup v šířce svého
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
fn view_window(zoom: f32, offset_hz: f64, span_hz: f64) -> (f64, f64) {
    let zoom = zoom.clamp(1.0, MAX_ZOOM) as f64;
    let vis = span_hz / zoom;
    let limit = (span_hz - vis) / 2.0;
    (offset_hz.clamp(-limit, limit), vis)
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

        let tuned_khz = self.set.vfo_khz + self.set.offset_hz / 1000.0;

        egui::Panel::top("ovladani").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("VFO [kHz]:");
                let resp =
                    ui.add(egui::TextEdit::singleline(&mut self.vfo_input).desired_width(90.0));
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(v) = self.vfo_input.trim().replace(',', ".").parse::<f64>() {
                        self.set_vfo(v);
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
                for d in [-10.0, -1.0, 1.0, 10.0] {
                    if ui
                        .button(format!("{d:+.0} k"))
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
                ui.label(
                    egui::RichText::new(format!("naladěno {tuned_khz:.2} kHz"))
                        .size(18.0)
                        .strong(),
                );
                // V jakém úseku pásma zrovna jsme.
                if let Some(s) = bandplan::at(tuned_khz) {
                    let (r, g, b) = s.usage.color();
                    ui.label(
                        egui::RichText::new(format!("{} · {}", s.band, s.usage.label()))
                            .color(egui::Color32::from_rgb(r, g, b)),
                    );
                }
                ui.separator();
                for m in [dsp::Mode::Am, dsp::Mode::Usb, dsp::Mode::Lsb, dsp::Mode::Cw] {
                    ui.selectable_value(&mut self.set.mode, m, m.label());
                }
                ui.separator();
                self.s_meter(ui);
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
            ui.horizontal(|ui| {
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
                let (bw_min, bw_max) = radio::bandwidth_range(self.set.mode);
                let mut bw_khz = self.bandwidth_hz() / 1000.0;
                if ui
                    .add(
                        egui::Slider::new(&mut bw_khz, bw_min / 1000.0..=bw_max / 1000.0)
                            .text("šířka [kHz]")
                            .fixed_decimals(1),
                    )
                    .changed()
                {
                    self.set_bandwidth_hz(bw_khz * 1000.0);
                }
                ui.separator();
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
            self.band_buttons(ui, tuned_khz);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("stav").show(ui, |ui| {
            let status = self.shared.status.lock().unwrap().clone();
            let hw = self.shared.hw_status.lock().unwrap().clone();
            ui.horizontal(|ui| {
                ui.label(status);
                ui.separator();
                ui.label(hw);
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

            // Squelch CW: nad touhle čárou signál dekodér otevře.
            // Jen když squelch opravdu něco dělá.
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
                        "squelch",
                        egui::FontId::proportional(9.0),
                        egui::Color32::from_rgb(255, 210, 60),
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
