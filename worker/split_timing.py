"""MP4 presentation timing used by the worker's stream-copy output checks."""

from __future__ import annotations

from dataclasses import dataclass
from fractions import Fraction
from functools import cached_property
import hashlib
from itertools import repeat
from pathlib import Path
import re
import struct


class TimingError(ValueError):
    """Timing or media layout is outside the supported proof."""


# Read metadata and, for AAC, only the few compressed access units that prove
# the source-to-edit mapping. Never read the movie payload into memory.
MAX_METADATA = 32 * 1024 * 1024
HASH_BLOCK_SIZE = 1024 * 1024
MAX_EXACT_EXPRESSION_INTEGER = 1 << 53  # FFmpeg setts uses a double evaluator.
AAC_RATE = 48000
AAC_FRAME = 1024


def require(condition, detail):
    if not condition:
        raise TimingError(detail)


def read_exact(handle, size):
    data = handle.read(size)
    require(len(data) == size, "Truncated MP4 data")
    return data


def box_size_and_type(header, remaining, read_large_size):
    size, kind = struct.unpack(">I4s", header)
    width = 8
    if size == 1:
        size, = struct.unpack(">Q", read_large_size())
        width = 16
    elif size == 0:
        size = remaining
    require(width <= size <= remaining, "Invalid MP4 box extent")
    return size, kind, width


def boxes(data, tag=None):
    cursor = 0
    while cursor < len(data):
        require(len(data) - cursor >= 8, "Truncated MP4 box header")
        size, kind, width = box_size_and_type(data[cursor:cursor + 8], len(data) - cursor,
                                             lambda: data[cursor + 8:cursor + 16])
        if tag is None or kind == tag:
            yield kind, data[cursor + width:cursor + size]
        cursor += size


def child(data, tag, optional=False):
    matches = iter(boxes(data, tag))
    found = next(matches, None)
    require(next(matches, None) is None and (found is not None or optional),
            f"Unexpected MP4 {tag.decode('ascii')} box count")
    return found[1] if found is not None else None


def version(data):
    require(data is not None and len(data) >= 4 and data[0] in (0, 1), "Unsupported MP4 box version")
    return data[0]


def clock(data):
    if version(data) == 0:
        scale, duration = struct.unpack_from(">II", data, 12)
    else:
        scale, duration = struct.unpack_from(">IQ", data, 20)
    require(scale > 0 and duration > 0, "Invalid MP4 clock")
    return scale, duration


def entries(data, fmt, *, start=8, count_at=4):
    count, = struct.unpack_from(">I", data, count_at)
    require(len(data) == start + struct.calcsize(fmt) * count, "Invalid MP4 sample table size")
    return struct.iter_unpack(fmt, data[start:])


@dataclass(frozen=True)
class Edit:
    duration: int
    media_time: int
    rate: int


