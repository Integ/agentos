#!/usr/bin/env python3
"""Synthesize the soundtrack under the narration.

    .venv/bin/pip install numpy
    .venv/bin/python music.py        # after tts.py

Reads out/intro-video/timeline.json (written by tts.py) and writes
out/intro-video/music.wav, 48 kHz stereo. Everything is generated here from
oscillators and seeded noise, so there is no sample or track to license and
the same timeline always gives the same file.

The music follows the picture: a riser and five hits under the cold open, a
drop on the title, a whoosh and a thump on every cut, and the bed dips under
every spoken line so the voice stays on top.
"""

from __future__ import annotations

import json
import wave
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "out" / "intro-video"
SR = 48_000
BPM = 112
BEAT = 60 / BPM
BAR = 4 * BEAT

rng = np.random.default_rng(20261127)

# D minor: i – VI – III – VII, two bars each.
CHORDS = [
    {"pad": [50, 53, 57, 62], "bass": 38, "arp": [62, 65, 69, 74, 77]},  # Dm
    {"pad": [50, 53, 58, 62], "bass": 34, "arp": [62, 65, 70, 74, 77]},  # Bb
    {"pad": [48, 53, 57, 60], "bass": 41, "arp": [60, 65, 69, 72, 77]},  # F
    {"pad": [48, 52, 55, 60], "bass": 36, "arp": [60, 64, 67, 72, 76]},  # C
]


def hz(midi: float) -> float:
    return 440.0 * 2 ** ((midi - 69) / 12)


def table(harmonics: int, rolloff: float, odd_only: bool = False) -> np.ndarray:
    """One period of a band-limited wave, 1/n harmonics with a soft rolloff."""
    phase = np.arange(4096) / 4096
    wave_ = np.zeros(4096)
    for n in range(1, harmonics + 1):
        if odd_only and n % 2 == 0:
            continue
        wave_ += np.sin(2 * np.pi * n * phase) / n * np.exp(-n / rolloff)
    return wave_ / np.abs(wave_).max()


SAW = table(36, 9.0)
WARM = table(10, 3.0)
PLUCK = table(9, 4.0, odd_only=True)


def osc(tab: np.ndarray, freq, n: int, phase0: float = 0.0) -> np.ndarray:
    f = np.broadcast_to(np.asarray(freq, dtype=float), (n,))
    phase = (phase0 + np.cumsum(f) / SR) % 1.0
    return tab[(phase * len(tab)).astype(int) % len(tab)]


def env(n: int, attack: float, release: float, sustain_to: int | None = None) -> np.ndarray:
    """Linear attack, hold, then a smooth release over the last `release` seconds."""
    e = np.ones(n)
    a = max(1, int(attack * SR))
    e[:a] = np.linspace(0, 1, a) ** 1.5
    r = max(1, int(release * SR))
    end = n if sustain_to is None else sustain_to
    tail = np.linspace(1, 0, r) ** 2
    e[max(0, end - r):end] *= tail[-min(r, end):]
    e[end:] = 0
    return e


def decay(n: int, seconds: float, attack: float = 0.004) -> np.ndarray:
    t = np.arange(n) / SR
    e = np.exp(-t / seconds)
    a = max(1, int(attack * SR))
    e[:a] *= np.linspace(0, 1, a)
    return e


def onepole(x: np.ndarray, cutoff) -> np.ndarray:
    """Lowpass with a per-sample cutoff (Hz); only used on short sounds."""
    c = np.broadcast_to(np.asarray(cutoff, dtype=float), x.shape)
    a = 1 - np.exp(-2 * np.pi * c / SR)
    y = np.empty_like(x)
    acc = 0.0
    for i in range(len(x)):
        acc += a[i] * (x[i] - acc)
        y[i] = acc
    return y


