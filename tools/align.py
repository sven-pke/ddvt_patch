"""Frame-exact alignment of two versions of the same film.

Decodes a stretch of both files starting at the same timestamp, shrinks every
frame to a small grey thumbnail, and finds the shift that makes the two
sequences match. Both files run on the same 23.976 grid from 0, so the shift
is the content offset in frames: positive means the reference is AHEAD, i.e.
reference frame (i + d) shows what target frame i shows.
"""
import subprocess, sys
import numpy as np

W, H = 32, 18
FRAME_NS = 1000000000 * 1001 / 24000.0


def thumbs(path, start_s, n, crop=None):
    vf = []
    if crop:
        vf.append(f"crop={crop}")
    vf.append(f"scale={W}:{H}")
    vf.append("format=gray")
    cmd = [
        "ffmpeg", "-v", "error", "-nostdin",
        "-ss", f"{start_s:.3f}", "-i", path,
        "-map", "0:v:0", "-frames:v", str(n),
        "-vf", ",".join(vf), "-f", "rawvideo", "-",
    ]
    out = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout
    got = len(out) // (W * H)
    a = np.frombuffer(out[: got * W * H], dtype=np.uint8).reshape(got, W * H).astype(np.float64)
    # Each frame on its own scale: content shape, not brightness.
    a -= a.mean(axis=1, keepdims=True)
    norm = np.linalg.norm(a, axis=1, keepdims=True)
    norm[norm == 0] = 1
    return a / norm


def best_shift(target, reference, span):
    """Correlate: how far into `reference` does `target` sit?"""
    n = min(len(target), len(reference)) - 2 * span
    if n < 20:
        return None
    scores = []
    for d in range(-span, span + 1):
        t = target[span : span + n]
        r = reference[span + d : span + d + n]
        scores.append((float((t * r).sum() / n), d))
    scores.sort(reverse=True)
    best, second = scores[0], scores[1]
    # Second best that is not a neighbour of the best, for the margin.
    other = next((s for s in scores[1:] if abs(s[1] - best[1]) > 2), second)
    return best[1], best[0], other[0]


if __name__ == "__main__":
    target_file, ref_file = sys.argv[1], sys.argv[2]
    crop = sys.argv[3] if len(sys.argv) > 3 else None
    span = 60
    n = 400 + 2 * span
    print(f"target    {target_file}")
    print(f"reference {ref_file}")
    print(f"crop on target: {crop or 'none'}; {W}x{H} thumbnails, +-{span} frames searched\n")
    print(f"{'at':>9}  {'shift':>6}  {'match':>6}  {'next best':>9}  verdict")
    for t in (60, 600, 1800, 2880, 4200, 5400, 5700):
        a = thumbs(target_file, t, n, crop)
        b = thumbs(ref_file, t, n)
        r = best_shift(a, b, span)
        if r is None:
            print(f"{t:>7} s  too few frames")
            continue
        d, score, other = r
        verdict = "solid" if score > 0.9 and score - other > 0.2 else "weak"
        print(f"{t:>7} s  {d:>+6}  {score:>6.3f}  {other:>9.3f}  {verdict}")
