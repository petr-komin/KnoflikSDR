<p align="center">
  <img src="docs/logo.jpg" alt="KnoflikSDR" width="220">
</p>

# KnoflikSDR

SDR přijímač pro **SoftRock** psaný v Rustu. Bere I/Q ze zvukové karty, ladí Si570 přes USB
(protokol DG8SAQ) a k tomu dělá panorama, vodopád a demodulaci AM/USB/LSB.

Vznikl jako náhrada Quisku pro ty SoftRocky, které berou I/Q ze zvukovky — s cílem mít
jeden statický binár místo Pythonu s C rozšířením.

## Co umí

- **Příjem AM, USB a LSB** z I/Q ze zvukové karty
- **Ladění Si570** přes USB, bez proprietárních knihoven
- **Panorama a vodopád** přes celou vzorkovací frekvenci, s mřížkou v dB a kHz
- **Ladění kliknutím** do spektra i vodopádu, tažením hran se mění šířka pásma
- **Oblíbené stanice** — jedním klikem i s režimem a šířkou filtru
- **Kdo to vlastně vysílá** — v AM se podle rozpisu EiBi ukáže, která stanice
  má na naladěné frekvenci právě teď být
- **Vyznačená mrtvá zóna** kolem VFO, kde má SoftRock DC spur
- **Doladění na nejsilnější stanici** po skoku o celé okno
- **Nastavení v okně** — zvuková zařízení, bitová hloubka i kalibrace Si570
- Nastavení se ukládá průběžně do `~/.config/knoflik-sdr/config.toml`

## Hardware

Vyvíjeno na **SoftRock RX Ensemble II** se Si570 (USB VID:PID `16c0:05dc`, firmware DG8SAQ)
a zvukovkou Creative Sound Blaster HD na 96 kHz.

Formát si program **vyjedná sám** — zkouší 192/96/48 kHz a v každé rychlosti
nejdřív 24 bit, pak 16.

## Sestavení

Potřebuješ Rust a vývojové balíčky libusb; na Linuxu navíc ALSA:

```bash
sudo apt install libasound2-dev libusb-1.0-0-dev
cargo build --release
./target/release/knoflik-sdr
```

Diagnostika bez GUI — ukáže, co si vyjednal vstup a jestli teče signál:

```bash
./target/release/knoflik-sdr --probe
```

## Nastavení

Tlačítko **⚙ nastavení** v liště otevře okno, kde se vybírá zvuková karta se vstupem I/Q,
výstupní zařízení, strop bitové hloubky a kalibrace Si570. Zvuk a Si570 se čtou při startu,
takže se změny **projeví až po restartu** programu.

Krystal je potřeba zkalibrovat pro každý kus zvlášť. Hodnotu můžeš převzít z `~/.quisk_conf.py`,
pokud jsi předtím jel na Quisku.

USB práva řeší na Debianu udev pravidlo z `libhamlib4`, root potřeba není.

## Přenositelnost

Zvuk je jediné, co se mezi systémy liší:

| | vstup a výstup | hloubka na `automaticky` |
|---|---|---|
| **Linux** | ALSA napřímo | 24 bit (`S243LE`) |
| **Windows** | cpal → WASAPI | 16 bit |
| **macOS** | cpal → CoreAudio | 16 bit |

Packed 24 bit umí spolehlivě jen ALSA. Jinde o formátu rozhoduje zvukový server, proto
tam automatika cílí na 16 bit — v nastavení jde hloubka přepnout ručně, kdyby to karta
zvládla. Zbytek (DSP, GUI přes OpenGL, ladění Si570 přes libusb) je stejný všude.

Na Windows si libusb ovladač pro SoftRock musíš podstrčit přes [Zadig](https://zadig.akeo.ie/),
jinak se rádio na USB nenajde.

## Rozpis stanic

Sezónní rozpis KV rozhlasu se stahuje z [EiBi](http://www.eibispace.de) do
`~/.cache/knoflik-sdr/`. Stahuje se jednou za sezónu, na pozadí — start
aplikace na síť nečeká a bez připojení funguje všechno ostatní.

Data udržuje a volně poskytuje Eike Bierwirth. Poděkování patří jemu.

## Licence

**GPL-3.0-or-later**, viz [LICENSE](LICENSE).

Funkce `registers()` v `src/si570.rs` je port ze souboru `softrock/hardware_usb.py` projektu
[Quisk](https://james.ahlstrom.name/quisk/) — Copyright (C) 2006-2025 James C. Ahlstrom, GPL.
Vlastní výpočet HSDIV/N1/RFREQ pro Si570 napsal **Ethan Blanton, KB8OJH**. Zbytek programu
je psaný od nuly.

## Stav

Funkční přijímač pro denní poslech. Vysílání není a zatím se nechystá.

Poznámky k dalším směrům:

- [docs/raspberry-pi.md](docs/raspberry-pi.md) — provoz SoftRocku na Pi.
  DSP zabere ~8 % jádra i9, takže by to mělo stačit; úzkým hrdlem bude spíš
  vodopád než procesor.
- [docs/sdrplay-rsp1.md](docs/sdrplay-rsp1.md) — napojení SDRplay RSP1.
  Přístup k hardwaru je ověřený, chybí decimace z 8 MSps a zoomovatelné
  panorama. Podstatně větší úloha.