@dataclass(frozen=True)
class Track:
    kind: bytes
    path: Path
    payloads: tuple[tuple[int, int], ...]
    movie_timescale: int
    timescale: int
    media_duration: int
    header_duration: int
    edits: tuple[Edit, ...]
    table: memoryview

    def simple_range(self) -> tuple[Fraction, Fraction]:
        if not self.edits:
            require(Fraction(self.header_duration, self.movie_timescale) ==
                    Fraction(self.media_duration, self.timescale),
                    "Unedited MP4 track has inconsistent duration")
            return Fraction(0), Fraction(self.media_duration, self.timescale)
        require(len(self.edits) <= 2, "Unsupported MP4 edit list")
        edit = self.edits[-1]
        require(edit.duration > 0 and edit.media_time >= 0
                and edit.rate == 65536, "Unsupported media edit")
        initial = 0
        if len(self.edits) == 2:
            empty = self.edits[0]
            require(empty.duration > 0 and empty.media_time == -1
                    and empty.rate == 65536, "Unsupported leading edit")
            initial = empty.duration
        # MOV muxing can round the track header up and the same edit down.
        require(abs(self.header_duration - (initial + edit.duration)) <= 1,
                "MP4 track header and edit duration differ beyond one movie tick")
        return Fraction(initial, self.movie_timescale), Fraction(initial + edit.duration, self.movie_timescale)

    @property
    def presentation_start(self) -> Fraction:
        return self.simple_range()[0]

    @property
    def presentation_end(self) -> Fraction:
        return self.simple_range()[1]

    @property
    def media_start(self) -> int:
        self.simple_range()
        return self.edits[-1].media_time if self.edits else 0

    @cached_property
    def _sizes(self) -> tuple[int, int, memoryview]:
        sizes = child(self.table, b"stsz")
        fixed_size, count = struct.unpack_from(">II", sizes, 4)
        require(len(sizes) == 12 + (0 if fixed_size else 4 * count), "Invalid MP4 sample sizes")
        return fixed_size, count, sizes[12:]

    @property
    def sample_count(self) -> int:
        try:
            return self._sizes[1]
        except (struct.error, IndexError, TypeError) as error:
            raise TimingError("Cannot read MP4 sample count") from error

    @cached_property
    def _durations(self) -> tuple[tuple[int, int], ...]:
        durations = tuple(entries(child(self.table, b"stts"), ">II"))
        require(durations and all(n > 0 for n, _ in durations)
                and sum(n for n, _ in durations) == self.sample_count, "Invalid MP4 decode timing count")
        return durations

    @cached_property
    def _offsets(self) -> tuple[tuple[int, int], ...]:
        offsets = child(self.table, b"ctts", optional=True)
        if offsets is None:
            return ((self.sample_count, 0),)
        layout = ">II" if version(offsets) == 0 else ">Ii"
        offsets = tuple(entries(offsets, layout))
        require(all(n > 0 for n, _ in offsets) and sum(n for n, _ in offsets) == self.sample_count,
                "Invalid MP4 composition timing count")
        return offsets

    @cached_property
    def _chunks(self) -> tuple[tuple[tuple[int, int, int, int], ...], memoryview, int]:
        offsets32 = child(self.table, b"stco", optional=True)
        offsets64 = child(self.table, b"co64", optional=True)
        require((offsets32 is None) != (offsets64 is None), "Unsupported MP4 chunk offsets")
        offsets, width = (offsets64, 8) if offsets32 is None else (offsets32, 4)
        chunk_count, = struct.unpack_from(">I", offsets, 4)
        require(len(offsets) == 8 + chunk_count * width, "Invalid MP4 chunk offset table")
        chunks = tuple(entries(child(self.table, b"stsc"), ">III"))
        require(chunks and chunks[0][0] == 1 and chunks[-1][0] <= chunk_count
                and all(per_chunk > 0 and description > 0 for _, per_chunk, description in chunks)
                and all(left[0] < right[0] for left, right in zip(chunks, chunks[1:])), "Unsupported MP4 chunk layout")
        after = tuple(first for first, _, _ in chunks[1:]) + (chunk_count + 1,)
        ranges = []
        end_sample = 0
        for (first, per_chunk, description), stop in zip(chunks, after):
            end_sample += (stop - first) * per_chunk
            ranges.append((first, per_chunk, description, end_sample))
        require(end_sample == self.sample_count, "Inconsistent MP4 chunk sample counts")
        return tuple(ranges), offsets[8:], width

    def samples(self):
        """Yield index, DTS, PTS and duration in the unedited media clock."""
        try:
            duration_runs = self._durations
            offset_runs = self._offsets
        except (struct.error, IndexError, TypeError) as error:
            raise TimingError("Cannot read MP4 sample timestamps") from error
        durations = (value for count, value in duration_runs for value in repeat(value, count))
        offsets = (value for count, value in offset_runs for value in repeat(value, count))
        dts = 0
        for index, (duration, offset) in enumerate(zip(durations, offsets)):
            yield index, dts, dts + offset, duration
            dts += duration

    def sample_size(self, index: int) -> int:
        try:
            fixed_size, count, sizes = self._sizes
            require(0 <= index < count, "MP4 sample index is out of range")
            return fixed_size or struct.unpack_from(">I", sizes, index * 4)[0]
        except (struct.error, IndexError, TypeError) as error:
            raise TimingError("Cannot read MP4 sample size") from error

    def packet(self, index: int) -> str:
        """Hash one compressed sample without reading neighboring payloads."""
        try:
            fixed_size, _, sizes = self._sizes
            size = self.sample_size(index)
            ranges, offsets, width = self._chunks
            previous_end = 0
            for first, per_chunk, _, end in ranges:
                if index < end:
                    from_run, within_chunk = divmod(index - previous_end, per_chunk)
                    chunk_index = first - 1 + from_run
                    break
                previous_end = end
            else:
                raise TimingError("MP4 sample has no chunk")
            position, = struct.unpack_from(">Q" if width == 8 else ">I", offsets, chunk_index * width)
            if fixed_size:
                position += within_chunk * fixed_size
            else:
                position += sum(n for n, in struct.iter_unpack(">I", sizes[(index - within_chunk) * 4:index * 4]))
            require(size > 0 and any(lo <= position and position + size <= hi for lo, hi in self.payloads),
                    "MP4 sample is outside the data boxes")
            digest = hashlib.sha256()
            with self.path.open("rb") as handle:
                handle.seek(position)
                while size:
                    data = read_exact(handle, min(size, HASH_BLOCK_SIZE))
                    digest.update(data)
                    size -= len(data)
            return digest.hexdigest()
        except (OSError, struct.error, IndexError, TypeError) as error:
            raise TimingError("Cannot read compressed MP4 sample") from error

    def sample_at(self, time: Fraction) -> tuple[int, int, int, int]:
        """Find the sample at a presented movie time, with raw media timestamps."""
        start, end = self.simple_range()
        require(start <= time < end, "Requested time is outside MP4 presentation")
        media = self.media_start + (time - start) * self.timescale
        return self.sample_at_media(media)

    def sample_at_media(self, media: int | Fraction) -> tuple[int, int, int, int]:
        require(media >= 0, "Requested MP4 media time is negative")
        try:
            if self.kind == b"soun" and self._offsets == ((self.sample_count, 0),):
                index = dts = 0
                for count, duration in self._durations:
                    if media < dts + count * duration:
                        offset = (media - dts) // duration
                        dts += offset * duration
                        return index + offset, dts, dts, duration
                    index += count
                    dts += count * duration
                raise TimingError("Audio sample does not cover presented time")
        except (struct.error, IndexError, TypeError) as error:
            raise TimingError("Cannot read MP4 sample timing") from error
        sample = max((row for row in self.samples() if row[2] <= media),
                     key=lambda row: row[2], default=None)
        require(sample is not None and media < sample[2] + sample[3], "No sample at requested MP4 time")
        return sample


