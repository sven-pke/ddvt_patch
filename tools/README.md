# Checking the tool with something other than itself

A tool that says its own output is right has said nothing. These
scripts were written to check it from the outside, and each one caught
something real. They need Python with numpy, and ffmpeg on the PATH.

## `obu_compare.py FILE_A FILE_B`

Walks the AV1 temporal units of two Matroska files, drops every metadata
OBU, and compares what is left. A rewrite that only puts metadata in must
come out byte-identical here - on the 138 678 frames of the acceptance file
it did.

Walk the OBUs properly (header, leb128 size, payload). Splitting the stream
on the temporal-delimiter byte pattern `12 00` looks tempting and is wrong:
that pair occurs inside RPU payloads.

## `align.py TARGET REFERENCE [CROP]`

Says which frame of the reference lines up with frame 0 of the target, by
correlating the two files' frame-to-frame change curves at several spots.
`CROP` is an ffmpeg crop for the target, to compare like with like when one
file has letterbox bars: `3840:1606:0:277`.

This is what `ddvt_patch align` now does in Rust. Keeping the Python is the
point: an independent implementation that can disagree.
