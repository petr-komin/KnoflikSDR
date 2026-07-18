# SDRplay RSP1 — podklady pro napojení

**Stav k 17. 7. 2026: implementováno a ověřeno na hardwaru** — RSP1 jede, přepíná se za běhu
selectem v liště (SoftRock ↔ RSP1 bez restartu, ověřeno integračním testem
`prepnuti_radia_za_behu`). Vzorkovačka je volitelná v ⚙ nastavení, od 1,344 do 6 MHz.
Průzkum hotový 16. 7. 2026, přístup k hardwaru ověřen třemi nezávislými cestami.

Co se cestou ukázalo a přepsalo původní plán:

- Vzorkovačka jde nastavit v rozsahu **1,3–12 MSps**, ne jen 8 MSps (§2.1). Jedeme na
  **1 344 000 Sps = 48 000 × 28**, takže decimace je celočíselná. Tím padla nejtěžší část
  návrhu řetězce (§6.2) i riziko ztrát vzorků (§6.5).
- Odhad ceny filtru v §6.2 byl **špatně** — `FirDecim` počítá jen na výstupním vzorku.

Naměřeno v aplikaci (`--probe`, střed 7,3 MHz): `rate=1344000 Hz, špička −14,5 dB,
medián −78,3 dB` — tedy 64 dB nad šumovým dnem, stabilně přes 5 s. Zátěž ~50 % jádra
proti ~25 % u SoftRocku.

**FM demodulace je hotová - WFM i NFM.** V `dsp.rs`:
- **WFM** (rozhlas) má vlastní řetězec (`WfmDemod`): mezifrekvence ~336 kHz, frekvenční
  diskriminátor, deemfáze 50 µs (CCIR), decimace na 48 kHz. Ověřeno na živé stanici
  (`wfm_ze_stanice`): energie zvuku 20× soustředěná v programovém pásmu.
- **NFM** (2 m/70 cm, kanál ~16 kHz) se vejde do stávajícího řetězce - jen za kanálovým
  filtrem je diskriminátor a audio propust. Ověřeno na 2 m i 70 cm (`nfm_z_vysilani`).

**"Díry" tuneru jsou lež, stejně jako u vzorkovačky.** SoapyMiri hlásí rozsahy s mezerami
(108-162, 240-470, 960-1450 MHz), ale to jsou jen metadata - RSP1 přes libmirisdr **ladí
souvisle do 2 GHz** (ověřeno: `set_frequency` projde a streamuje na 145, 300, 433, 1090,
1296 MHz; korelace spekter na 145/150/155 je 0,1-0,4, tedy opravdu ladí, neořezává).
Proto jsou v bandplanu i 2 m, 70 cm a 23 cm a strop ladění je 2 GHz.

> **Poznámka k S-metru a šumu:** NFM ani WFM nemají squelch, takže na prázdném kanálu jde
> ven zesílený šum. Squelch je zjevný další krok.

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
Full freq range: [0.15, 30], [64, 108], [162, 240], [470, 960], [1450, 1675] MHz  ← LŽE, viz níž
Sample rates: 8 MSps     (jiná se nenabízí)  ← taky lže
```

Dvě věci stojí za zapamatování:

- **CF32 nativně** — knihovna nám dá rovnou `Complex<f32>`, přesně to, co náš DSP už žere. Odpadá rozbalování 12bitových vzorků.
- **Frekvenční rozsah je souvislý do 2 GHz.** Mezery, které tenhle probe hlásí, jsou stejná
  lež v metadatech jako u vzorkovačky (§2.1) — `set_frequency` bere cokoli a opravdu tam ladí
  (ověřeno korelací spekter). Datasheet RSP1 uvádí souvislé 1 kHz–2 GHz a sedí to.

## 2.1 Vzorkovačka — 8 MSps je jen to, co inzeruje SoapyMiri

**Ověřeno 17. 7. 2026** přes `gr-osmosdr` backend `miri` (ten sahá na libmirisdr přímo, mimo Soapy).

`get_sample_rates()` sice hlásí `start=8000000 stop=8000000 step=0`, ale to je **jen metadata**.
`set_sample_rate()` ve skutečnosti bere cokoli od **1,3 MSps do 12 MSps**; pod 1,3 MSps knihovna
sama hlásí `can't set rate …, setting minimum rate: 1300000`.

