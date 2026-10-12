#!/usr/bin/env bash
#
# Does Voice Focus pass your voice?
#
#   bash test/voice-check.sh      # then talk for five seconds
#
# Records the raw microphone and the filtered source at the same time and
# compares them. Both at once matters: without the raw track there is no way to
# tell a filter that ate your voice from a microphone that never heard it.
#
# Three things make this harder than it looks, and each one reads as silence:
#
#   - timeout must send INT, not TERM. pw-record finalises the WAV header on
#     Ctrl-C; killed with TERM it leaves a file with no data chunk.
#   - nodes are targeted by name. The numeric ids in `wpctl status` are
#     reassigned whenever nodes come and go.
#   - a sine wave proves nothing. RNNoise exists to remove steady tones, so a
#     tone test is silent whether the filter works or not. Only speech decides.

set -uo pipefail

SECONDS_TO_RECORD=${1:-6}
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

mic=$(pw-metadata -n default 2>/dev/null | grep "key:'default.audio.source'" | tail -1 |
      sed -n "s/.*value:'{\"name\":\"\([^\"]*\)\"}'.*/\1/p")
# The raw device, whatever the default happens to be pointing at.
raw=$(pw-dump 2>/dev/null | /usr/bin/python3 -c "
import json,sys
for o in json.load(sys.stdin):
    p=((o.get('info') or {}).get('props') or {})
    if p.get('media.class')=='Audio/Source' and str(p.get('node.name','')).startswith('alsa_input'):
        print(p['node.name']); break
")

if ! pw-link -o 2>/dev/null | grep -q '^voice_focus:'; then
    echo "Voice Focus is not running. Start it with:"
    echo "    systemctl --user start studio-effects-voice"
    exit 1
fi

echo "Recording ${SECONDS_TO_RECORD}s from both. Talk now, at your normal volume."
timeout -s INT "$SECONDS_TO_RECORD" pw-record --target voice_focus "$OUT/filtered.wav" >/dev/null 2>&1 &
a=$!
timeout -s INT "$SECONDS_TO_RECORD" pw-record --target "$raw" "$OUT/raw.wav" >/dev/null 2>&1 &
b=$!
wait $a $b 2>/dev/null
echo

/usr/bin/python3 - "$OUT" <<'PY'
import array, os, sys, wave

def level(path):
    if not os.path.exists(path) or os.path.getsize(path) < 100:
        return None
    with wave.open(path) as w:
        ch, sw, n = w.getnchannels(), w.getsampwidth(), w.getnframes()
        raw = w.readframes(n)
    if n == 0:
        return None
    s = array.array({1: "b", 2: "h", 4: "i"}[sw])
    s.frombytes(raw)
    full = float(1 << (8 * sw - 1))
    per_channel = []
    for c in range(ch):
        part = s[c::ch]
        peak = max(abs(x) for x in part) / full
        rms = (sum(float(x) * x for x in part) / len(part)) ** 0.5 / full
        per_channel.append((peak * 100, rms * 100))
    return per_channel

out = sys.argv[1]
r = level(f"{out}/raw.wav")
f = level(f"{out}/filtered.wav")
if r is None or f is None:
    sys.exit("  recording failed; is anything else holding the microphone?")

def show(label, chans):
    for i, (peak, rms) in enumerate(chans):
        side = " (left)" if i == 0 and len(chans) > 1 else " (right)" if i == 1 else ""
        print(f"  {label if i == 0 else '':16} ch{i}{side:8} peak {peak:6.2f}%   rms {rms:6.3f}%")

show("raw microphone", r)
show("voice focus", f)
print()

# Both channels must carry the voice, or it plays out of one speaker only.
if len(f) > 1 and f[0][0] > 0.5 and f[1][0] < f[0][0] * 0.1:
    print("  Only the left channel has audio; the source is effectively mono.")
    print("  Check that voice-focus.conf uses noise_suppressor_stereo.")
    print()

r = r[0]
f = f[0]

if r[0] < 1.0:
    print("  The microphone barely heard anything, so this says nothing about the")
    print("  filter. Talk louder or closer, or raise the input volume:")
    print("      wpctl set-volume @DEFAULT_AUDIO_SOURCE@ 100%")
elif f[0] > r[0] * 0.15:
    print("  Voice Focus is passing your voice. It is quieter than the raw")
    print("  microphone, which is the denoiser doing its job.")
else:
    print("  The microphone heard you and Voice Focus did not pass it on.")
    print("  The gate is most likely too aggressive for your voice. Lower it in")
    print("  /usr/share/studio-effects/voice-focus.conf:")
    print('      "VAD Threshold (%)" = 20.0')
    print("  then: systemctl --user restart studio-effects-voice")
PY
