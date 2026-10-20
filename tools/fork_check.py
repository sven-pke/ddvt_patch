"""Check dovi_tool and hdr10plus_tool on AV1 from the outside.

Every command against every kind of input, judged by tools that did not
write the output:

- FFmpeg's CBS parser (`-bsf:v trace_headers`) reads every OBU the way a
  strict decoder does. A metadata OBU without trailing_bits() fails here;
  that is what JORDZIE saw as "Malformed ITU-T T.35" in dav1d.
- The OBU order of every temporal unit: metadata after the sequence
  header, before the first frame. Metadata ahead of the sequence header is
  copied into av1C by mkvmerge.
- Round trips: what goes in comes back out, and remove gives back the
  stream that went in.

The validator is checked too: a stream with the trailing byte taken off
must fail it, or its silence means nothing.

    python fork_check.py HDR10PLUS_TOOL.exe DOVI_TOOL.exe WORKDIR

WORKDIR holds base.obu, base.ivf, base.mkv (AV1 without metadata),
hp.json and rpu.bin for the same number of frames.
"""

import hashlib
import json
import os
import subprocess
import sys

FF = os.environ.get("FFMPEG", "ffmpeg")
H, D, W = sys.argv[1], sys.argv[2], sys.argv[3]
os.chdir(W)

results = []


def run(args, stdin=None):
    p = subprocess.run(args, capture_output=True, stdin=stdin)
    return p.returncode, p.stdout.decode("utf-8", "replace") + p.stderr.decode("utf-8", "replace")


def check(name, ok, detail=""):
    results.append((name, ok, detail))
    print(("  ok   " if ok else "  FAIL ") + name + ("" if ok else "   " + detail[:300]))


def leb(b, p):
    v = n = 0
    while True:
        x = b[p + n]
        v |= (x & 0x7F) << (7 * n)
        n += 1
        if not x & 0x80:
            return v, n


def enc_leb(v):
    out = bytearray()
    while True:
        x = v & 0x7F
        v >>= 7
        out.append(x | (0x80 if v else 0))
        if not v:
            return bytes(out)


def obus(data):
    """(type, body) for every OBU of a Section 5 stream."""
    p = 0
    out = []
    while p < len(data):
        h = data[p]
        t = (h >> 3) & 0xF
        ext = 1 if h & 4 else 0
        size, n = leb(data, p + 1 + ext)
        s = p + 1 + ext + n
        out.append((t, data[s : s + size]))
        p = s + size
    return out


def kind(t, body):
    if t == 5 and body[:2] == b"\x04\xb5":
        return {b"\x00\x3c": "HP", b"\x00\x3b": "RPU"}.get(body[2:4], "T35")
    return {1: "SEQ", 2: "TD", 3: "FH", 4: "TILE", 5: "META", 6: "FRAME"}.get(t, str(t))


def container(path):
    head = open(path, "rb").read(4)
    if head == b"DKIF":
        return "ivf"
    if head == b"\x1a\x45\xdf\xa3":
        return "mkv"
    return "obu"


def as_obu(path):
    """Any AV1 file as a Section 5 OBU stream, via FFmpeg. Judged by its
    first bytes, never its name: a tool keeps the container it was given."""
    if container(path) == "obu":
        return open(path, "rb").read()
    tmp = path + ".norm.obu"
    run([FF, "-v", "error", "-y", "-i", path, "-map", "0:v:0", "-c", "copy", "-f", "obu", tmp])
    data = open(tmp, "rb").read()
    os.remove(tmp)
    return data


def strict(path):
    code, out = run([FF, "-v", "error", "-i", path, "-map", "0:v:0", "-c", "copy",
                     "-bsf:v", "trace_headers", "-f", "null", "-"])
    bad = [l for l in out.splitlines() if l.strip()]
    return code == 0 and not bad, "\n".join(bad[:4])


