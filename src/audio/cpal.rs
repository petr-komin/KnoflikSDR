//! Zvuk přes cpal - WASAPI na Windows, CoreAudio na macOS.
//!
//! Na rozdíl od ALSA je cpal postavený na callbacích: zvukový server si říká
//! o data sám a nečeká na nás. Mezi callback a zbytek programu proto dáváme
//! ring buffer a [`Capture::read`] / [`Playback::write`] blokují na něm.
//! Blokování na plném výstupním ringu zároveň udává tempo, stejně jako
//! `writei` u ALSA.
//!
//! Packed 24 bit se tudy spolehlivě dostat nedá - o formát se dohaduje
//! zvukový server, takže [`super::Depth::Auto`] tu cílí na 16 bit.

use super::{Capture, DeviceInfo, Negotiated, Playback};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedStreamConfig,
};
use std::time::{Duration, Instant};

pub const NAME: &str = if cfg!(windows) { "WASAPI" } else { "CoreAudio" };

/// Prázdný název = výchozí zařízení systému. Konkrétní karty se adresují
/// jménem z [`DeviceTrait::description`].
pub const DEFAULT_CAPTURE: &str = "";
pub const DEFAULT_PLAYBACK: &str = "";

/// Vzorkovačky od nejlepší. Vyšší = širší panorama.
const RATES: [u32; 3] = [192_000, 96_000, 48_000];

/// Jak dlouho čekat na vzorky, než to prohlásíme za rozbité. Když se stream
/// sesype (uživatel vytáhne kartu), callback přestane chodit a bez tohohle
/// bychom se zacyklili napořád.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Jak dlouho spát, když ring zrovna nemá co dát/vzít.
const IDLE: Duration = Duration::from_millis(1);

fn label_for_default(input: bool) -> &'static str {
    if input {
        "(výchozí vstup)"
    } else {
        "(výchozí výstup)"
    }
}

fn list(input: bool) -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let mut out = vec![DeviceInfo {
        id: String::new(),
        label: label_for_default(input).to_string(),
    }];
    let devices = if input {
        host.input_devices().map(|d| d.collect::<Vec<_>>())
    } else {
        host.output_devices().map(|d| d.collect::<Vec<_>>())
    };
    let Ok(devices) = devices else {
        return out;
    };
    for d in devices {
        if let Ok(desc) = d.description() {
            let name = desc.name().to_string();
            out.push(DeviceInfo {
                id: name.clone(),
                label: name,
            });
        }
    }
    out
}

pub fn list_capture() -> Vec<DeviceInfo> {
    list(true)
}

pub fn list_playback() -> Vec<DeviceInfo> {
    list(false)
}

/// Najde zařízení podle jména; prázdné jméno = výchozí.
fn find(device: &str, input: bool) -> Result<Device> {
    let host = cpal::default_host();
    if device.is_empty() {
        return if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        }
        .ok_or_else(|| anyhow!("systém nehlásí žádné výchozí zvukové zařízení"));
    }
    let devices = if input {
        host.input_devices()?.collect::<Vec<_>>()
    } else {
        host.output_devices()?.collect::<Vec<_>>()
    };
    devices
        .into_iter()
        .find(|d| d.description().is_ok_and(|desc| desc.name() == device))
        .ok_or_else(|| anyhow!("zvukové zařízení '{device}' nenalezeno"))
}

/// Formáty od nejlepšího, oříznuté stropem hloubky.
///
/// F32 necháváme v obou větvích schválně: WASAPI ve sdíleném režimu často
/// jiný formát ani nenabídne, takže odmítnout ho kvůli stropu by znamenalo
/// žádný zvuk. Míchá se do něj stejně až za kartou, takže o vzorky nepřijdeme.
fn format_prefs(max_bits: u32) -> &'static [SampleFormat] {
    if max_bits >= 24 {
        &[
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::F32,
            SampleFormat::I16,
        ]
    } else {
        &[SampleFormat::I16, SampleFormat::F32]
    }
}

/// Vybere nejlepší kombinaci rychlosti a formátu, kterou karta umí.
/// Když se netrefíme do vlastního seznamu, vezmeme výchozí konfiguraci
/// od cpalu - lepší něco než nic.
fn pick_input_config(dev: &Device, max_bits: u32) -> Result<SupportedStreamConfig> {
    let supported: Vec<_> = dev.supported_input_configs()?.collect();
    for &rate in &RATES {
        for &want in format_prefs(max_bits) {
            for r in &supported {
                if r.channels() != 2 || r.sample_format() != want {
                    continue;
                }
                if let Some(c) = r.clone().try_with_sample_rate(rate) {
                    return Ok(c);
                }
            }
        }
    }
    let def = dev.default_input_config()?;
    if def.channels() != 2 {
        return Err(anyhow!(
            "zvukovka nenabízí stereo (I/Q potřebuje dva kanály), hlásí {} kanálů",
            def.channels()
        ));
    }
    Ok(def)
}

