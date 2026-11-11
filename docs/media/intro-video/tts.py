#!/usr/bin/env python3
"""Synthesize the narration and lay it out on a timeline.

    python3 -m venv .venv && .venv/bin/pip install edge-tts
    .venv/bin/python tts.py

Reads narration.json, synthesizes each line with a Microsoft Edge neural voice
(edge-tts: online, no key; the narration text is sent to Microsoft's speech
service), trims the silence around each clip and writes, under
out/intro-video/:

    narration.wav   the whole voice track, 48 kHz mono
    timeline.json   start/end of every scene and line, read by video.html
    captions.srt    the same lines as subtitles

Clips are cached by voice + rate + text, so editing one line only
re-synthesizes that line.
"""

import asyncio
import hashlib
import json
import subprocess
import wave
from pathlib import Path

import edge_tts

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "out" / "intro-video"
CLIPS = OUT / "clips"
RATE = 48_000
LEAD, TAIL = 0.55, 0.65  # default silence before a scene's first line / after its last


async def synthesize(text: str, voice: str, rate: str) -> Path:
    key = hashlib.sha256(f"{voice}|{rate}|{text}".encode()).hexdigest()[:16]
    mp3 = CLIPS / f"{key}.mp3"
    if not mp3.exists():
        for attempt in range(4):
            try:
                await edge_tts.Communicate(text, voice, rate=rate).save(str(mp3))
                break
            except Exception:
                mp3.unlink(missing_ok=True)
                if attempt == 3:
                    raise
                await asyncio.sleep(2 * (attempt + 1))
    return mp3


def decode_trimmed(mp3: Path) -> bytes:
    """mp3 → s16le mono PCM with leading and trailing silence removed."""
    trim = "silenceremove=start_periods=1:start_threshold=-48dB:start_silence=0.03"
    chain = f"{trim},areverse,{trim},areverse"
    return subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(mp3), "-af", chain, "-ac", "1", "-ar", str(RATE), "-f", "s16le", "-"],
        check=True,
        capture_output=True,
    ).stdout


def stamp(t: float) -> str:
    ms = round(t * 1000)
    return f"{ms // 3_600_000:02}:{ms // 60_000 % 60:02}:{ms // 1000 % 60:02},{ms % 1000:03}"


async def main() -> None:
    CLIPS.mkdir(parents=True, exist_ok=True)
    script = json.loads((HERE / "narration.json").read_text())
    voice, rate, gap = script["voice"], script["rate"], script["gap"]

    pcm = bytearray()
    scenes, srt = [], []

    def now() -> float:
        return len(pcm) / 2 / RATE

    def silence(seconds: float) -> None:
        pcm.extend(b"\0\0" * round(seconds * RATE))

    for scene in script["scenes"]:
        start = now()
        silence(scene.get("lead", LEAD))
        lines = []
        for i, line in enumerate(scene["lines"]):
            if i:
                silence(gap)
            clip = decode_trimmed(await synthesize(line.get("say", line["text"]), voice, rate))
            begin = now()
            pcm.extend(clip)
            lines.append({"text": line["text"], "start": round(begin, 3), "end": round(now(), 3)})
            srt.append(f"{len(srt) + 1}\n{stamp(begin)} --> {stamp(now())}\n{line['text']}\n")
        silence(scene.get("tail", TAIL))
        scenes.append({"id": scene["id"], "start": round(start, 3), "end": round(now(), 3), "lines": lines})

    with wave.open(str(OUT / "narration.wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(bytes(pcm))
    (OUT / "timeline.json").write_text(
        json.dumps({"duration": round(now(), 3), "voice": voice, "scenes": scenes}, ensure_ascii=False, indent=2)
    )
    (OUT / "captions.srt").write_text("\n".join(srt))
    print(f"{now():.1f}s narration, {len(srt)} lines → {OUT}")
    for s in scenes:
        print(f"  {s['id']:9} {s['start']:6.1f} – {s['end']:6.1f}")


asyncio.run(main())