def order_ok(data, want):
    """Every temporal unit: `want` metadata present once, after SEQ, before FRAME."""
    tus = []
    for t, b in obus(data):
        k = kind(t, b)
        if k == "TD":
            tus.append([])
        if tus:
            tus[-1].append(k)
    # A check that finds nothing to check has not passed.
    if len(tus) != n_frames:
        return False, f"{len(tus)} temporal units found, {n_frames} expected"
    for i, tu in enumerate(tus):
        for w in want:
            if tu.count(w) != 1:
                return False, f"TU {i}: {tu.count(w)}x {w}: {tu}"
            at = tu.index(w)
            if "SEQ" in tu and tu.index("SEQ") > at:
                return False, f"TU {i}: {w} ahead of the sequence header: {tu}"
            first_pic = min([tu.index(k) for k in ("FRAME", "FH") if k in tu] or [len(tu)])
            if at > first_pic:
                return False, f"TU {i}: {w} behind the picture: {tu}"
    return True, f"{len(tus)} TUs"


def md5(b):
    return hashlib.md5(b).hexdigest()


base = open("base.obu", "rb").read()
base_md5 = md5(base)
n_frames = sum(1 for t, b in obus(base) if t == 2)
print(f"base: {n_frames} temporal units")

# ---- the validator itself --------------------------------------------------
print("\nValidator")
ok, _ = strict("base.obu")
check("clean stream passes trace_headers", ok)
# One HDR10+ unit written the broken way, by hand.
code, _ = run([H, "inject", "-i", "base.obu", "-j", "hp.json", "-o", "v_good.obu"])
good = open("v_good.obu", "rb").read()
bad = bytearray()
p = 0
while p < len(good):
    h = good[p]
    t = (h >> 3) & 0xF
    ext = 1 if h & 4 else 0
    size, n = leb(good, p + 1 + ext)
    s = p + 1 + ext + n
    body = good[s : s + size]
    if kind(t, body) == "HP" and body[-1:] == b"\x80":
        body = body[:-1]
    bad += good[p : p + 1 + ext] + enc_leb(len(body)) + body
    p = s + size
open("v_bad.obu", "wb").write(bad)
ok, _ = strict("v_bad.obu")
check("a metadata OBU without trailing_bits() FAILS trace_headers", not ok)

# ---- each tool, each input -------------------------------------------------
json_in = json.load(open("hp.json"))
rpu_in = open("rpu.bin", "rb").read()

tools = [
    ("hdr10plus_tool", H, ["inject", "-j", "hp.json"], "HP"),
    ("dovi_tool", D, ["inject-rpu", "--rpu-in", "rpu.bin"], "RPU"),
]
for name, exe, inject, tag in tools:
    for src in ("base.obu", "base.ivf", "base.mkv", "stdin"):
        label = f"{name} inject <- {src}"
        out = f"o_{name}_{src.replace('.', '_')}.obu"
        if src == "stdin":
            with open("base.obu", "rb") as f:
                code, log = run([exe, inject[0], "-", "-o", out] + inject[1:], stdin=f)
        else:
            code, log = run([exe, inject[0], "-i", src, "-o", out] + inject[1:])
        if src in ("base.mkv", "stdin"):
            # Injection needs a raw stream it can rewrite in place, for HEVC
            # upstream as much as here: refused, with a sentence, no crash.
            check(label + " is refused plainly",
                  code != 0 and "panicked" not in log and "AV1" in log, log)
            continue
        if code != 0 or not os.path.exists(out):
            check(label, False, log)
            continue
        print(f"\n{label}")
        ok, why = strict(out)
        check("strict parse", ok, why)
        data = as_obu(out)
        ok, why = order_ok(data, [tag])
        check("one unit per TU, after SEQ, before the picture", ok, why)
        units = [b for t, b in obus(data) if kind(t, b) == tag]
        ok = len(units) == n_frames and all(b[-1:] == b"\x80" for b in units)
        check("every injected OBU ends in trailing_bits()", ok, f"{len(units)} units")

        # Round trip
        if name == "hdr10plus_tool":
            run([exe, "extract", "-i", out, "-o", out + ".json"])
            back = json.load(open(out + ".json")) if os.path.exists(out + ".json") else {}
            same = back.get("SceneInfo") == json_in.get("SceneInfo")
            check("extract gives the JSON back", same)
        else:
            run([exe, "extract-rpu", "-i", out, "-o", out + ".bin"])
            back = open(out + ".bin", "rb").read() if os.path.exists(out + ".bin") else b""
            check("extract-rpu gives the RPUs back", md5(back) == md5(rpu_in),
                  f"{len(back)} vs {len(rpu_in)} bytes")
        rm = out + ".rm.obu"
        run([exe, "remove", "-i", out, "-o", rm])
        check("remove gives the input stream back",
              os.path.exists(rm) and md5(as_obu(rm)) == base_md5)
        if src != "stdin":
            check(f"output keeps the input's container ({container(src)})",
                  container(out) == container(src), container(out))
            if container(src) == container(rm):
                check("remove gives the input FILE back, byte for byte",
                      md5(open(rm, "rb").read()) == md5(open(src, "rb").read()))
        # Injecting over what is there replaces, never doubles.
        again = out + ".again.obu"
        run([exe, inject[0], "-i", out, "-o", again] + inject[1:])
        check("injecting twice is injecting once",
              os.path.exists(again) and md5(open(again, "rb").read()) == md5(open(out, "rb").read()))