@dataclass(frozen=True)
class Movie:
    path: Path
    timescale: int
    duration: int
    tracks: tuple[Track, ...]
    payloads: tuple[tuple[int, int], ...]

    @property
    def videos(self) -> tuple[Track, ...]:
        return tuple(track for track in self.tracks if track.kind == b"vide")

    @property
    def audios(self) -> tuple[Track, ...]:
        return tuple(track for track in self.tracks if track.kind == b"soun")


def read_track(path, payloads, data, movie_timescale):
    tkhd = child(data, b"tkhd")
    if version(tkhd) == 0:
        duration, = struct.unpack_from(">I", tkhd, 20)
    else:
        duration, = struct.unpack_from(">Q", tkhd, 28)
    mdia = child(data, b"mdia")
    scale, media_duration = clock(child(mdia, b"mdhd"))
    kind = bytes(child(mdia, b"hdlr")[8:12])
    require(len(kind) == 4, "Missing MP4 track handler")
    samples = child(child(mdia, b"minf"), b"stbl")
    edits = ()
    edts = child(data, b"edts", optional=True)
    if edts is not None:
        elst = child(edts, b"elst")
        layout = ">Iii" if version(elst) == 0 else ">Qqi"
        edits = tuple(Edit(*row) for row in entries(elst, layout))
        require(edits, "Empty MP4 edit list")
    return Track(kind, path, payloads, movie_timescale, scale, media_duration, duration, edits, samples)


