//! SDRplay RSP1 přes SoapySDR (modul `miri` nad libmirisdr).
//!
//! Podrobný průzkum hardwaru je v `docs/sdrplay-rsp1.md`. Dvě věci odtud,
//! které jsou v kódu vidět a vypadaly by jinak jako chyba:
//!
//! 1. **Vzorkovačka 1,344 MSps je schválně.** `get_sample_rate_range()` hlásí
//!    `min=max=8000000`, ale to je lež v metadatech - zařízení bere 1,3-12 MSps.
//!    1 344 000 = 48 000 × 28, takže se na audio decimuje celým číslem.
//!    Na 8 MSps by to jelo na doraz USB (24 z 24,576 MB/s), takhle na 16 %.
//! 2. **Zařízení jde otevřít jen jednou.** Druhé otevření tiše zamrzne, i když
//!    se první handle mezitím zahodí. Proto se `Device` otevírá jednou a do
//!    ladění jde klon, ne nové otevření.

use super::{rsp1_decim, GainRange, Source, Tuner, RSP1_DEFAULT_RATE_HZ, RSP1_RATES_HZ};
use crate::settings::Settings;
use anyhow::{anyhow, Result};
use num_complex::Complex32;
use soapysdr::{Device, Direction, ErrorCode, RxStream};

const ARGS: &str = "driver=miri";
/// Kolik čekat na vzorky. Štědře - přetečení a timeout řešíme jako "zkus znovu".
const READ_TIMEOUT_US: i64 = 500_000;

pub fn open(set: &Settings) -> Result<(Box<dyn Source>, Box<dyn Tuner>)> {
    // Nedůvěřuj slepě configu: jen hodnota z nabídky vyjde na celočíselnou
    // decimaci. Cokoli jiného stáhni na výchozí, ať se nezkreslí stupnice.
    let rate = if RSP1_RATES_HZ.contains(&set.rsp1_rate_hz) {
        set.rsp1_rate_hz
    } else {
        RSP1_DEFAULT_RATE_HZ
    };
    let decim = rsp1_decim(rate);

    let dev = Device::new(ARGS).map_err(|e| anyhow!("RSP1 se nepodařilo otevřít: {e}"))?;

    dev.set_sample_rate(Direction::Rx, 0, rate)
        .map_err(|e| anyhow!("RSP1 nevzal {rate} Sps: {e}"))?;
    let got = dev.sample_rate(Direction::Rx, 0).unwrap_or(rate);
    if (got - rate).abs() > 1.0 {
        return Err(anyhow!(
            "RSP1 drží {got} Sps místo {rate} - decimace {decim}× by nevyšla"
        ));
    }
    let _ = dev.set_gain(Direction::Rx, 0, set.rsp1_gain_db);

    let mut stream = dev
        .rx_stream::<Complex32>(&[0])
        .map_err(|e| anyhow!("RSP1: stream nešel otevřít: {e}"))?;
    stream
        .activate(None)
        .map_err(|e| anyhow!("RSP1: stream nešel spustit: {e}"))?;

    let tuner = Rsp1Tuner {
        // Klon, ne nové otevření - viz poznámka 2 v hlavičce.
        dev: dev.clone(),
        label: label_of(&dev),
    };
    Ok((Box::new(Rsp1Source { stream, rate }), Box::new(tuner)))
}

fn label_of(dev: &Device) -> String {
    match dev.hardware_key() {
        Ok(k) => format!("RSP1 ({k})"),
        Err(_) => "RSP1".to_string(),
    }
}

struct Rsp1Source {
    stream: RxStream<Complex32>,
    rate: f64,
}

impl Source for Rsp1Source {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        match self.stream.read(&mut [out], READ_TIMEOUT_US) {
            Ok(n) => Ok(n),
            // Přetečení znamená, že jsme nestíhali číst; vzorky jsou pryč,
            // ale zařízení jede dál. Timeout totéž. Obojí = zkus znovu.
            Err(e) if e.code == ErrorCode::Overflow || e.code == ErrorCode::Timeout => Ok(0),
            Err(e) => Err(anyhow!("RSP1: čtení selhalo: {e}")),
        }
    }

    fn rate(&self) -> f64 {
        self.rate
    }

    fn label(&self) -> String {
        format!("příjem z RSP1 @ {:.0} kHz (SoapySDR)", self.rate / 1000.0)
    }
}

struct Rsp1Tuner {
    dev: Device,
    label: String,
}

impl Tuner for Rsp1Tuner {
    fn set_center(&mut self, hz: f64) -> Result<()> {
        self.dev
            .set_frequency(Direction::Rx, 0, hz, ())
            .map_err(|e| anyhow!("RSP1: ladění na {hz} Hz selhalo: {e}"))
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn gain_range(&self) -> Option<GainRange> {
        // Přes SoapyMiri je dostupné jen LNA. Kdyby knihovna rozsah nedala,
        // radši zisk neukazujeme, než abychom hádali.
        self.dev
            .gain_range(Direction::Rx, 0)
            .ok()
            .map(|r| GainRange {
                min: r.minimum,
                max: r.maximum,
            })
    }

    fn set_gain(&mut self, db: f64) -> Result<()> {
        self.dev
            .set_gain(Direction::Rx, 0, db)
            .map_err(|e| anyhow!("RSP1: zisk {db} dB nešel nastavit: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Výchozí vzorkovačka musí vycházet na celočíselnou decimaci. Kontrola
    /// všech nabízených hodnot je v `source::tests`, tady jen výchozí.
    #[test]
    fn vychozi_vzorkovacka_deli_beze_zbytku_na_48k() {
        assert_eq!(RSP1_DEFAULT_RATE_HZ / rsp1_decim(RSP1_DEFAULT_RATE_HZ) as f64, 48_000.0);
    }
}