# ---- a pipe, and nothing to find -------------------------------------------
print("\nPipes, and streams without the metadata")
run([H, "inject", "-i", "base.obu", "-j", "hp.json", "-o", "p_hp.obu"])
run([D, "inject-rpu", "-i", "base.obu", "--rpu-in", "rpu.bin", "-o", "p_dv.obu"])
for name, exe, cmd, src in [
    ("hdr10plus_tool", H, "remove", "p_hp.obu"),
    ("dovi_tool", D, "remove", "p_dv.obu"),
]:
    with open(src, "rb") as f:
        code, log = run([exe, cmd, "-", "-o", f"p_{name}_rm.obu"], stdin=f)
    ok = code == 0 and "panicked" not in log and os.path.exists(f"p_{name}_rm.obu")
    check(f"{name} remove from a pipe", ok and md5(open(f"p_{name}_rm.obu", "rb").read()) == base_md5, log)
with open("p_hp.obu", "rb") as f:
    code, log = run([H, "extract", "-", "-o", "p.json"], stdin=f)
check("hdr10plus_tool extract from a pipe",
      code == 0 and json.load(open("p.json")).get("SceneInfo") == json_in.get("SceneInfo"), log)
with open("p_dv.obu", "rb") as f:
    code, log = run([D, "extract-rpu", "-", "-o", "p.bin"], stdin=f)
check("dovi_tool extract-rpu from a pipe", code == 0 and md5(open("p.bin", "rb").read()) == md5(rpu_in), log)
for name, exe, cmd, out in [
    ("hdr10plus_tool", H, "extract", "none.json"),
    ("dovi_tool", D, "extract-rpu", "none.bin"),
]:
    if os.path.exists(out):
        os.remove(out)
    code, log = run([exe, cmd, "-i", "base.obu", "-o", out])
    # The HEVC path refuses here; an empty file with a success exit reads as
    # "extracted" to anyone scripting it.
    check(f"{name} {cmd} on a stream without it refuses",
          code != 0 and not os.path.exists(out), f"exit {code}: {log}")

# ---- both in one stream ----------------------------------------------------
print("\nBoth tools on one stream")
run([H, "inject", "-i", "base.obu", "-j", "hp.json", "-o", "both1.obu"])
run([D, "inject-rpu", "-i", "both1.obu", "--rpu-in", "rpu.bin", "-o", "both.obu"])
ok, why = strict("both.obu")
check("strict parse", ok, why)
ok, why = order_ok(open("both.obu", "rb").read(), ["HP", "RPU"])
check("HDR10+ and RPU each once per TU, placed right", ok, why)
run([D, "remove", "-i", "both.obu", "-o", "both_nodv.obu"])
run([H, "remove", "-i", "both_nodv.obu", "-o", "both_none.obu"])
check("removing both gives the input back", md5(open("both_none.obu", "rb").read()) == base_md5)

bad = [r for r in results if not r[1]]
print(f"\n{len(results) - len(bad)} of {len(results)} checks passed")
sys.exit(1 if bad else 0)