def fft_filter(x: np.ndarray, low: float | None = None, high: float | None = None) -> np.ndarray:
    """Static highpass/lowpass on a whole bus, with soft edges."""
    spec = np.fft.rfft(x)
    f = np.fft.rfftfreq(len(x), 1 / SR)
    gain = np.ones_like(f)
    if high is not None:
        gain *= 1 / np.sqrt(1 + (f / high) ** 4)
    if low is not None:
        gain *= 1 / np.sqrt(1 + (low / np.maximum(f, 1e-3)) ** 4)
    return np.fft.irfft(spec * gain, len(x))


def reverb(x: np.ndarray, seconds: float, seed: int) -> np.ndarray:
    r = np.random.default_rng(seed)
    n = int(seconds * SR)
    t = np.arange(n) / SR
    ir = r.standard_normal(n) * np.exp(-t / (seconds / 4.2))
    ir = fft_filter(ir, high=5200)
    ir[: int(0.012 * SR)] = 0  # a little pre-delay
    ir /= np.sqrt((ir**2).sum())
    size = 1 << int(np.ceil(np.log2(len(x) + n)))
    return np.fft.irfft(np.fft.rfft(x, size) * np.fft.rfft(ir, size), size)[: len(x)]


class Bus:
    def __init__(self, n: int):
        self.l = np.zeros(n)
        self.r = np.zeros(n)

    def add(self, sig: np.ndarray, start: float, gain: float = 1.0, pan: float = 0.0) -> None:
        i0 = int(round(start * SR))
        if i0 < 0:
            sig, i0 = sig[-i0:], 0
        n = min(len(sig), len(self.l) - i0)
        if n <= 0:
            return
        gl, gr = np.cos((pan + 1) * np.pi / 4) * np.sqrt(2), np.sin((pan + 1) * np.pi / 4) * np.sqrt(2)
        self.l[i0 : i0 + n] += sig[:n] * gain * gl
        self.r[i0 : i0 + n] += sig[:n] * gain * gr


# ---------- instruments ----------

def pad_note(midi: float, seconds: float) -> tuple[np.ndarray, np.ndarray]:
    n = int(seconds * SR)
    left, right = np.zeros(n), np.zeros(n)
    for cents, pan in ((-9, -0.7), (0, 0.0), (8, 0.7)):
        v = osc(SAW, hz(midi + cents / 100), n, rng.random())
        left += v * (1 - pan) / 2
        right += v * (1 + pan) / 2
    e = env(n, 0.9, 1.4)
    return left * e, right * e


def kick(gain: float = 1.0) -> np.ndarray:
    n = int(0.45 * SR)
    t = np.arange(n) / SR
    f = 46 + 110 * np.exp(-t / 0.035)
    body = np.sin(2 * np.pi * np.cumsum(f) / SR) * np.exp(-t / 0.22)
    click = rng.standard_normal(n) * np.exp(-t / 0.004) * 0.25
    return np.tanh((body + click) * 1.6) * gain


def hat(seconds: float = 0.06) -> np.ndarray:
    n = int(seconds * SR)
    noise = rng.standard_normal(n)
    noise = noise - onepole(noise, 7000)  # keep the top
    return noise * decay(n, seconds / 3, 0.001)


def clap() -> np.ndarray:
    n = int(0.3 * SR)
    noise = rng.standard_normal(n)
    band = onepole(noise, 2600) - onepole(noise, 700)
    t = np.arange(n) / SR
    e = np.exp(-t / 0.07) + 0.6 * np.exp(-np.maximum(0, t - 0.012) / 0.05) * (t > 0.012)
    return band * e * 2.2


def bass_note(midi: float, seconds: float) -> np.ndarray:
    n = int(seconds * SR)
    f = hz(midi)
    v = 0.75 * np.sin(2 * np.pi * f * np.arange(n) / SR) + 0.45 * osc(WARM, f, n)
    return v * decay(n, seconds * 0.55, 0.006)


def pluck(midi: float) -> np.ndarray:
    n = int(0.5 * SR)
    v = osc(PLUCK, hz(midi), n) * 0.7 + np.sin(2 * np.pi * hz(midi + 12) * np.arange(n) / SR) * 0.3
    return v * decay(n, 0.14, 0.002)