def read_movie(path: Path) -> Movie:
    try:
        movie_data = None
        payloads = []
        with path.open("rb") as handle:
            file_size = handle.seek(0, 2)
            pos = 0
            while pos < file_size:
                require(file_size - pos >= 8, "Truncated MP4 top-level header")
                handle.seek(pos)
                size, kind, width = box_size_and_type(read_exact(handle, 8), file_size - pos,
                                                     lambda: read_exact(handle, 8))
                require(kind != b"moof", "Fragmented MP4 timing is unsupported")
                if kind == b"moov":
                    require(movie_data is None and size <= MAX_METADATA, "Unsupported MP4 metadata layout or size")
                    movie_data = memoryview(read_exact(handle, size - width))
                elif kind == b"mdat":
                    payloads.append((pos + width, pos + size))
                pos += size
        require(movie_data is not None and payloads, "Missing MP4 data or presentation timing")
        require(child(movie_data, b"mvex", optional=True) is None, "Fragmented MP4 timing is unsupported")
        scale, duration = clock(child(movie_data, b"mvhd"))
        payloads = tuple(payloads)
        tracks = tuple(read_track(path, payloads, track, scale) for _, track in boxes(movie_data, b"trak"))
        require(tracks, "Missing MP4 tracks")
        return Movie(path, scale, duration, tracks, payloads)
    except (OSError, struct.error, IndexError) as error:
        raise TimingError("Cannot read MP4 timing tables") from error


def audio_probe_command(ffprobe: str, path: Path) -> list[str]:
    return [
        ffprobe, "-v", "error", "-select_streams", "a", "-read_intervals", "%+#1",
        "-show_data", "-show_data_hash", "sha256", "-show_entries",
        "stream=codec_name,profile,sample_rate,time_base,start_pts,duration_ts,extradata,extradata_size:"
        "packet=pts,dts,duration,data_hash:packet_side_data=side_data_type,skip_samples,discard_padding",
        "-of", "json", str(path),
    ]


def exact_audio_ticks(seconds: Fraction) -> int:
    ticks = seconds * AAC_RATE
    require(ticks.denominator == 1 and 0 <= ticks < MAX_EXACT_EXPRESSION_INTEGER,
            "Audio boundary is outside the exact sample clock")
    return int(ticks)


