//! KnoflikSDR - přijímač pro SoftRock.
//!
//! I/Q ze zvukové karty, ladění Si570 přes USB, panorama + vodopád,
//! režimy AM/USB/LSB a oblíbené stanice.

mod audio;
mod dsp;
mod radio;
mod settings;
mod si570;

use settings::{Autosave, Settings, Station};

use eframe::egui;
use radio::{Shared, FFT_SIZE};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

/// Zvuková karta s I/Q ze SoftRocku (z ~/.quisk_conf.py).
const CAPTURE_DEVICE: &str = "hw:CARD=HD,DEV=0";
/// Kalibrovaný krystal Si570.
const SI570_XTAL: f64 = 114_269_790.0;
const SI570_I2C_ADDR: u16 = 0x55;

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

fn main() -> eframe::Result {
    let shared = Arc::new(Shared::new());

    // Audio ring: ~0.5 s rezerva na 48 kHz.
    let (audio_tx, audio_rx) = rtrb::RingBuffer::<f32>::new(24_000);

    audio::spawn(audio_rx, radio::AUDIO_RATE as u32, shared.running.clone());
    radio::spawn(CAPTURE_DEVICE.to_string(), shared.clone(), audio_tx);
    let tuner = spawn_tuner(shared.clone());

    // Diagnostika bez GUI: ukáže, co si capture vyjednal a jestli teče signál.
    if std::env::args().any(|a| a == "--probe") {
        probe(&shared);
        shared.running.store(false, Ordering::Relaxed);
        return Ok(());
    }

    // Nastavení potřebujeme už tady, abychom okno otevřeli v poslední velikosti.
    let saved = Settings::load();
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

/// Ladicí vlákno. USB control transfer trvá jednotky ms, takže nesmí
/// běžet v GUI ani v audio cestě.
fn spawn_tuner(shared: Arc<Shared>) -> mpsc::Sender<f64> {
    let (tx, rx) = mpsc::channel::<f64>();
    std::thread::spawn(move || {
        let mut si = match si570::Si570::open(SI570_XTAL, SI570_I2C_ADDR) {
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
    /// Po skoku za roh se má doladit na nejsilnější stanici, ale až se
    /// panorama ustálí - proto až po tomto čase.
    snap_at: Option<std::time::Instant>,
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
            snap_at: None,
            wf_pixels: vec![0; FFT_SIZE * WF_HEIGHT * 4],
            wf_tex: None,
            last_generation: 0,
            autosave: Autosave::new(s),
        }
    }

    fn bandwidth_hz(&self) -> f64 {
        self.set.bandwidth()
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
            dsp::Mode::Am => (self.set.offset_hz - bw / 2.0, self.set.offset_hz + bw / 2.0),
            dsp::Mode::Usb => (self.set.offset_hz, self.set.offset_hz + bw),
            dsp::Mode::Lsb => (self.set.offset_hz - bw, self.set.offset_hz),
        }
    }

    /// Které hrany jde chytit a táhnout. U SSB je vnitřní hrana zároveň
    /// nosná, takže tažení nechává jen tu vnější.
    fn draggable_edges(&self) -> Vec<f64> {
        let (lo, hi) = self.band_edges();
        match self.set.mode {
            dsp::Mode::Am => vec![lo, hi],
            dsp::Mode::Usb => vec![hi],
            dsp::Mode::Lsb => vec![lo],
        }
    }

    fn set_vfo(&mut self, khz: f64) {
        self.set.vfo_khz = khz.clamp(100.0, 60_000.0);
        self.vfo_input = format!("{:.1}", self.set.vfo_khz);
        let _ = self.tuner.send(self.set.vfo_khz * 1000.0);
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

    /// Naladí oblíbenou stanici i s jejím režimem a šířkou pásma.
    /// VFO se posadí tak, aby stanice padla mimo mrtvou zónu kolem DC.
    fn tune_station(&mut self, st: &Station) {
        self.set.mode = st.mode;
        self.set_bandwidth_hz(st.bandwidth_hz);
        self.set_vfo(st.freq_khz - PARK_OFFSET_HZ / 1000.0);
        self.set.offset_hz = PARK_OFFSET_HZ;
        self.snap_at = None; // ruční volba má přednost před hledáním
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
                                        for m in [dsp::Mode::Am, dsp::Mode::Usb, dsp::Mode::Lsb] {
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

        let hz_of_x = |x: f32| ((x - rect.center().x) / rect.width()) as f64 * span_hz;
        let x_of_hz = |hz: f64| rect.center().x + (hz / span_hz) as f32 * rect.width();

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
                        dsp::Mode::Am => d.abs() * 2.0,
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
                ui.separator();
                for m in [dsp::Mode::Am, dsp::Mode::Usb, dsp::Mode::Lsb] {
                    ui.selectable_value(&mut self.set.mode, m, m.label());
                }
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
                ui.label("dB rozsah:");
                ui.add(egui::Slider::new(&mut self.set.db_min, -140.0..=-40.0).text("min"));
                ui.add(egui::Slider::new(&mut self.set.db_max, -60.0..=0.0).text("max"));
            });
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

        egui::CentralPanel::default().show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let spec_h = full.height() * 0.35;

            // Převod frekvenčního offsetu na x a zpět - společné pro obě plochy.
            let x_of = |rect: &egui::Rect, hz: f64| -> f32 {
                rect.center().x + (hz / span_hz) as f32 * rect.width()
            };
            let (band_lo, band_hi) = self.band_edges();

            // --- Panorama ---
            let (resp, painter) = ui.allocate_painter(
                egui::vec2(full.width(), spec_h),
                egui::Sense::click_and_drag(),
            );
            let rect = resp.rect;
            painter.rect_filled(rect, 0.0, egui::Color32::from_gray(16));

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

            // Svislá mřížka po kHz, popisky v absolutní frekvenci
            let khz_step = nice_khz_step(span_hz / 1000.0);
            let half_khz = span_hz / 2000.0;
            let mut k = -(half_khz / khz_step).floor() * khz_step;
            let mut grid_lines: Vec<(f32, f64)> = Vec::new();
            while k <= half_khz {
                let x = x_of(&rect, k * 1000.0);
                grid_lines.push((x, self.set.vfo_khz + k));
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

            let n = bins.len().max(2);
            let pts: Vec<egui::Pos2> = bins
                .iter()
                .enumerate()
                .map(|(i, &db)| {
                    let x = rect.left() + rect.width() * i as f32 / (n - 1) as f32;
                    let t = ((db - self.set.db_min) / (self.set.db_max - self.set.db_min)).clamp(0.0, 1.0);
                    let y = rect.bottom() - rect.height() * t;
                    egui::pos2(x, y)
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
            painter.text(
                egui::pos2(x_of(&rect, 0.0) + 3.0, rect.top() + 2.0),
                egui::Align2::LEFT_TOP,
                "VFO",
                egui::FontId::proportional(10.0),
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
                let wf_resp = ui.add(
                    egui::Image::new(tex)
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

        self.push_controls();
        self.autosave.tick(self.set.clone());
    }

    fn on_exit(&mut self) {
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