def whoosh(seconds: float = 0.7) -> np.ndarray:
    n = int(seconds * SR)
    t = np.linspace(0, 1, n)
    noise = rng.standard_normal(n)
    cutoff = 300 + 7000 * t**2.2
    body = onepole(noise, cutoff) - onepole(noise, cutoff * 0.25)
    return body * (t**2.4) * 2.4


def riser(seconds: float) -> np.ndarray:
    n = int(seconds * SR)
    t = np.linspace(0, 1, n)
    noise = rng.standard_normal(n)
    hiss = onepole(noise, 200 + 9000 * t**2) * 1.5
    tone = osc(SAW, hz(50) * 2 ** (t * 1.0), n) * 0.35 + osc(SAW, hz(57) * 2 ** (t * 1.0), n) * 0.25
    tone = onepole(tone, 400 + 5000 * t**2)
    return (hiss + tone) * t**1.8


def impact(big: bool = False) -> np.ndarray:
    seconds = 2.6 if big else 1.2
    n = int(seconds * SR)
    t = np.arange(n) / SR
    f = 26 + (64 if big else 46) * np.exp(-t / 0.25)
    sub = np.sin(2 * np.pi * np.cumsum(f) / SR) * np.exp(-t / (0.9 if big else 0.4))
    noise = rng.standard_normal(n)
    crash = onepole(noise, 5200) * np.exp(-t / (0.5 if big else 0.18)) * (0.7 if big else 0.35)
    return np.tanh((sub * 1.4 + crash) * 1.3)


# ---------- arrangement ----------

