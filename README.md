# ddvt_patch

HDR metadata for large Matroska files - Dolby Vision, HDR10+, static HDR10,
film grain, the frame rate - without the file being rewritten seven times.

Two facts shape the tool. A change that keeps every byte length can be written
straight into the file where it lies, in milliseconds, whatever the file
weighs. A change that adds bytes to every frame cannot, and the file has to be
rewritten - but once, not once per tool. The commands follow that line.

```
ddvt_patch align    <file> --from <reference>
ddvt_patch auto     <file> --from <reference> [-o <out>] [--fps <rate>] [--dry-run]
                    [--grain-iso N [--grain-chroma]] [--frame-offset N] [--no-align]
                    [--no-rpu] [--no-hdr10plus]
ddvt_patch inspect  <file> [--scan MiB] [--json]
ddvt_patch patch    <file> [values] [--from <reference>] [--dry-run]
ddvt_patch restore  <file>
ddvt_patch retime   <file> --fps <rate> [--dry-run]
ddvt_patch extract  <file> [--rpu FILE] [--hdr10plus FILE]
ddvt_patch rewrite  <file> -o <out> [values] [--from <reference> [--only-missing]]
                    [--rpu FILE.bin|FILE.xml] [--hdr10plus FILE] [--frame-offset N]
                    [--active-area L,R,T,B | --keep-active-area]
                    [--grain-iso N [--grain-chroma]] [--drop-rpu] [--drop-hdr10plus]
                    [--drop-grain] [--container auto|keep|drop] [--dv-compat-id N]
                    [--fps <rate>] [--no-rpu] [--no-hdr10plus]
```

AV1 and HEVC, in Matroska. No demuxing, no temporary streams.

## What it will not do to your files

A tool that rewrites 20 GB files has to be careful about which file is
which, so three things are ruled out before anything is written:

- **Writing over what it is reading.** The output is refused when it names
  the input, the reference, an RPU file or an HDR10+ JSON that the same run
  reads. Two paths are the same file whether they differ in case, in the
  slash, or by a folder step that cancels out (`E:\film.mkv`, `e:/FILM.MKV`,
  `E:\.\film.mkv`), and the check asks the file system rather than compares
  strings. The rewriter itself refuses it too, so no path into it is exempt.
- **Leaving something that looks finished but is not.** A rewrite builds its
  result as `NAME.mkv.ddvtpart` and gives it the real name only when it is
  whole. A run that is stopped or killed halfway leaves the part file, never
  a Matroska file that would open, play, and end early. In the window a
  stopped run says so and offers to delete what it left.
- **Filling the disk halfway through.** The free space where the output goes
  is checked against the size of the source before the pass starts.

## Everything at once: `auto`

```
ddvt_patch auto film.av1.mkv --from "\\nas\films\Film (2023)\Film (2023).mkv" --dry-run
ddvt_patch auto film.av1.mkv --from "\\nas\films\Film (2023)\Film (2023).mkv"
```

Open the file, point at the reference - the master, or the source the file
was made from - and let the tool work out the rest. `auto` looks at both
files and prints what it finds: what each carries (Dolby Vision, HDR10+,
MaxCLL/MaxFALL, mastering display), the frame rate each states and the grid
its timestamps actually follow, and how many frames each has. Then the plan:
each kind of metadata the file lacks and the reference has is taken from the
reference, lined up frame by frame with it (see `align` below), with the
Dolby Vision active area fitted to the black bars measured in the file's own
pixels, and the file is put on the reference's frame rate when its own is
wrong, missing, or right in the head but not in the timestamps. `--dry-run`
stops there; without it the plan is carried out in one rewrite, with the
result verified afterwards. What the file already has stays as it is; what
neither file has is not invented. When nothing is off, nothing is written.

The reads it takes before deciding are small - the first 64 MiB of each
file, the Cues, and the Clusters after the last Cue point - so the analysis
of a 6.8 GB encode against an 18.9 GB master on a NAS takes about three
seconds. The frame counts come from the first and last frame on the grid the
timestamps follow, not from the duration in the head, which is on whatever
grid the muxer believed in at the time. A difference between the two counts
is reported loudly before anything is written, together with the
`--frame-offset` to give if it sits at the start.