@dataclass(frozen=True)
class AacSamples:
    movie: Movie
    track: Track
    @classmethod
    def read(cls, movie: Movie, *, allow_leading_empty: bool = False) -> AacSamples:
        require(len(movie.audios) == 1, "AAC cap requires one audio track")
        track = movie.audios[0]
        start, _ = track.simple_range()
        require(track.timescale == AAC_RATE and (
            (len(track.edits) == 1 and start == 0)
            or (allow_leading_empty and len(track.edits) == 2 and start > 0)
        ), "AAC cap requires a simple 48 kHz edit")
        table = track.table
        require(child(table, b"ctts", optional=True) is None, "AAC offsets are not verified")
        descriptions = child(table, b"stsd")
        count, = struct.unpack_from(">I", descriptions, 4)
        require(count == 1 and tuple(kind for kind, _ in boxes(descriptions[8:])) == (b"mp4a",),
                "Unsupported AAC sample description")
        groups = child(table, b"sgpd", optional=True)
        mapping = child(table, b"sbgp", optional=True)
        if groups is not None or mapping is not None:
            require(groups is not None and mapping is not None and
                    struct.unpack(">I4sIIh", groups) == (1 << 24, b"roll", 2, 1, -1) and
                    struct.unpack(">I4sIII", mapping) == (0, b"roll", 1, track.sample_count, 1),
                    "Unverified AAC sample groups")
        timing = track._durations
        require(all(0 < delta <= AAC_FRAME for _, delta in timing), "Unsupported AAC sample durations")
        require(all(delta == AAC_FRAME for _, delta in timing[:-1])
                and (timing[-1][1] == AAC_FRAME or timing[-1][0] == 1), "Unsupported AAC frame layout")
        require(sum(n * delta for n, delta in timing) == track.media_duration
                and track.media_duration < MAX_EXACT_EXPRESSION_INTEGER, "Inconsistent AAC media duration")
        require(all(description == 1 for _, _, description, _ in track._chunks[0]),
                "AAC has an unverified sample description switch")
        return cls(movie, track)

    def packet(self, media_sample: int) -> tuple[int, str]:
        require(0 <= media_sample < self.track.media_duration, "AAC sample is outside the media")
        # All non-terminal AAC units contain exactly 1024 samples.
        index, location = divmod(media_sample, AAC_FRAME)
        return location, "SHA256:" + self.track.packet(index)

    def presentation(self, probe: dict, duration: int, *, exact_movie: bool = False) -> tuple[int, int]:
        streams, packets = probe["streams"], probe["packets"]
        require(len(streams) == len(packets) == 1, "AAC timing probe did not return one stream and packet")
        stream = streams[0]
        packet = packets[0]
        config_text = re.search(r"^\s*00000000:\s+([0-9a-f]{4})(?:\s|$)", stream["extradata"], re.I | re.M)
        require(stream["extradata_size"] == 2 and config_text is not None, "Unproved AAC codec configuration")
        config = int(config_text.group(1), 16)
        require(config >> 11 == 2 and (config >> 7) & 15 == 3 and (config >> 3) & 15 in (1, 2)
                and config & 7 == 0, "AAC cap requires AAC-LC, 48 kHz and 1024 samples per frame")
        start = exact_audio_ticks(self.track.presentation_start) if exact_movie else 0
        # The supported FFprobe clock includes an initial empty edit in
        # duration_ts. Require agreement with the exact MP4 range.
        require(stream["codec_name"] == "aac" and stream.get("profile", "LC") == "LC"
                and int(stream["sample_rate"]) == AAC_RATE and Fraction(stream["time_base"]) == Fraction(1, AAC_RATE)
                and int(stream["start_pts"]) == start
                and int(stream["duration_ts"]) == start + duration and duration > 0,
                "Unproved AAC presentation clock")
        actual = self.track.presentation_end
        expected = Fraction(start + duration, AAC_RATE)
        if exact_movie:
            require(expected == actual, "Source AAC edit does not end at an exact audio sample")
        else:
            require(self.movie.timescale == 1000000 and abs(actual - expected) < Fraction(1, self.movie.timescale),
                    "Output AAC edit differs beyond its movie-tick quantization")
        media_start = self.track.media_start
        require(media_start + duration <= self.track.media_duration, "AAC media does not cover the presentation")
        pts, dts = int(packet["pts"]), int(packet["dts"])
        skip = start - pts
        require(pts == dts and skip >= 0 and media_start >= skip, "Unproved AAC demux origin")
        side = packet.get("side_data_list", [])
        if skip:
            require(len(side) == 1 and side[0]["side_data_type"] == "Skip Samples"
                    and int(side[0]["skip_samples"]) == skip and int(side[0]["discard_padding"]) == 0,
                    "Unproved AAC priming")
        else:
            require(not side, "Unexpected AAC priming")
        omitted = media_start - skip
        require(omitted % AAC_FRAME == 0 and self.packet(omitted)[1] == packet["data_hash"],
                "AAC demux origin packet is not on the proven sample clock")
        return skip, config