**Platí i pro crate `soapysdr`** — ověřeno 17. 7. 2026 crate `soapysdr` 0.4.5 proti
`driver=miri`. `get_sample_rate_range()` vrátí `min=8000000 max=8000000 step=0` (tedy stejná
lež jako u gr-osmosdr), ale `set_sample_rate(1_344_000)` **projde a opravdu streamuje**:

```
chteno   1344000 -> hlasi   1344000 | zmereno   1334177 =  99.3% | chyb cteni 0
chteno   2000000 -> hlasi   2000000 | zmereno   2056188 = 102.8% | chyb cteni 0
chteno   8000000 -> hlasi   8000000 | zmereno   8053091 = 100.7% | chyb cteni 0
```

Obsah streamu na 1,344 MSps (262 144 vzorků, střed 7,3 MHz): **100 % nenulových, −8,1 dBFS,
špička 89 dB nad mediánem** — tedy skutečný signál, a úroveň sedí s měřením přes gr-osmosdr
v §5 (−8,2 dBFS). Přes Soapy navíc **nula chyb čtení**, zatímco gr-osmosdr sypal `samples lost`.

**Nikdy nevěř `get_sample_rate_range()` u SoapyMiri — je to metadata, ne schopnost zařízení.**

Změřeno (1 s streamu, střed 7,3 MHz, zisk 6 dB):

| chtěno | hlásí | změřeno | nenulových | špička nad mediánem |
|---|---|---|---|---|
| 1 300 000 | 1 300 000 | 1 258 956 | 100 % | 87 dB |
| **1 344 000** | **1 344 000** | **1 299 161** | **100 %** | **87 dB** |
| 1 536 000 | 1 536 000 | 1 489 938 | 100 % | 87 dB |
| 1 920 000 | 1 920 000 | 1 881 523 | 100 % | 87 dB |
| 3 072 000 | 3 072 000 | 3 024 678 | 100 % | 86 dB |
| 8 000 000 | 8 000 000 | 7 727 314 | 100 % | 82 dB |

Naměřená hodnota vychází soustavně na ~97 % chtěné **při každé rychlosti včetně 8 MSps**, což
odpovídá konstantní režii startu v jednosekundovém okně, ne ztrátám úměrným toku. Skutečné
ztráty jsem tímhle testem neizoloval — hlášky `samples lost` z gr-osmosdr chodily dál.

**Klíčový důsledek: 1 344 000 = 48 000 × 28.** Existuje tedy vzorkovačka, ze které se na 48 kHz
audio dostaneme **celočíselnou decimací 28×**. Zlomkový resampler ani smíření se s odtékajícím
ringem (§6.2 bod 1) nejsou potřeba.

V aplikaci se nabízí víc vzorkovaček, všechny násobky 48 kHz (v `source::RSP1_RATES_HZ`):
1,344 / 1,920 / 3,072 / 4,800 / 6,000 MHz. Ověřeno na hardwaru, že **všech osm testovaných**
(28/32/40/50/64/80/100/125×) streamuje 100 % nenulových vzorků se špičkou >85 dB. Nižší =
užší panorama a míň zátěže, vyšší = širší přehled. Výchozí je nejužší 1,344 MHz.

Na tok po USB to má taky vliv: 1,344 MSps × 3 B = **4,03 MB/s** proti stropu 24,576 MB/s, tedy
16 % sběrnice místo jízdy na doraz (§6.5).

> **Pozn. k testování — na tomhle jsem naběhl dvakrát:** RSP1 jde otevřít **jen jednou**.
> Druhé otevření **tiše zamrzne** (žádná chyba, jen visí), a to i když ten první handle už byl
> zahozen — uvolnění není okamžité. Platí pro `osmosdr.source()` v Pythonu i pro
> `soapysdr::Device::new()` v Rustu. Otevři zařízení jednou a rychlosti projeď na něm;
> nikdy neotvírej zvlášť „na dotaz" a zvlášť „na stream".

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

## 6. Co bylo potřeba udělat v KnoflikSDR

