//! Nahrávání demodulovaného zvuku do WAV.
//!
//! Zapisuje se 16bit PCM mono - to otevře cokoli a na řeč z éteru je to
//! bohatě dost. Odbočka sedí v DSP vlákně **před** regulací hlasitosti, takže
//! nahrávka nezávisí na tom, jak máš zrovna stažený knoflík.
//!
//! Hlavička WAV nese délku dat, kterou při otevírání souboru ještě neznáme.
//! Píše se proto nejdřív s nulami a při zavření se do ní vrátíme a doplníme ji.

use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Velikost hlavičky WAV (RIFF + fmt + data), za kterou začínají vzorky.
const HEADER_LEN: u32 = 44;

pub struct WavWriter {
    out: BufWriter<File>,
    path: PathBuf,
    /// Kolik vzorků už je v souboru - pro délku nahrávky i pro hlavičku.
    frames: u32,
    sample_rate: u32,
}

impl WavWriter {
    /// Založí soubor a zapíše hlavičku s nulovými délkami.
    pub fn create(path: impl AsRef<Path>, sample_rate: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("nejde vyrobit adresář {}", dir.display()))?;
        }
        let file = File::create(&path)
            .with_context(|| format!("nejde vyrobit soubor {}", path.display()))?;
        let mut w = WavWriter {
            out: BufWriter::new(file),
            path,
            frames: 0,
            sample_rate,
        };
        w.write_header()?;
        Ok(w)
    }

    /// Hlavička pro 16bit PCM mono. Délky se počítají z `frames`, takže
    /// stejná funkce poslouží i při zavírání souboru.
    fn write_header(&mut self) -> Result<()> {
        const BITS: u16 = 16;
        const CHANNELS: u16 = 1;
        let data_len = self.frames * (BITS / 8) as u32 * CHANNELS as u32;
        let byte_rate = self.sample_rate * (BITS / 8) as u32 * CHANNELS as u32;

        self.out.write_all(b"RIFF")?;
        // Velikost celého souboru bez prvních osmi bajtů.
        self.out.write_all(&(HEADER_LEN - 8 + data_len).to_le_bytes())?;
        self.out.write_all(b"WAVE")?;

        self.out.write_all(b"fmt ")?;
        self.out.write_all(&16u32.to_le_bytes())?; // délka fmt bloku
        self.out.write_all(&1u16.to_le_bytes())?; // 1 = nekomprimované PCM
        self.out.write_all(&CHANNELS.to_le_bytes())?;
        self.out.write_all(&self.sample_rate.to_le_bytes())?;
        self.out.write_all(&byte_rate.to_le_bytes())?;
        self.out.write_all(&(CHANNELS * BITS / 8).to_le_bytes())?; // zarovnání bloku
        self.out.write_all(&BITS.to_le_bytes())?;

        self.out.write_all(b"data")?;
        self.out.write_all(&data_len.to_le_bytes())?;
        Ok(())
    }

    /// Přidá blok vzorků. Vstup je -1..1, ukládá se jako i16.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            self.out.write_all(&v.to_le_bytes())?;
        }
        self.frames = self.frames.saturating_add(samples.len() as u32);
        Ok(())
    }

    /// Délka nahrávky v sekundách - pro ukazatel v GUI.
    pub fn seconds(&self) -> f32 {
        self.frames as f32 / self.sample_rate.max(1) as f32
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Doplní do hlavičky skutečné délky a soubor zavře.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.out.flush()?;
        self.out.seek(SeekFrom::Start(0))?;
        self.write_header()?;
        self.out.flush()?;
        Ok(self.path)
    }
}

/// Kam ukládat nahrávky: `~/Nahrávky/KnoflikSDR`, případně vedle configu,
/// když domovský adresář není k mání.
pub fn default_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        return home.join("Nahrávky").join("KnoflikSDR");
    }
    PathBuf::from("nahravky")
}

/// Jméno souboru z času a naladěné frekvence, ať jde nahrávka poznat.
pub fn file_name(tuned_khz: f64, mode: &str) -> String {
    let now = chrono::Local::now();
    format!(
        "{}_{:.0}kHz_{}.wav",
        now.format("%Y-%m-%d_%H-%M-%S"),
        tuned_khz,
        mode
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zapsaný soubor musí mít správnou hlavičku i délku dat - jinak ho
    /// přehrávač buď odmítne, nebo utne konec.
    #[test]
    fn wav_ma_spravnou_hlavicku_a_delku() {
        let dir = std::env::temp_dir().join("knoflik_wav_test");
        let path = dir.join("t.wav");
        let _ = std::fs::remove_file(&path);

        let mut w = WavWriter::create(&path, 48_000).unwrap();
        // 480 vzorků = 10 ms.
        let samples: Vec<f32> = (0..480).map(|i| (i as f32 / 480.0) - 0.5).collect();
        w.write(&samples).unwrap();
        assert!((w.seconds() - 0.01).abs() < 1e-6, "délka {} s", w.seconds());
        let path = w.finish().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(&data[36..40], b"data");

        // Datový blok musí sedět na počet vzorků krát dva bajty.
        let data_len = u32::from_le_bytes(data[40..44].try_into().unwrap());
        assert_eq!(data_len, 480 * 2, "délka dat v hlavičce");
        assert_eq!(data.len() as u32, HEADER_LEN + data_len, "délka souboru");

        // A RIFF délka musí odpovídat zbytku souboru.
        let riff_len = u32::from_le_bytes(data[4..8].try_into().unwrap());
        assert_eq!(riff_len, data.len() as u32 - 8);

        let _ = std::fs::remove_file(&path);
    }

    /// Prázdná nahrávka nesmí vyrobit rozbitý soubor.
    #[test]
    fn prazdna_nahravka_je_platny_wav() {
        let dir = std::env::temp_dir().join("knoflik_wav_test");
        let path = dir.join("prazdny.wav");
        let _ = std::fs::remove_file(&path);

        let w = WavWriter::create(&path, 48_000).unwrap();
        let path = w.finish().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len() as u32, HEADER_LEN);
        assert_eq!(u32::from_le_bytes(data[40..44].try_into().unwrap()), 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Jméno souboru musí nést frekvenci i režim, ať se nahrávky nepletou.
    ///
    /// Schválně ne na půl kilohertzu: `{:.0}` zaokrouhluje half-to-even, takže
    /// 7130,5 dá 7130 a test by kontroloval spíš pravidla formátování než to,
    /// co nás zajímá.
    #[test]
    fn jmeno_souboru_nese_frekvenci_a_rezim() {
        let n = file_name(7130.7, "LSB");
        assert!(n.ends_with("_7131kHz_LSB.wav"), "jméno: {n}");
        assert!(n.starts_with("20"), "chybí datum: {n}");
        // A jiná frekvence i režim musí dát jiné jméno.
        let m = file_name(99_500.0, "WFM");
        assert!(m.ends_with("_99500kHz_WFM.wav"), "jméno: {m}");
    }
}