@dataclass(frozen=True)
class AudioCap:
    ticks: int
    codec_config: int
    skip: int
    begin: tuple[int, str]
    end: tuple[int, str]

    @property
    def filter(self) -> str:
        return f"setts=pts=PTS:dts=DTS:duration=min(DURATION\\,{self.ticks}-PTS)"

    def verify(self, output: Movie, probe: dict) -> None:
        try:
            samples = AacSamples.read(output)
            self.boundaries(samples, probe)
        except TimingError:
            raise
        except (KeyError, OSError, struct.error, IndexError, TypeError, ValueError, ZeroDivisionError) as error:
            raise TimingError("Cannot verify capped AAC presentation") from error

    def boundaries(self, samples: AacSamples, probe: dict) -> None:
        require(samples.presentation(probe, self.ticks) == (self.skip, self.codec_config),
                "AAC codec configuration or priming changed")
        start = samples.track.media_start
        stop = start + self.ticks
        require(samples.track.media_duration == stop, "Excess AAC samples remain after the presentation")
        require(samples.packet(start) == self.begin and samples.packet(stop - 1) == self.end,
                "AAC presentation boundary differs from the source")


def audio_cap(source: Movie, audio: Movie, start: Fraction, end: Fraction,
              source_probe: dict, audio_probe: dict) -> AudioCap:
    """Prove that one audio-only intermediate can be capped during the final mux."""
    try:
        original = AacSamples.read(source, allow_leading_empty=True)
        require(not audio.videos and len(audio.tracks) == 1, "Expected a private audio-only intermediate")
        part = AacSamples.read(audio)
        source_start = exact_audio_ticks(original.track.presentation_start)
        source_end = exact_audio_ticks(original.track.presentation_end)
        _, config = original.presentation(source_probe, source_end - source_start, exact_movie=True)
        begin, stop = exact_audio_ticks(start), exact_audio_ticks(end)
        require(source_start <= begin < stop <= source_end, "AAC cap is outside the source's presentation")
        source_origin = original.track.media_start
        ticks = stop - begin
        skip, _ = part.presentation(audio_probe, ticks)
        require(ticks + skip < MAX_EXACT_EXPRESSION_INTEGER, "AAC cap exceeds the exact filter clock")
        # The intermediate has exactly one positive partial tail at most. Its
        # unedited clock therefore proves every fed packet begins before the cap.
        cap = AudioCap(ticks, config, skip, original.packet(source_origin + begin - source_start),
                       original.packet(source_origin + stop - source_start - 1))
        cap.boundaries(part, audio_probe)
        return cap
    except TimingError:
        raise
    except (KeyError, OSError, struct.error, IndexError, TypeError, ValueError, ZeroDivisionError) as error:
        raise TimingError("Cannot prove AAC sample boundaries") from error


def audio_boundary(track: Track, media_sample: int) -> tuple[int, str]:
    index, _, pts, _ = track.sample_at_media(media_sample)
    return media_sample - pts, track.packet(index)


def presented_tick_count(track: Track) -> int:
    """Map an output edit to the only sample count inside a movie clock tick."""
    start, end = track.simple_range()
    ticks = (end - start) * track.timescale
    error_bound = Fraction(track.timescale, track.movie_timescale)
    require(error_bound < Fraction(1, 2), "MP4 edit cannot identify one audio sample boundary")
    nearest = round(ticks)
    require(nearest > 0 and abs(nearest - ticks) < error_bound, "Audio edit is not on the sample clock")
    return nearest


def neighboring_ticks(ticks: Fraction) -> tuple[int, ...]:
    lower = ticks.numerator // ticks.denominator
    return (lower,) if ticks.denominator == 1 else (lower, lower + 1)