**Hotovo 17. 7. 2026.** Sekce zůstává jako záznam, proč to vypadá, jak to vypadá.

### 6.1 Abstrakce zdroje — hotová, ale ve dvou patrech
Je v `src/source/`. Podstatné je, že zdroj je **rozdělený na dvě půlky do různých vláken**:

- `Source` dodává vzorky a bydlí v DSP vlákně.
- `Tuner` ladí a řídí zisk a bydlí ve vlastním vlákně — u SoftRocku je ladění zápis do Si570
  po USB, trvá jednotky ms a v DSP vlákně by cvakalo do zvuku.

Obě půlky otevírá `source::open()` naráz z DSP vlákna, které si nechá `Source` a `Tuner` pošle
kanálem ladicímu vláknu. Díky tomu se při opětovném připojení vymění obě společně.

U SoftRocku jsou to dvě fyzicky nezávislá zařízení (zvukovka + Si570), u RSP1 jedno — ale
`soapysdr::Device` je `Clone` a `set_frequency` bere `&self`, takže každá půlka drží klon.
**Ne nové otevření** — to by zamrzlo, viz poznámka v §2.1.

Pod SoftRockovou větví zůstal starší trait `Capture` z `src/audio/` (zvukovky a bitová hloubka).

### 6.2 Decimace na 48 kHz (**už ne jádro práce**)
Po zjištění z §2.1 se úloha smrskla:

1. ~~**Poměr není celočíselný.**~~ **Vyřešeno:** jedeme na **1 344 000 Sps = 48 000 × 28**,
   decimace je celočíselná 28×. Žádný zlomkový resampler.
2. ~~**První stupeň musí být levný.**~~ **Taky odpadá** — a původní odhad byl špatně. `FirDecim`
   v `dsp.rs` počítá skalární součin **jen na výstupním vzorku**, ne na každém vstupním
   (`push()` vrátí `None` a hned se vrátí, dokud nedojde fáze). Cena filtru je tedy
   `počet_koeficientů × výstupní_rychlost`, nezávisle na vstupní. Kaskáda CIC ani půlpásmových
   filtrů není potřeba.

   Co na vstupní rychlosti záleží: NCO a zápis do historie, tedy pár operací na vzorek
   (1,344 M/s = zanedbatelné), a **FFT panoramatu** — tam pozor, dnes se počítá každých
   2048 vzorků, což by na 1,344 MSps bylo 656 FFT/s místo dnešních 47. To se musí přiškrtit,
   GUI stejně kreslí ~60×/s.

   Zvětšit se musí `PRE_TAPS` (dnes 127, laděno na 96 kHz vstup). Na 1,344 MSps je přechodové
   pásmo 127 koeficientů ~35 kHz, takže by aliasing prolezl. Potřeba ~1023, což při 48 kHz
   výstupu stojí 49 M complex MAC/s — zhruba tolik co dnešní kanálový filtr, tedy únosné.

Vedlejší efekt: filtrovat až po decimaci je **správnější i pro SoftRock** — dnes máme přechodové
pásmo ~300 Hz, protože filtrujeme na 96 kHz. Po decimaci by stejný počet koeficientů dal
mnohem strmější hranu a šlo by jít pod dnešní minimum 150 Hz (užitečné pro CW).

### 6.3 Zoomovatelné panorama — nebylo potřeba nic přepisovat
Na 1,344 MSps vyjde 2048 binů na **656 Hz na bin** místo 3,9 kHz. Aplikace už zoom má
(`MAX_ZOOM = 32`), což dá výřez ~42 kHz. **Jestli to na SSB stačí, zatím nikdo neposoudil** —
ověřeno je jen to, že panorama jede a ukazuje signál.

Co ale potřeba bylo: **přiškrtit FFT**. Počítala se každých 2048 vzorků, což je na 96 kHz
~47×/s, ale na 1,344 MSps by to bylo 656×/s — GUI přitom kreslí ~60×/s. `FFT_INTERVAL`
v `radio.rs` to drží na ~62×/s (ověřeno: `FFT#37` → `FFT#95` za sekundu).