`--fps` names the rate outright when the reference is not to be trusted;
`--grain-iso` adds photon noise on the way; `-o` names the output, which is
otherwise `NAME.fixed.mkv` next to the input. `--frame-offset N` given
outright is taken as given and nothing is measured; `--no-align` skips the
measurement and takes the two cuts to start together.

`--no-rpu` and `--no-hdr10plus` leave one kind out and take everything else:
a file that is to keep its own grade but still wants the reference's HDR10+,
or the other way round. They are not `--drop-rpu` and `--drop-hdr10plus`,
which remove what the file already carries; these two only say what not to
take from the reference. With both of them the reference is never read for
per-frame data at all, so nothing is measured and nothing is lined up - only
the static values and the frame rate are left to fix. The same two switches
work on `rewrite --from`, and the window has them as ticks in both modes.

## Which frame is which: `align`

```
ddvt_patch align film.av1.mkv --from master.hevc.mkv
```

Two files of the same film from different sources rarely start on the same
frame: one keeps a logo the other cut, one ends two frames later. Per-frame
metadata taken from such a reference lands on the wrong pictures unless the
difference is known, and the frame counts alone cannot say whether it sits at
the start, the end, or both - only that it exists.

The pictures can say it. Both files are decoded to small grey thumbnails at
six spots spread over the film, each frame's change from the one before it is
measured, and the two curves are correlated. The spots are frame numbers, not
seconds: a file whose timestamps run on another grid than the reference - the
very thing `auto` is there to repair - holds the same picture at another
time, and seeking both to the same second would make the measured offset grow
through the film and turn a frame-rate fault into a shift that is not there. Scene cuts dominate that curve,
and a cut is frame-exact, so the correlation peaks sharply at the true offset
and nowhere else. On the pair this was built for it reads 1.000 at the right
shift and 0.06 one frame beside it. Every spot must agree: if the offset at
one spot differs from another, a stretch is missing or added in the middle,
no single number can put that right, and the tool says so instead of writing
something plausible. A spot whose curves match well enough to be believed has
a vote even when its peak is not sharp enough to count on its own - one clear
spot alone cannot carry a number the others contradict.

`auto` runs this itself whenever it takes per-frame metadata, and applies
what it finds to the RPUs and the HDR10+ alike. When it cannot measure - no
ffmpeg, or the two cuts do not line up by one offset - it refuses rather than
fall back to zero, because per-frame metadata on the wrong pictures is worse
than none.

A real case: an AV1 encode of 138 678 frames against a master of 138 704.
The difference is 26 frames, but not where a guess would put it: the master
has 24 frames more at the start and 2 at the end. Taken with the frame offset
left at 0, every RPU and every HDR10+ unit in the result sat 24 frames - one
second - from the picture it was graded for.

Two checks say the result is right. The metadata carries scene marks, and
every one of the 1 891 marks in the output is exactly a master mark less the
offset - the whole list, not a sample; the one master mark that falls in the
24 frames cut off the front is the only one missing. And those marks land on
the film's own cuts: of the 18 real scene cuts in the first 3 000 frames, all
18 carry a mark at offset 24 and none does at any offset from 20 to 28.
(A picture detector also flags four brightness jumps inside the unbroken
opening shot, which the grade quite rightly marks as nothing at all - the
master has no mark anywhere between frames 592 and 1 369.)

## The window

`ddvt_patch` on its own, or `ddvt_patch gui <file>`, opens a window with
the file at the top (Open…, or drop one anywhere on the window), what it
says on the left, the log at the bottom, and two ways of working:

- **Autopilot** is `auto`: the reference the file was made from, where the
  fixed file goes, the frame rate (the reference's, as a rule), and two
  buttons - "1 Analyse" compares the two files and writes nothing, "2 GO fix"
  carries the plan out. Grain, frame offset and the like sit folded under
  More.
- **Manual** is every option the commands have in one form: where to write
  (a new file, or into the file itself), the per-frame metadata to take
  (reference, RPU file, HDR10+ JSON), the static values, the grain, the
  frame rate, what to remove, and folded groups for the rarer things. The
  window works out which command that amounts to: written into the file it
  is `patch` (static values) or `retime` (frame rate), each undoable and
  with a Dry run switch next to GO; written to a new file it is `rewrite`
  with everything at once. An Extract button in the export group writes the
  RPU file and the HDR10+ JSON.

A sentence under the buttons says in plain words what is about to happen,
and they stay grey with the reason while something is missing. Writing into
the file, or over an existing output, asks first; when a .ddvtbak lies next
to the file, an Undo button restores it. Options the chosen way of writing
cannot act on are greyed out rather than hidden - an in-place change carries
the static values or the frame rate, and a tick that quietly does nothing
would be worse than one that is visibly idle. Every path has an Open… or Save
to… button with the Windows dialog behind it, everything explains itself
when the mouse rests on it, and every run starts with its command line in
the log, with a button to copy it. The console the window was started from
is let go, and the commands it runs open none.

## The terminal screen

`ddvt_patch ui <file>` opens the same commands
as a screen in the terminal: the file at the top, what it says on the left,
the action and its options on the right, the log at the bottom. Tab moves
between fields, arrows change a choice, Enter on a path opens a file browser
(drives, folders, only the files that fit the field), F5 runs, F9 stops,
F10 quits. The mouse works too: click a tab, a field, a toggle or Run, click
a path field twice for the browser, wheel over the report, the log or the
browser list. Patch starts as a dry run; untick it to write.

Every run is a real command line. It is printed first in the log, so what
the screen does can be repeated in a shell, put in a script, or pasted into
a bug report. The progress bar is the same one the command prints.

## What the file says: `inspect`

Reads the head (a few kilobytes) and the first 64 MiB of video, or all of it
with `--scan 0`. Reports the container's Colour element, the colour the
HEVC sequence parameter set states, the AV1 sequence header from
CodecPrivate - including whether a grain table is present - whether Dolby
Vision is signalled and how, and what the bitstream carries: mastering
display and content light level units with their values and how many copies,
Dolby Vision RPUs, HDR10+ units. When the Dolby Vision record does not fit
the RPUs and the picture - none at all over RPUs, or HDR10 compatible over an
HLG picture - it says so.

The container and the bitstream both hold the static values and they can
disagree. A decoder reads the bitstream. `inspect` shows both.

## Correcting static values in place: `patch`

MaxCLL, MaxFALL and the mastering display are fixed-width fields in the
container and in the bitstream alike, so a new value is a seek and a write.
Every copy in the stream gets it. 6.29 GiB of AV1: about 15 seconds, 14 of
which are reading the file to find the units.

A unit that is missing cannot be overwritten; it is spliced into the first
temporal unit and the bytes are taken from the `Void` padding every muxer
leaves in the head, so no Cluster after the first moves. That one lands in
the first temporal unit only - a player seeking into the middle does not see
it. If it has to be at every keyframe, that is a `rewrite`.

```
ddvt_patch patch film.mkv --max-cll 1768 --max-fall 505 --luminance-min 0.005
ddvt_patch patch film.mkv --from "\\nas\films\master.mkv"
```

`--from` reads a reference and fills in what was not given explicitly, from
its bitstream first. Explicit values always win.

Every patch writes `<file>.ddvtbak` with the original bytes of each region it
replaced; `restore` puts them back, verified byte for byte. The file is read
back after writing and the values checked before the command reports success.

### One source, not two

With a value in both the container and the stream, MediaInfo reports the
second copy as a separate `_Original` field. The reference grades keep these
numbers in the stream only, and that is the shape `--container auto`
(the default) produces: once the stream carries a value, the container copy
is removed. `keep` writes both, `drop` always removes the container copy.

## A wrong or missing frame rate: `retime`

```
ddvt_patch retime film.mkv --fps 23.976
```

An AV1 bitstream carries no frame rate - SVT-AV1 writes no timing info into
the sequence header, HEVC keeps its in the SPS - so in Matroska the rate is
the container's word alone: DefaultDuration on the track and the timestamps
that follow it. A mux through IVF loses that word. ffmpeg writes its time
base into the IVF header ("rate 1000, scale 1"), mkvmerge reads that as the
frame rate, and the file ends up with no DefaultDuration at all, which every
player then guesses (ffprobe says 25), or with one estimated from the
timestamps, which are stored in whole milliseconds: 23.953 fps instead of
23.976. Such a file drifts against its audio by a millisecond every second,
a frame every 43 seconds, seven seconds over a two-hour film, and the value
survives every further remux. `inspect` shows it in
its Timing section: the stated rate against the standard ones, whether the
bitstream states any, and how the timestamps sit on the grid.

`retime` puts the file on the rate given, in place: the track's
DefaultDuration (added when there was none, paid from the head's Void),
every Cluster timestamp, every block's two-byte offset, the video Cue times
and the duration. Audio and subtitle timestamps stay where they are. The
timestamps are mapped frame by frame from the grid they follow, so a rounded
timestamp still lands on its own frame. Every byte is fixed width, so nothing
moves; `.ddvtbak` and `restore` work as for `patch`, and the file is read
back afterwards to confirm every frame sits on the new grid. Clusters
carrying a CRC-32 (ffmpeg muxes) are refused, because the checksum would go
stale; those take `rewrite --fps`, which does the same mapping while it
copies the file and drops the checksums.

The grid the timestamps follow is measured, never taken from the head: the
first 64 MiB of video give a few thousand frames, and the span over the
steps gives the frame interval to within a thousandth of a millisecond, with
a gap or a doubled frame left out. That matters because the two disagree in
exactly the files this is for. A file whose DefaultDuration was corrected
afterwards - by mkvpropedit, or a remux told the right rate - while the
timestamps stayed on the 23.953 grid states the right rate and is still
wrong; `inspect`, `retime`, `rewrite --fps` and
`auto` all say so ("the timestamps follow 23.953 fps although the track
states 23.976") and put it right, and a file whose timestamps are on the
right grid under a wrong or missing rate gets only its head fixed.

## Pulling metadata out: `extract`

```
ddvt_patch extract master.mkv --rpu master.bin --hdr10plus master.json
```

One read for both. The RPU file is dovi_tool's format, the JSON is
hdr10plus_tool's, produced with their libraries; against the same file they
match the originals byte for byte and entry for entry. Both are in
presentation order, which is what those tools write and what `rewrite` reads.

## Everything per frame, in one pass: `rewrite`

```
ddvt_patch rewrite in.mkv -o out.mkv \
    --rpu master.bin --hdr10plus master.json \
    --max-cll 1768 --max-fall 505 --luminance-min 0.005 \
    --grain-iso 100
```

One read of the source, one write of the target. Each temporal unit is
handled in a fixed order: what is being replaced comes out; the frame headers
are rewritten for grain; the new units go in - behind the sequence header and
ahead of the picture for AV1, prefix SEI ahead of the first slice and the RPU
last for HEVC, the way encoders and dovi_tool lay them out. Static values go
to every keyframe. The Cues are rebuilt as the Clusters are written; audio,
subtitles, chapters, tags and attachments are copied through.

A 6.75 GB AV1 film with 138 678 frames takes 12 seconds on a local drive,
which is the drive's speed. The 18.9 GB HEVC master of the same film, read
over SMB from a NAS, took 7.4 minutes - the network's speed - with its 2 388
keyframes rewritten and its RPUs and HDR10+ carried through untouched.

What the options do:

- `--rpu FILE` injects one Dolby Vision RPU per frame and signals Dolby Vision
  in the container: a configuration record with profile 10 for AV1 or the
  RPU's own profile for HEVC (`dvcC` up to 7, `dvvC` from 8), the level from
  the pixel rate, BL+RPU, and the compatibility id - the number behind the
  dot, what a player without Dolby Vision makes of the picture. It is worked
  out from the file: profile 5 RPUs go with a picture only Dolby Vision can
  show, 0 (5, 10); every other profile rides on a plain picture, and the
  transfer the picture states - in the AV1 sequence header or the HEVC
  sequence parameter set, else the container - says which: PQ 1 (8.1, 10.1),
  HLG 4 (8.4, 10.4), SDR 2 (8.2, 10.2). A picture that states nothing is
  taken as HDR10, and the report says it was a guess. `--dv-compat-id` sets
  the id outright; one that contradicts the file is used, and the report
  says what contradicts it. Without the record MediaInfo and players see
  plain HDR10 no matter how many RPUs the stream holds; with it they report
  `dav1.10` or `dvhe.08`. Any RPUs already in the stream are replaced.
  `--drop-rpu` removes them and the record.
- Profile 7 RPUs describe two layers, and everything this tool writes has
  one - in AV1 profile 7 is never valid, as profile 10 has no enhancement
  layer. They are made 8.1 on the way in, the way `dovi_tool -m 2` does it.
  Profile 5 RPUs go in as they are, with a note: they belong with the IPT
  picture they were made for; for an HDR10 picture they have to be made 8.1
  first (`dovi_tool -m 3`).
- `--rpu FILE.xml` takes a Dolby Vision metadata XML instead - the file a
  grading house or DaVinci Resolve hands out, shots and trims rather than
  units - and makes the RPUs out of it on the way in, for this file's picture.
  The canvas the active area needs is the video's own width and height, so
  nothing has to be typed; the old chain called `dovi_tool generate --xml`
  with the same two numbers, and the RPUs that come out here are byte for
  byte the ones it produced. The picture's transfer decides the mapping, as
  `-p` does for dovi_tool: a PQ picture gets 8.1 RPUs, an HLG picture 8.4.
  An SDR picture is refused - no generator makes the mapping it would need.
  An XML with more frames than the stream is fine,
  the same way an RPU file with more units is - the surplus is taken to sit
  at the end, and a line says so before anything is written, because an XML
  carries no picture to measure the difference against the way a reference
  film does. The `EditRate` the XML was graded at is read too and compared
  with the film's rate, since shots in an XML are frame numbers and cannot be
  stretched to fit another rate. A document that is not well-formed XML is
  refused with a sentence: the library behind this parses with an unwrap, so
  the panic is caught here rather than passed on as a crash.
- `--hdr10plus FILE` injects one HDR10+ unit per frame from the JSON; existing
  ones are replaced. `--drop-hdr10plus` removes them. A JSON with more entries
  than the stream has frames is fine (the rest are unused); fewer is an error.
- `--drop-alien-blocks` leaves out blocks in the video track that hold no
  video. A remux that removes a track but not its data leaves its blocks
  behind under the video's own number - zlib-compressed subtitles, for one.
  They decode to nothing, they sit on the timestamps of real pictures, and
  every one of them puts the frame count out against the list of metadata.
  Without the switch such a file is refused with what the block really is
  and where it sits, because dropping data is not something to do quietly.
  `mkvmerge -o clean.mkv broken.mkv` clears them out too, in seconds.
- `--grain-table FILE` puts a film grain table into the stream instead: the
  `filmgrn1` format aomenc writes, `grav1synth inspect` reads out of a film
  and `grav1synth diff` measures from a source and a denoised copy. Grain
  measured from a real picture carries autoregressive coefficients, which
  photon noise does not, and the report says which kind came in and what
  stretch it covers. A table that ends before the film does is a warning,
  not an error: the rest gets no grain. Byte for byte, the result is what
  `grav1synth apply --grain` writes.
- `--grain-iso N` replaces whatever grain table the stream has with photon
  noise at that ISO, luma only unless `--grain-chroma`. It is the same table
  `grav1synth generate --iso N` writes, from the same library and the same
  parser code: on a 3054-frame AV1 film and on a UHD PQ clip, the whole AV1
  elementary stream comes out byte for byte identical to grav1synth's, grain
  seeds and all. Which transfer function the pictures are on decides the
  shape of the curve - at ISO 400 a PQ picture gets 43 at black falling to 1
  at white, a BT.1886 one a nearly flat 7 - so it is read from the sequence
  header, and from the container when the sequence header carries no colour
  description at all. The old table has to
  come out first and does, in the same pass: `film_grain_params_present` is
  cleared or set in every sequence header including the one in CodecPrivate,
  and every frame header is rewritten. `--drop-grain` just removes it. AV1
  only; HEVC has no grain synthesis.
- The static values work as in `patch`, at every keyframe, resolved against
  what the first temporal unit says so a partial patch keeps the rest.

The metadata files are matched to frames in presentation order. AV1 temporal
units already come that way. HEVC blocks sit in decode order, so the
presentation position is recovered from the block timestamp and the frame
duration; two frames landing on the same index is reported, not guessed at.

`--fps RATE` puts the file on that frame rate on the way - the same change
`retime` makes in place, folded into the copy: the Cluster timestamps,
every block offset, the Cues as they are rebuilt, the duration and the
track's DefaultDuration. Audio and subtitles keep their times. This is the
path for files `retime` refuses (Clusters with a CRC-32) and for an HEVC
file that states no rate at all, whose presentation order could otherwise
not be recovered. The result is verified like a retime: the stated rate,
and every scanned frame on its grid.

### Taking it all from another file: `--from`

```
ddvt_patch rewrite film.av1.mkv -o film.dv.mkv --from master.hevc.mkv
```

`--from` reads the reference once and takes everything the command line did
not give: the static values, the Dolby Vision RPUs and the HDR10+ metadata.
`--rpu` or `--hdr10plus` on the command line win over the reference; `--drop-rpu`
or `--drop-hdr10plus` skip that part of it. No files land on disk in between;
the lists go from the reference into the target through the same code the
`extract` files would take, which is why they come out byte for byte the same.

What the target already carries is replaced, kind by kind: a reference with
RPUs replaces the target's RPUs, a reference without leaves them alone; the
same for HDR10+ and for the static values. `--only-missing` turns that
around: each kind the target's stream already has - RPUs, HDR10+,
MaxCLL/MaxFALL, the mastering display - stays as it is, and only the rest
comes from the reference. An AV1 encode that already carries Dolby Vision
but lost its HDR10+ gets the HDR10+ and nothing else.

Two things get fitted on the way, because a grade knows where the picture
sits and how many frames it has:

- **Active area.** The RPU level 5 says how far the picture is from each edge
  of the canvas. The pixels decide where that is: with ffmpeg at hand (next
  to `ddvt_patch.exe`, on the PATH, or named by `DDVT_FFMPEG`), five frames
  spread over the film are decoded to grey and the black rows and columns at
  the edges counted, the narrowest bar over the five winning so a dark scene
  cannot overstate one - the check the old chain ran as DetectBorders. Every
  level 5 is then moved so the grade's smallest offsets land on the measured
  bars, keeping whatever scene-to-scene offsets the grade had. A master
  cropped to 3840x1604 going into a 3840x2160 encode whose bars are 277
  lines gets 277, not the 278 a centred guess would give. Without ffmpeg the
  reference picture is assumed centred in the target and the offsets moved by
  half the difference, and the log says so. A move that would go past an
  edge is an error, not a clamp; bars more than 8 px from the centred guess
  are reported loudly, since the two pictures then do not fit the way they
  should. `--active-area L,R,T,B` sets the offsets outright instead (also
  for an RPU file); `--keep-active-area` leaves them alone. `inspect` shows
  the level 5 an RPU carries next to the bars it measures.
- **Frame offset.** `--frame-offset N` says that reference frame N is the
  target's frame 0, for the RPUs and the HDR10+ alike. Positive drops leading
  reference frames; negative repeats the first one, for a target that starts
  before the reference does. Entries past the end of the target are ignored,
  as with a file; fewer than the target has frames is an error. `align`
  measures N, and `auto` applies it on its own.

Against dovi_tool's editor doing the same edit from a JSON, the RPUs that
come out are byte for byte identical.

## The case this was built for

An AV1 encode of a film whose Dolby Vision and HDR10+ grade exists in an HEVC
master of a different geometry: the master is 3840x1604 (letterbox cropped),
the AV1 is 3840x2160 with the bars in the picture, and the two are 26 frames
apart in length. The metadata is right; it has to be moved over and told
about the new canvas.

One command, one read of the master, one write of the result:

    ddvt_patch rewrite film.av1.mkv -o film.dv.mkv --from "\\nas\films\Film (2023)\Film (2023).mkv" --grain-iso 100

The master's RPUs and HDR10+ go in per frame, its MaxCLL, MaxFALL and
mastering display at every keyframe, the level 5 active area is moved onto
the bars measured in the encode's pixels - 277 lines top and bottom here,
where the centred guess from the canvases would have said 278 - and the AV1
gets its `dvvC` record and photon noise. The 26 frames the master
has and the encode does not are left over at the end and reported. On the
real files this took 9.4 minutes, 9 of them reading the 18.9 GB master over
the network and 16 seconds writing the 6.8 GB result.

The two cuts do not start together - the master has 24 frames more at the
start and 2 at the end - and `auto` measures that itself before it writes
anything (see `align`). A stretch missing in the middle is a different matter
and no single offset can fix it; that is what `extract` and dovi_tool's
editor are for.

`ddvt_patch inspect film.dv.mkv --scan 0` shows it all in place.

## Where the numbers live

| | AV1 | HEVC |
|---|---|---|
| static HDR | metadata OBUs, HDR_MDCV (2) and HDR_CLL (1) | prefix SEI 137 and 144 |
| chromaticity | /65536 | /50000 |
| luminance max | 24.8 fixed point | 1/10000 cd/m2 |
| luminance min | 18.14 fixed point | 1/10000 cd/m2 |
| primary order | R, G, B | G, B, R |
| Dolby Vision RPU | ITU-T T.35 OBU, 0xB5 / 0x003B, EMDF wrapped | NAL type 62, emulation prevention bytes |
| HDR10+ | ITU-T T.35 OBU, 0xB5 / 0x003C | prefix SEI 4, 0xB5 / 0x003C |
| container signalling | BlockAdditionMapping `dvvC`, profile 10 | `dvvC` (profile 8+) or `dvcC` |

Profile 8.1 to 10.1 is not a conversion of the RPU: the payload is the same,
the carriage and the profile field differ. That is what `rewrite` does on the
way in.

## Limits

- Per-frame metadata cannot be patched in place, by construction.
- `patch` inserts missing units for AV1 only; for HEVC it corrects units that
  exist and reports payloads it cannot overwrite because an emulation
  prevention byte sits inside them.
- Grain synthesis is AV1 only. Streams with stand-alone `OBU_TILE_GROUP` are
  refused rather than guessed at.
- Video blocks with lacing, Clusters of unknown size and tracks with
  `ContentEncodings` are refused with a message.
- CRC-32 elements, which ffmpeg writes into Info, Tracks and every Cluster:
  `patch` refuses a head or first Cluster that carries one, because an
  in-place write would leave it stale. `rewrite` drops them from what it
  rebuilds (Tracks, Clusters, Cues) and copies the rest, whose checksums stay
  valid.
- HEVC leading pictures with timestamps before the first decoded frame are
  not handled.

## Building

```
cargo build --release
cargo test
```

`dolby_vision` and `hdr10plus` come from the AV1-capable forks of dovi_tool
and hdr10plus_tool ([sven-pke/dovi_tool](https://github.com/sven-pke/dovi_tool),
[sven-pke/hdr10plus_tool](https://github.com/sven-pke/hdr10plus_tool)), pinned
to a commit in `Cargo.toml`. The
frame header parser under `src/grain` is grav1synth's (MIT, Josh Holmer) with
the two corrections from its `feat/fix-grain-rewrite` branch, cut loose from
ffmpeg. Photon noise comes from `av1-grain`. The EBML, OBU and NAL handling is
in this crate.