def verify_audio_copy(original: Track, cut: Track, actual: Track,
                      start: Fraction, end: Fraction, bounded: bool, *, strict_end: bool = False,
                      aac_source: tuple[Movie, dict] | None = None) -> None:
    """Verify copied audio range and matching original first and last access units."""
    rate = original.timescale
    require(actual.timescale == cut.timescale == rate, "Copied audio changed its sample clock")
    origin = original.presentation_start
    expected_end = min(end, original.presentation_end)
    require(expected_end > max(start, origin), "No source audio at this part's range")
    require(cut.presentation_start == actual.presentation_start, "Copied audio changed its presentation delay")

    intended = presented_tick_count(cut)
    observed = presented_tick_count(actual)
    require(observed >= intended and (not bounded or observed == intended),
            "Final mux shortened a required sample range or did not honor the cap")
    if strict_end:
        # Use the sample count recovered from the output edit, not its
        # quantized movie-clock duration or FFprobe's container duration.
        require(start + actual.presentation_start + Fraction(observed, rate) <= expected_end,
                "Final mux presented audio beyond the trimmed input")

    media_start = original.media_start
    original_first = media_start + (start + cut.presentation_start - origin) * rate
    original_end = original.media_start + (expected_end - origin) * rate
    # The source's coarse movie edit may round end up past its last sample
    # (e.g. a 1 kHz edit covering an integral 48 kHz track). Prove that the
    # discrepancy is only source movie-tick quantization before using the
    # precise backing AAC data extent.
    source_backed_end = original.media_duration
    if original_end > source_backed_end:
        require(original_end - source_backed_end < Fraction(rate, original.movie_timescale),
                "Source audio edit extends beyond its backing samples")
        original_end = Fraction(source_backed_end)
    expected_left = audio_boundary(cut, cut.media_start)
    expected_right = audio_boundary(cut, cut.media_start + intended - 1)
    matches = [first for first in neighboring_ticks(original_first)
               if first + intended in neighboring_ticks(original_end)
               and audio_boundary(original, first) == expected_left
               and audio_boundary(original, first + intended - 1) == expected_right]
    require(len(matches) == 1, "Copied audio boundaries do not uniquely match source samples")
    require(audio_boundary(actual, actual.media_start) == expected_left and
            audio_boundary(actual, actual.media_start + intended - 1) == expected_right,
            "Final mux did not retain the expected audio sample boundaries")
    first, = matches
    if first < media_start:
        # Only the first AAC part may expose a proved prefix of encoder priming.
        require(start == cut.presentation_start == 0 and aac_source is not None,
                "Unproved leading AAC presentation")
        movie, probe = aac_source
        samples = AacSamples.read(movie, allow_leading_empty=True)
        skip, _ = samples.presentation(probe, exact_audio_ticks(original.presentation_end - origin), exact_movie=True)
        extra = media_start - first
        require(samples.track is original and first == media_start - exact_audio_ticks(origin)
                and 0 < extra < min(skip, AAC_FRAME, intended) and skip == media_start
                and cut.media_start == actual.media_start == first,
                "Leading presentation is not limited to source AAC priming")
        for index in range(max(0, first // AAC_FRAME - 1), media_start // AAC_FRAME + 1):
            require(original.packet(index) == cut.packet(index) == actual.packet(index),
                    "Leading AAC units or their decoder predecessor differ from source")
    else:
        required = media_start + max(0, (start - origin) * rate)
        require(first in neighboring_ticks(required), "Copied audio does not cover the required start")
    if observed > intended:
        # Legacy copy mux may restore only the unpresented suffix of the same
        # final AAC access unit. Never accept another packet as harmless padding.
        descriptions = child(actual.table, b"stsd")
        codecs = tuple(kind for kind, _ in boxes(descriptions[8:]))
        require(codecs == (b"mp4a",), "Only a proven final AAC unit can have a retained suffix")
        tail = actual.sample_at_media(actual.media_start + observed - 1)
        intended_tail = actual.sample_at_media(actual.media_start + intended - 1)
        require(tail[0] == intended_tail[0] and 0 < tail[3] <= AAC_FRAME,
                "Final mux added audio outside the last verified AAC unit")