### 6.4 Řízení zisku — hotové, ale jen LNA
Přes SoapyMiri je dostupné **jen LNA 0–10,2 dB**. RSP1 má stupňů víc (mixer, IF). Na slabé HF
signály to může být málo → možná bude potřeba jít na libmirisdr přímo.
**Pozor:** hlavičky libmirisdr v systému **nejsou**, pro přímé FFI by se musely doinstalovat.

V aplikaci je posuvník v ⚙ nastavení, rozsah si bere z `gain_range()` zařízení (nehádá se).
Zisk se na rozdíl od zbytku nastavení **projeví hned, bez restartu** — jde vlastním kanálem
do ladicího vlákna. Jestli je 10,2 dB na slabé signály dost, **zatím nevyzkoušeno**.

### 6.5 ~~Ztráty vzorků~~ (riziko odpadlo na 1,344 MSps)
Na 8 MSps to jelo na doraz: 8 MSps × 3 B = **24 MB/s** proti stropu **24,576 MB/s**, a ztráty
byly vidět v `SoapySDRUtil --probe` i v gr-osmosdr. Na **1,344 MSps** je to 4,03 MB/s, tedy
16 % sběrnice. Pokud KnoflikSDR pojede na 1,344 MSps, tenhle bod je bezpředmětný.

---

## 7. Otevřené otázky

- ~~**Umí libmirisdr nižší vzorkovačku než 8 MSps?**~~ **Zodpovězeno 17. 7. 2026: ano, 1,3–12 MSps.**
  Viz §2.1. SoapyMiri to jen neinzeruje.
- Jsou ztráty vlastnost izochronního přenosu na doraz, nebo jen nedostatečně rychlé vyčítání?
  (Na 1,344 MSps nejspíš jedno — sběrnice má 6× rezervu.)
- Kolik stupňů zisku je dostupných přes libmirisdr přímo, mimo SoapyMiri? (Zůstává, §6.4.)
- Jaká je skutečná šířka pásma po `mirisdr_set_bandwidth` na 1,344 MSps? Tuner MSi001 má
  vlastní IF filtry a je otázka, jestli se nastaví samy podle vzorkovačky, nebo se o ně
  musíme přihlásit — jinak riskujeme aliasing z okolí.
- ~~Nabízí crate `soapysdr` cestu k rychlostem, které SoapyMiri neinzeruje?~~ **Zodpovězeno
  17. 7. 2026: ano**, `set_sample_rate(1_344_000)` přes crate `soapysdr` 0.4.5 projde a streamuje.
  Viz §2.1. Cesta (a) tedy platí a na libmirisdr přímo není důvod jít.

---

## 8. Co zbývá

Řetězec jede a panorama ukazuje signál. **Neposouzené je, jak to zní a jak se s tím ladí** —
`--probe` na to neodpoví:

- **Poslechnout AM/SSB.** WFM i NFM jsou ověřené (`wfm_ze_stanice`, `nfm_z_vysilani`), ale že
  z RSP1 leze srozumitelné **AM/SSB na KV**, ověřeno automaticky **není** - jen že vzorky tečou.
- **Squelch pro FM.** WFM ani NFM ho nemají, takže prázdný kanál syčí zesíleným šumem. Nejvíc
  chybějící věc pro reálný poslech 2 m/70 cm.
- **WFM stereo a RDS** by byl další krok (pilot 19 kHz, RDS na 57 kHz). Teď je WFM mono.
- **Zisk 0–10,2 dB na slabé signály** (§6.4). Když nebude stačit, je to důvod na libmirisdr přímo.
- **IF filtry MSi001** (§7). Nevíme, jestli se nastaví samy podle vzorkovačky. Kdyby ne, leze
  do 672 kHz okolo aliasing zvenčí — antialiasingová propust v `dsp.rs` řeší jen decimaci,
  ne to, co pustí dovnitř tuner.
- **Zoom na SSB** (§6.3): 656 Hz/bin, `MAX_ZOOM = 32`. Nejspíš stačí, ale nikdo se nedíval.
- **Mrtvá zóna kolem DC.** `DC_GUARD_HZ` v `main.rs` je psaná pro SoftRock. MSi001 je taky
  zero-IF, takže spur nejspíš má taky — ale jestli je stejně velký, nevíme.