def main() -> None:
    tl = json.loads((OUT / "timeline.json").read_text())
    duration = tl["duration"]
    scenes = {s["id"]: s for s in tl["scenes"]}
    n = int((duration + 0.5) * SR)
    bed, drums, fx, send = Bus(n), Bus(n), Bus(n), Bus(n)

    drop = scenes["intro"]["start"]
    groove = scenes["flow"]["start"]
    outro = scenes["outro"]["start"]

    # Cold open: a riser under the typing, five hits under the five words, a drop on the title.
    fx.add(riser(1.8), 0.0, 0.5)
    words = [1.85, 2.27, 2.69, 3.11, 3.53]
    for i, w in enumerate(words):
        fx.add(kick(), w, 0.9)
        fx.add(clap(), w, 0.35, pan=(-0.3 if i % 2 else 0.3))
        send.add(clap(), w, 0.25)
    fx.add(riser(0.75), drop - 0.75, 0.45)
    fx.add(impact(big=True), drop, 1.0)
    send.add(impact(big=True), drop, 0.5)

    # Chords from the drop to the end, two bars each.
    t = drop
    k = 0
    while t < duration:
        chord = CHORDS[k % 4]
        length = 2 * BAR
        last = t + length >= outro
        hold = (duration - t + 0.4) if last else length + 1.4
        for midi in chord["pad"]:
            left, right = pad_note(midi, hold)
            bed.add(left, t, 0.055, pan=-1)
            bed.add(right, t, 0.055, pan=1)
            send.add((left + right) / 2, t, 0.03)
        if last:
            break
        # bass: long notes before the groove starts, then eighths
        if t + length <= groove:
            bed.add(bass_note(chord["bass"], length), t, 0.32)
        else:
            for e in range(16):
                at = t + e * BEAT / 2
                if at >= groove - 0.01 and at < outro:
                    bed.add(bass_note(chord["bass"] + (12 if e % 4 == 3 else 0), BEAT / 2), at, 0.3)
        # arpeggio: sixteenths, up and down, with an echo
        if t + length > drop + 4.0:
            notes = chord["arp"] + chord["arp"][-2:0:-1]
            for s in range(32):
                at = t + s * BEAT / 4
                if drop + 4.0 <= at < outro + 3.5:
                    p = pluck(notes[s % len(notes)])
                    pan = -0.45 if s % 2 else 0.45
                    bed.add(p, at, 0.075, pan=pan)
                    bed.add(p, at + 0.75 * BEAT, 0.03, pan=-pan)
                    send.add(p, at, 0.05)
        t += length
        k += 1

    # Drums from the first scene with a title until the outro: half-time kick, offbeat hats.
    beat = 0
    at = groove
    while at < outro - 0.05:
        if beat % 2 == 0:
            drums.add(kick(), at, 0.55)
        if beat % 4 == 2:
            drums.add(clap(), at, 0.12)
            send.add(clap(), at, 0.06)
        drums.add(hat(), at + BEAT / 2, 0.06, pan=0.35)
        if beat % 2 == 1:
            drums.add(hat(0.03), at + 3 * BEAT / 4, 0.035, pan=-0.35)
        at += BEAT
        beat += 1

    # Every cut: a whoosh into it and a thump on it.
    for s in tl["scenes"][2:]:
        b = s["start"]
        fx.add(whoosh(), b - 0.7, 0.3, pan=-0.2)
        fx.add(impact(big=s["id"] == "outro"), b, 0.55 if s["id"] != "outro" else 0.9)
        send.add(impact(), b, 0.2)

    # Sidechain: the bed breathes with the kick.
    pump = np.ones(n)
    at = groove
    i = 0
    while at < outro:
        if i % 2 == 0:
            j = int(at * SR)
            m = min(n - j, int(0.3 * SR))
            pump[j : j + m] = np.minimum(pump[j : j + m], 0.55 + 0.45 * np.linspace(0, 1, m) ** 0.7)
        at += BEAT
        i += 1
    bed.l *= pump
    bed.r *= pump

    # Under every spoken line the bed and drums dip by 9 dB.
    ctl = 1000
    steps = int((duration + 0.5) * ctl)
    target = np.ones(steps)
    for s in tl["scenes"]:
        for line in s["lines"]:
            a, b = int((line["start"] - 0.12) * ctl), int((line["end"] + 0.18) * ctl)
            target[max(0, a) : b] = 10 ** (-9 / 20)
    duck = np.empty(steps)
    g = 1.0
    for i in range(steps):
        coef = 0.012 if target[i] < g else 0.004  # faster down than up
        g += coef * (target[i] - g)
        duck[i] = g
    duck = np.interp(np.arange(n) / SR, np.arange(steps) / ctl, duck)

    # Filter and mix.
    bed.l, bed.r = fft_filter(bed.l, low=35, high=3800), fft_filter(bed.r, low=35, high=3800)
    wet_l, wet_r = reverb(send.l + bed.l * 0.25, 3.2, 1), reverb(send.r + bed.r * 0.25, 3.2, 2)
    left = (bed.l + drums.l + wet_l * 0.55) * duck + fx.l + wet_l * 0.2 * (1 - duck)
    right = (bed.r + drums.r + wet_r * 0.55) * duck + fx.r + wet_r * 0.2 * (1 - duck)

    # Fade with the picture and keep the level a fixed distance under the voice.
    tail = np.clip((duration - np.arange(n) / SR) / 2.5, 0, 1) ** 1.5
    left, right = left * tail, right * tail
    with wave.open(str(OUT / "narration.wav"), "rb") as w:
        voice = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(float) / 32768
    voice_rms = np.sqrt(np.mean(voice[np.abs(voice) > 0.01] ** 2))
    gaps = duck > 0.95
    music_rms = np.sqrt(np.mean(((left + right) / 2)[gaps] ** 2))
    gain = voice_rms * 10 ** (-7 / 20) / music_rms
    left, right = np.tanh(left * gain * 1.1) / 1.1, np.tanh(right * gain * 1.1) / 1.1
    peak = max(np.abs(left).max(), np.abs(right).max())
    if peak > 0.95:
        left, right = left * 0.95 / peak, right * 0.95 / peak

    pcm = (np.stack([left, right], axis=1) * 32767).astype(np.int16)
    with wave.open(str(OUT / "music.wav"), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())
    print(f"{duration:.1f}s soundtrack → {OUT / 'music.wav'}")


main()
