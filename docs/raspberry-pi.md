# SoftRock na Raspberry Pi — podklady

**Stav:** nevyzkoušeno, Pi jsem neměl k dispozici. Čísla níže jsou **změřená
na desktopu** a přepočtená odhadem; ber je jako vodítko, ne jako slib.

Scénář je **SoftRock na Pi**, tedy tahle aplikace tak, jak je: I/Q ze zvukové
karty, Si570 přes USB. Nic z těžké práce kolem SDRplay (decimace z 8 MSps,
zoomovatelné panorama pro MHz rozsahy) tu není potřeba — ta je popsaná zvlášť
v [sdrplay-rsp1.md](sdrplay-rsp1.md) a s tímhle scénářem nesouvisí.

---

## 1. Kolik to stojí procesoru — změřeno

Test `dsp::tests::zmer_vykon_retezce` (v `src/dsp.rs`) prožene DSP řetězcem
10 sekund signálu a vypíše realtime faktor. Spustit:

```bash
cargo test --release zmer_vykon -- --nocapture
```

Na **Intel i9-10900X @ 3,7 GHz**:

| režim | dekodér | CPU | realtime |
|---|---|---|---|
| AM 8 kHz | vypnuto | 8,8 % jádra | 11,3× |
| CW 500 Hz | vypnuto | 8,2 % | 12,2× |
| CW 500 Hz | CW | 7,4 % | 13,5× |
| AM 8 kHz | RTTY | 7,8 % | 12,9× |

**Dekodéry jsou v rámci šumu zadarmo.** Drtivá většina času jde do kanálového
filtru (1023 koeficientů na 48 kHz).

> Pozn.: měření má rozehřívací průchod. Bez něj vycházel první řádek o 60 %
> pomaleji kvůli cache a sváděl to k nesmyslným závěrům.

## 2. Klíčové zjištění: nevektorizuje se to

Build s `RUSTFLAGS="-C target-cpu=native"` (tedy s AVX2) je **stejně rychlý**
jako výchozí — 0,83 vs 0,88 s, což je v šumu měření.

Filtr se tedy nevektorizuje. Na vině je nejspíš kruhové indexování v
`FirDecim::push`:

```rust
let i = self.idx.wrapping_sub(1 + k) & self.mask;
```

Kompilátor nedokáže dokázat souvislost paměti, takže to jede skalárně.

**Pro Pi je to dobrá zpráva.** Neneseme tam handicap užšího SIMD (NEON 128 bit
proti AVX2 256 bit), protože žádné SIMD nepoužíváme ani na x86. Zbývá jen
rozdíl v taktu a IPC.

## 3. Odhad pro Pi

Čistě z taktu a IPC, **nevyzkoušeno**:

| | odhad DSP |
|---|---|
| Pi 5 (Cortex-A76 @ 2,4 GHz) | ~15–20 % jádra |
| Pi 4 (Cortex-A72 @ 1,5 GHz) | ~35–45 % jádra |
| Pi 3 | nedoporučuju, viz §4 |

DSP je jedno vlákno, Pi má čtyři jádra — tohle by nemělo být úzké hrdlo.

## 4. Kde to reálně zaškobrtne

### Vodopád (největší riziko)
`App::update` nahrává **celou texturu 2048×256 RGBA, tedy 2 MB každý snímek**,
přestože se mění jediný řádek. Při 30 fps je to 60 MB/s po sběrnici. Na desktopu
to zapadne, na Pi to bude bolet víc než celý DSP dohromady.

**Podle mého je tohle skutečný limit, ne procesor.**

### Zvuková karta
Testovaný Sound Blaster jede jako full-speed USB (12 Mbit/s). Pi 4 a 5 mají
oddělený USB řadič, takže v pořádku. **Pi 3 sdílí USB s ethernetem** a 96kHz
izochronní stream by tam mohl trhat.

### Vykreslování
Pi 4/5 umí GLES 3.1, což backendu `glow` stačí.

**Důležité:** projekt je schválně přepnutý z výchozího `wgpu` na `glow`
(viz `Cargo.toml`). wgpu na Linuxu chce Vulkan, a když pro kartu není Vulkan
ovladač, spadne to do softwarového llvmpipe. Na vývojovém stroji (NVIDIA Quadro
M2000 s funkčním OpenGL, ale bez Vulkan ICD) to znamenalo **587 % CPU místo
65 %** — 20 vláken llvmpipe po ~33 %. Kdyby někdo tuhle volbu vrátil, na Pi to
bude ještě horší než na desktopu.

## 5. Páky, kdyby to nestíhalo

Seřazeno podle poměru zisk/práce. Žádná není výzkum:

1. **Vektorizovat filtr** — držet historii lineárně a zapisovat každý vzorek
   dvakrát, aby skalární součin četl souvislý úsek. Kompilátor to pak
   zvektorizuje. Čekal bych 2–4×; největší jednotlivý zisk a ověřitelný tím
   samým testem výkonu.
2. **Vodopád po řádcích** — nahrávat jen změněný řádek místo celé textury.
   Chce to kruhový buffer a posun přes UV.
3. **Kratší kanálový filtr pro AM a SSB** — 1023 koeficientů potřebuje jen
   úzké CW. Pro fonii by stačila čtvrtina. Pozor: meze v
   `radio::bandwidth_range` jsou navázané na ostrost filtru a hlídá je test
   `uzky_cw_filtr_odpovida_stitku`.
4. **Snížit snímkovou frekvenci** GUI z 30 fps.

## 6. Jak to vyzkoušet

`cargo build --release` na aarch64 projde bez úprav. Potřebné balíčky jsou
stejné jako na desktopu (`libasound2-dev`, `libusb-1.0-0-dev`).

Pak spustit test výkonu — vypíše skutečná čísla místo mých odhadů:

```bash
cargo test --release zmer_vykon -- --nocapture
```

A `./target/release/knoflik-sdr --probe` ukáže, jestli zvukovka dává I/Q
a jestli teče signál, bez spouštění GUI.
