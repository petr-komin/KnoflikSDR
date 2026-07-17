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
- Nastavení se ukládá průběžně do `~/.config/knoflik-sdr/config.toml`

## Hardware

Vyvíjeno na **SoftRock RX Ensemble II** se Si570 (USB VID:PID `16c0:05dc`, firmware DG8SAQ)
a zvukovkou Creative Sound Blaster HD na 96 kHz.

Zvukovku i formát si program **vyjedná sám** — zkouší 192/96/48 kHz a v každé rychlosti
nejdřív 24 bit, pak 16.

## Sestavení

Potřebuješ Rust a vývojové balíčky ALSA a libusb:

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

Zvuková karta, kalibrace krystalu Si570 a I2C adresa jsou zatím konstanty na začátku
`src/main.rs`:

```rust
const CAPTURE_DEVICE: &str = "hw:CARD=HD,DEV=0";
const SI570_XTAL: f64 = 114_269_790.0;
const SI570_I2C_ADDR: u16 = 0x55;
```

Krystal je potřeba zkalibrovat pro každý kus zvlášť. Hodnotu můžeš převzít z `~/.quisk_conf.py`,
pokud jsi předtím jel na Quisku.

USB práva řeší na Debianu udev pravidlo z `libhamlib4`, root potřeba není.

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

Průzkum k napojení SDRplay RSP1 je v [docs/sdrplay-rsp1.md](docs/sdrplay-rsp1.md) —
přístup k hardwaru je ověřený, chybí decimace z 8 MSps a zoomovatelné panorama.