pub struct CpalCapture {
    // Stream musí žít, jinak se zavře a callback přestane chodit.
    _stream: Stream,
    rx: rtrb::Consumer<f32>,
    negotiated: Negotiated,
}

/// Postaví vstupní stream pro konkrétní typ vzorku a sype ho do ringu.
fn build_input<T>(
    dev: &Device,
    cfg: &StreamConfig,
    mut tx: rtrb::Producer<f32>,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    dev.build_input_stream::<T, _, _>(
        cfg.clone(),
        move |data: &[T], _| {
            for &s in data {
                // Když ring přeteče, vzorek zahodíme - DSP je pozadu
                // a dohnat to stejně nejde.
                let _ = tx.push(f32::from_sample(s));
            }
        },
        |e| eprintln!("chyba zvukového vstupu: {e}"),
        None,
    )
}

pub fn open_capture(device: &str, max_bits: u32) -> Result<Box<dyn Capture>> {
    let dev = find(device, true)?;
    let supported = pick_input_config(&dev, max_bits)?;
    let fmt = supported.sample_format();
    let rate = supported.sample_rate();
    let cfg: StreamConfig = supported.into();

    // ~0.5 s rezervy, ať callback nezahazuje při zadrhnutí DSP vlákna.
    let (tx, rx) = rtrb::RingBuffer::<f32>::new(rate as usize);
    let stream = match fmt {
        SampleFormat::I16 => build_input::<i16>(&dev, &cfg, tx),
        SampleFormat::I24 => build_input::<cpal::I24>(&dev, &cfg, tx),
        SampleFormat::I32 => build_input::<i32>(&dev, &cfg, tx),
        SampleFormat::F32 => build_input::<f32>(&dev, &cfg, tx),
        other => return Err(anyhow!("nepodporovaný formát vstupu: {other:?}")),
    }?;
    stream.play()?;

    Ok(Box::new(CpalCapture {
        _stream: stream,
        rx,
        negotiated: Negotiated {
            rate,
            bits: fmt.bits_per_sample(),
        },
    }))
}

impl Capture for CpalCapture {
    fn negotiated(&self) -> Negotiated {
        self.negotiated
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            // Jen celé rámce, ať se I a Q nerozjedou.
            let frames = (self.rx.slots().min(out.len())) / 2;
            if frames > 0 {
                for slot in out.iter_mut().take(frames * 2) {
                    *slot = self.rx.pop().unwrap_or(0.0);
                }
                return Ok(frames);
            }
            if Instant::now() >= deadline {
                return Err(anyhow!("ze zvukového vstupu {READ_TIMEOUT:?} nic nepřišlo"));
            }
            std::thread::sleep(IDLE);
        }
    }
}

pub struct CpalPlayback {
    _stream: Stream,
    tx: rtrb::Producer<f32>,
}

pub fn open_playback(device: &str, rate: u32) -> Result<Box<dyn Playback>> {
    let dev = find(device, false)?;
    let supported = dev
        .supported_output_configs()?
        .find(|r| {
            r.channels() == 2
                && r.min_sample_rate() <= rate
                && r.max_sample_rate() >= rate
                && r.sample_format() == SampleFormat::F32
        })
        .map(|r| r.with_sample_rate(rate))
        .map_or_else(|| dev.default_output_config(), Ok)?;

    if supported.channels() != 2 {
        return Err(anyhow!(
            "výstup nenabízí stereo, hlásí {} kanálů",
            supported.channels()
        ));
    }
    let fmt = supported.sample_format();
    let cfg: StreamConfig = supported.into();

    let (tx, mut rx) = rtrb::RingBuffer::<f32>::new(super::CHUNK * 8 * 2);
    let stream = match fmt {
        SampleFormat::F32 => dev.build_output_stream::<f32, _, _>(
            cfg,
            move |data: &mut [f32], _| {
                for s in data.iter_mut() {
                    // Prázdný ring = ticho.
                    *s = rx.pop().unwrap_or(0.0);
                }
            },
            |e| eprintln!("chyba zvukového výstupu: {e}"),
            None,
        ),
        SampleFormat::I16 => dev.build_output_stream::<i16, _, _>(
            cfg,
            move |data: &mut [i16], _| {
                for s in data.iter_mut() {
                    *s = i16::from_sample(rx.pop().unwrap_or(0.0));
                }
            },
            |e| eprintln!("chyba zvukového výstupu: {e}"),
            None,
        ),
        other => return Err(anyhow!("nepodporovaný formát výstupu: {other:?}")),
    }?;
    stream.play()?;

    Ok(Box::new(CpalPlayback {
        _stream: stream,
        tx,
    }))
}

impl Playback for CpalPlayback {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        let mut i = 0;
        while i < samples.len() {
            match self.tx.push(samples[i]) {
                Ok(()) => i += 1,
                // Plný ring znamená, že karta ještě nestihla odebrat -
                // počkat je přesně to, co chceme: drží nám to tempo.
                Err(_) => std::thread::sleep(IDLE),
            }
        }
        Ok(())
    }
}
