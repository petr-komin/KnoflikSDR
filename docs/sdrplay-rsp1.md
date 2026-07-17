# SDRplay RSP1 — podklady pro napojení

**Stav:** neimplementováno. Průzkum hotový 16. 7. 2026, přístup k hardwaru ověřen třemi nezávislými cestami.

> Tenhle dokument je **jen o SDRplay RSP1**. Provoz SoftRocku na slabším stroji
> (typicky Raspberry Pi) s tímhle nesouvisí a je popsaný v
> [raspberry-pi.md](raspberry-pi.md) — je to výrazně menší úloha.

Cílem tohoto dokumentu je, aby se příště nemuselo nic zjišťovat znovu. Rozlišuje **ověřeno** (pustil jsem příkaz a viděl výsledek) od **odhad** (rozmyšleno, ale nevyzkoušeno).

---

## 1. Hardware — ověřená fakta

| Vlastnost | Hodnota | Jak zjištěno |
|---|---|---|
| VID:PID | `1df7:2500` | `lsusb` |
| USB | 2.0 high speed, vendor specific class | `lsusb -v` |
| Kernel driver | **žádný navázaný** | sysfs `1-10.1:1.0` → driver ZADNY |
| Přenos | **izochronní**, EP 0x81 IN | `lsusb -v` |
| Paket | `wMaxPacketSize 0x1400` = 3× 1024 B / mikrorámec | `lsusb -v` |
| Strop přenosu | 3072 B × 8 mikrorámců/ms = **24,576 MB/s** | dopočet z paketu |
| Přístupová práva | funguje bez rootu | testy běžely jako `manx` |

Chipset je Mirics **MSi2500** (USB/ADC most) + **MSi001** (tuner). RSP1 je v podstatě referenční design Mirics, proto na něj funguje otevřená `libmirisdr`.

> **Pozor:** RSP1A a RSP2 mají navíc hardware (filtry, bias-T přes GPIO), který libmirisdr neřeší. Tenhle dokument platí **jen pro RSP1**.

---

## 2. Parametry zařízení

Z `SoapySDRUtil --probe="driver=miri"` — **ověřeno**:

```
Channels: 1 Rx, 0 Tx     Timestamps: NO     Supports AGC: NO
Stream formats: CF32     Native format: CF32 [full-scale=1]
Antennas: RX
Full gain range: [0, 10.2, 0.1] dB   (jen LNA)
Full freq range: [0.15, 30], [64, 108], [162, 240], [470, 960], [1450, 1675] MHz
Sample rates: 8 MSps     (jiná se nenabízí)
```

Dvě věci stojí za zapamatování:

- **CF32 nativně** — knihovna nám dá rovnou `Complex<f32>`, přesně to, co náš DSP už žere. Odpadá rozbalování 12bitových vzorků.
- **HF rozsah 0,15–30 MHz** pokrývá všechno, co umí SoftRock, a víc. Mezery mezi pásmy jsou daností tuneru MSi001.

---

## 3. Cesty k zařízení — tři, všechny ověřené

### (a) libmirisdr4 + SoapySDR — **otevřená, pro KnoflikSDR tahle**

```
libmirisdr4 2.0.0-4                       (/usr/lib/x86_64-linux-gnu/libmirisdr.so.4)
soapysdr0.8-module-mirisdr 0.8.1-5        (libmiriSupport.so)
libsoapysdr-dev 0.8.1-5                   (hlavičky v /usr/include/SoapySDR/)
```

Ověřeno:
```bash
SoapySDRUtil --find="driver=miri"    # → Found device 0, label = Mirics MSi2500
SoapySDRUtil --probe="driver=miri"   # → otevřelo a streamovalo
```

### (b) gr-osmosdr built-in `miri` source — otevřená

Debianí `gr-osmosdr 0.2.6` má zakompilované backendy:
`airspy bladerf hackrf miri mirisdr rtl soapy uhd` — **bez `sdrplay`** (Debian uzavřenou knihovnu balit nemůže).

Ověřeno streamem 2 000 000 vzorků (viz §5).

### (c) Proprietární SDRplay API v2 — **uzavřená, tuhle nechceme**

`/opt/gqrx-sdr-2.11.5-linux-x64/lib/libmirsdrapi-rsp.so.2.11` — stripovaná binárka z 25. 3. 2018,
exportuje `mir_sdr_AgcControl`, `mir_sdr_ApiVersion`, `mir_sdr_DCoffsetIQimbalanceControl` atd.

Tohle používá **stará gqrx 2.11.5**, která jako jediná uživateli fungovala. V systému jinde uzavřené
SDRplay API **není** a není potřeba.

> Pozn. k hledání: soubor se jmenuje `libmirsdrapi-rsp.so`, takže ho **nenajdou** vzory `*mir_sdr*`
> ani `*sdrplay*`. Na tomhle jsem si jednou naběhl a prohlásil, že knihovna neexistuje.

