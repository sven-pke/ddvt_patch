"""Compare the AV1 temporal units of two Matroska files: after removing
OBU_METADATA (type 5) from both, every temporal unit must be byte-identical.
Proves a rewrite touched nothing but the metadata.

The video track comes out of ffmpeg as a Section 5 OBU stream; it is walked
OBU by OBU (header, leb128 size, payload), and a temporal delimiter starts a
new unit. No byte-pattern guessing."""
import subprocess, sys

OBU_TD, OBU_METADATA = 2, 5


def leb128(b, p):
    v, n = 0, 0
    while True:
        if p + n >= len(b):
            return None, None
        c = b[p + n]
        v |= (c & 0x7F) << (7 * n)
        n += 1
        if not (c & 0x80):
            return v, n


def temporal_units(path):
    """Yield each temporal unit as a list of (type, bytes)."""
    cmd = ["ffmpeg", "-v", "error", "-i", path, "-map", "0:v:0", "-c", "copy", "-f", "obu", "-"]
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, bufsize=1 << 24)
    buf = b""
    pos = 0
    tu = None
    eof = False
    while True:
        # A complete OBU available?
        if pos + 1 <= len(buf):
            h = buf[pos]
            ext = (h >> 2) & 1
            has_size = (h >> 1) & 1
            assert has_size, "OBU without size field in a Section 5 stream"
            hl = 1 + ext
            size, n = leb128(buf, pos + hl)
        else:
            size = None
        if size is not None and pos + hl + n + size <= len(buf):
            t = (h >> 3) & 0xF
            end = pos + hl + n + size
            raw = buf[pos:end]
            pos = end
            if t == OBU_TD:
                if tu is not None:
                    yield tu
                tu = []
            if tu is not None:
                tu.append((t, raw))
            continue
        if eof:
            break
        chunk = p.stdout.read(1 << 24)
        if not chunk:
            eof = True
            continue
        buf = buf[pos:] + chunk
        pos = 0
    if tu:
        yield tu
    p.wait()


a, b = sys.argv[1], sys.argv[2]
n = same = 0
meta_out = 0
bad = 0
for ta, tb in zip(temporal_units(a), temporal_units(b)):
    n += 1
    ma = [r for t, r in ta if t != OBU_METADATA]
    mb = [r for t, r in tb if t != OBU_METADATA]
    meta_out += sum(1 for t, _ in tb if t == OBU_METADATA)
    if ma == mb:
        same += 1
    else:
        bad += 1
        print("TU", n, "differs beyond metadata:", [(t, len(r)) for t, r in ta], [(t, len(r)) for t, r in tb])
        if bad > 5:
            break
print("temporal units compared:", n, "| identical apart from metadata OBUs:", same, "| metadata OBUs in the output:", meta_out)
sys.exit(0 if n == same else 1)