**Pro KnoflikSDR → cesta (a) přes crate [`soapysdr`](https://crates.io/crates/soapysdr).** Stejná námaha jako
přímé FFI na libmirisdr, ale zadarmo přibude i RTL-SDR, HackRF, Airspy a další — moduly na ně jsou
v systému už teď (`soapysdr0.8-module-all`).

---

## 4. Situace kolem gqrx (vyřešeno)

| | stará 2.11.5 (`/opt`) | Debianí 2.17.6 |
|---|---|---|
| gr-osmosdr backendy | `sdrplay`, `soapy` (**bez miri**) | `miri`, `mirisdr`, `soapy`, … (**bez sdrplay**) |
| jak sahá na RSP1 | přibalené uzavřené API 2.11 | musí přes `miri` |
| fungovalo | ano | ne |

**Příčina:** `~/.config/gqrx/default.conf` má sekci `[input]` **úplně prázdnou** — bez `device=`.
Autodetekce v gqrx RSP1 nenabídne, takže nová verze nemá co otevřít.

**Řešení — ověřeno**, gqrx 2.17.6 s tímhle configem zařízení otevřela a streamovala
(`Using device #0: Mirics MSi2500`, 40 řádků streamovacích hlášek, žádná chyba):

```ini
[input]
device="miri=0"
sample_rate=8000000
bandwidth=0
lnb_lo=0
```

V GUI totéž: I/O devices → „Other" → Device string `miri=0`.

---

## 5. Reprodukce testu streamu

Tohle je ten nejcennější test — jde **přesně tou knihovnou, kterou by použil KnoflikSDR**:

```python
from gnuradio import gr, blocks
import osmosdr, numpy as np

class TB(gr.top_block):
    def __init__(self, n):
        gr.top_block.__init__(self)
        self.src = osmosdr.source("miri=0")
        self.src.set_sample_rate(8e6)
        self.src.set_center_freq(7.3e6)
        self.src.set_gain(6)
        self.head = blocks.head(gr.sizeof_gr_complex, n)
        self.snk = blocks.vector_sink_c()
        self.connect(self.src, self.head, self.snk)

tb = TB(2_000_000)
tb.run()
d = np.array(tb.snk.data())
# ověř: len(d) == 2000000, všechny nenulové, ve spektru špička >> medián
```

**Naměřeno:** 2 000 000 vzorků, 100 % nenulových, průměrná úroveň −8,2 dBFS,
ve spektru **špička 72 dB nad mediánem** → skutečný signál, ne šum ani nuly.

Pozn.: `src.get_sample_rates()` vrací `meta_range_t`, který **není iterovatelný** — použij
`.start()` / `.stop()` / `.step()`.

---

## 6. Co bude potřeba udělat v KnoflikSDR

Seřazeno podle toho, kolik to je práce. **Přístup k hardwaru není v seznamu — ten je vyřešený.**

### 6.1 Abstrakce zdroje (malá)
`radio.rs` má ALSA capture zadrátovaný natvrdo. Potřebuje trait/enum zdroje, který dodává
`Complex32` + vzorkovačku. SoftRock i RSP1 pak budou jen dvě implementace.

### 6.2 Decimace 8 MSps → 48 kHz (**jádro práce**)
Dva nezávislé problémy:

1. **Poměr není celočíselný.** `8e6 / 48e3 = 166,67` (přesně 500/3). Buď zlomkový polyfázový
   resampler, nebo decimace na 48 192 Hz a smíření se s trvalým odtékáním ringu.
2. **První stupeň musí být levný.** Náš současný 511koeficientový FIR na 8 MSps by dělal
   ~4 miliardy MAC/s → nereálné. Chce to kaskádu CIC nebo půlpásmových filtrů, teprve pak
   ostrý kanálový filtr na nízké vzorkovačce.

Vedlejší efekt: filtrovat až po decimaci je **správnější i pro SoftRock** — dnes máme přechodové
pásmo ~300 Hz, protože filtrujeme na 96 kHz. Po decimaci by stejný počet koeficientů dal
mnohem strmější hranu a šlo by jít pod dnešní minimum 800 Hz (užitečné pro CW).

### 6.3 Zoomovatelné panorama (největší viditelná změna)
8 MHz na 2048 binů = **3,9 kHz na bin** — SSB signál by nebyl vidět. Potřebuje širokopásmový
přehled + přiblížený výřez, tedy přepracování celého zobrazení a ladění.

### 6.4 Řízení zisku (odhad)
Přes SoapyMiri je dostupné **jen LNA 0–10,2 dB**. RSP1 má stupňů víc (mixer, IF). Na slabé HF
signály to může být málo → možná bude potřeba jít na libmirisdr přímo.
**Pozor:** hlavičky libmirisdr v systému **nejsou**, pro přímé FFI by se musely doinstalovat.

### 6.5 Ztráty vzorků (riziko)
8 MSps × 12 b × 2 kanály = **24 MB/s** proti stropu izochronního přenosu **24,576 MB/s**.
Jede to na doraz sběrnice a ztráty byly vidět jak v `SoapySDRUtil --probe`, tak v gr-osmosdr.

---

## 7. Otevřené otázky (nikdo je zatím neověřil)

- **Umí libmirisdr nižší vzorkovačku než 8 MSps?** SoapyMiri žádnou jinou nenabízí. Kdyby ano,
  vyřešilo by to ztráty vzorků i část decimace. **Tohle zjistit jako první** — může to změnit
  celý návrh řetězce.
- Jsou ztráty vlastnost izochronního přenosu na doraz, nebo jen nedostatečně rychlé vyčítání?
- Kolik stupňů zisku je dostupných přes libmirisdr přímo, mimo SoapyMiri?
- Jak se chová `mirisdr_set_sample_rate` mimo nabízenou hodnotu?

---

## 8. Odhad náročnosti

| Fáze | Rozsah |
|---|---|
| „vydá to zvuk" — abstrakce zdroje + hrubá decimace | pár večerů |
| použitelné — zoom, pořádná vícestupňová decimace, zisk | týdny večerů |

**Riziko nízké.** Nic z toho není výzkum ani reverzní inženýrství — je to poctivá DSP a UI práce
s předvídatelným koncem. Ta část, které by se šlo bát (dostat data ze zařízení), je hotová
a třikrát ověřená.
