"""Proof for the independently re-encoded video in an N3 split part."""

from __future__ import annotations

from dataclasses import dataclass
from fractions import Fraction
from math import ceil, lcm
from pathlib import Path
from typing import Callable
import json
import re
import struct
import subprocess

if __package__:
    from . import split_timing as timing
else:
    import split_timing as timing


# The same bounds apply to both the encoder and its decoded comparison. N3
# processes one affected part at a time. A short lookahead limits frame buffers.
_THREADS = "4"
_ERROR_CODE = "processing.split_validation_failed"
_FIELDS = ("codec_name", "width", "height", "pix_fmt", "sample_aspect_ratio", "field_order",
           "color_range", "color_space", "color_transfer", "color_primaries", "chroma_location")
_COLOR_OPTIONS = {
    "color_range": "-color_range", "color_space": "-colorspace", "color_transfer": "-color_trc",
    "color_primaries": "-color_primaries", "chroma_location": "-chroma_sample_location",
}
_METADATA_ERROR = "Cannot verify the re-encoded video format"
RunCommand = Callable[[list[str], str], subprocess.CompletedProcess[str]]


def _probe(path: Path, ffprobe: str, run_command: RunCommand):
    fields = ",".join((*_FIELDS, "start_pts", "time_base", "extradata_hash"))
    result = run_command([ffprobe, "-v", "error", "-select_streams", "v", "-show_streams",
                          "-show_data_hash", "sha256", "-show_entries",
                          f"stream={fields}:stream_side_data=side_data_type", "-of", "json", str(path)],
                         _ERROR_CODE)
    try:
        stream, = json.loads(result.stdout)["streams"]
    except (ValueError, KeyError, TypeError) as error:
        raise timing.TimingError(_METADATA_ERROR) from error
    timing.require(not stream.get("side_data_list"), "Unsupported video display metadata for re-encoding")
    return stream


def _format(stream):
    def aspect():
        ratio = stream.get("sample_aspect_ratio")
        return "1:1" if ratio in (None, "N/A", "0:1") else ratio

    return tuple(aspect() if name == "sample_aspect_ratio" else
                 (stream.get(name) if stream.get(name) not in ("unknown", "unspecified") else None)
                 for name in _FIELDS)


def _first_sync(track):
    try:
        table = timing.child(track.table, b"stss", optional=True)
        # ISO BMFF treats a track without stss as entirely sync samples.
        return table is None or next(timing.entries(table, ">I"), None) == (1,)
    except (struct.error, IndexError, TypeError) as error:
        raise timing.TimingError("Cannot verify the first video sync sample") from error


def _encoded_digest(path, ffmpeg, run_command):
    result = run_command([ffmpeg, "-v", "error", "-xerror", "-ignore_editlist", "1", "-i", str(path),
                          "-map", "0:v:0", "-c", "copy", "-f", "streamhash", "-hash", "sha256", "-"],
                         _ERROR_CODE)
    digest = result.stdout.strip()
    timing.require(re.fullmatch(r"0,v,SHA256=[0-9a-f]{64}", digest) is not None,
                   "Cannot verify the re-encoded video stream digest")
    return digest


@dataclass(frozen=True)
class GeneratedVideo:
    start: Fraction
    source_first_picture: Fraction
    timescale: int
    timestamps: tuple[int, ...]
    format: tuple
    codec_configuration: str
    digest: str
    ffmpeg: str
    ffprobe: str
    run_command: RunCommand

    def verify(self, path: Path, *, verify_payload: bool = True) -> None:
        """Prove that the final mux preserves the already matched video and its PTS.

        The caller verifies the expected final edit end separately so that the
        existing Method B can extend the final displayed picture to audio end.
        """
        movie = timing.read_movie(path)
        timing.require(len(movie.videos) == 1, "No unique re-encoded video track")
        track = movie.videos[0]
        timing.require(track.presentation_start == 0 and track.timescale == self.timescale
                       and track.media_start == 0 and track.sample_count == len(self.timestamps)
                       and _first_sync(track), "Invalid re-encoded video track timing or sync sample")
        for target, (_, dts, pts, duration) in zip(self.timestamps, track.samples()):
            timing.require(dts == pts == target and duration > 0, "Re-encoded video PTS does not match the source plan")
        timing.require(self.timestamps[-1] < track.presentation_end * track.timescale,
                       "The re-encoded video edit hides the last verified picture")
        stream = _probe(path, self.ffprobe, self.run_command)
        timing.require(_format(stream) == self.format and stream.get("extradata_hash") == self.codec_configuration,
                       "Mux changed the verified video format or H.264 configuration")
        if verify_payload:
            timing.require(_encoded_digest(path, self.ffmpeg, self.run_command) == self.digest,
                           "Mux changed the verified re-encoded video payload")


def _sync_indices(track: timing.Track) -> set[int] | None:
    try:
        table = timing.child(track.table, b"stss", optional=True)
        return {index - 1 for (index,) in timing.entries(table, ">I")} if table is not None else None
    except (struct.error, TypeError, IndexError) as error:
        raise timing.TimingError("Cannot verify video sync samples for splitting") from error


def sync_times(track: timing.Track) -> list[Fraction]:
    # Use the source table for exact coordinates and avoid dumping a full
    # FFprobe packet JSON file. N3 checks the actual packet at every cut.
    start, end = track.simple_range()
    indices = _sync_indices(track)
    ticks = {pts for index, _, pts, _ in track.samples()
             if (indices is None or index in indices) and pts >= track.media_start}
    return [point for tick in sorted(ticks) if (point := start + Fraction(tick - track.media_start, track.timescale)) < end]


def _source_plan(track, start, end):
    timing.require(track.presentation_start == 0 and 0 <= start < end,
                   "Unsupported re-encoded video presentation origin or interval")
    media_end = min(end, track.presentation_end)
    timing.require(start < media_end, "No source picture at requested split start")
    anchor_index, _, anchor_pts, _ = track.sample_at(start)
    source_clock = track.timescale
    clock = lcm(source_clock, 48000, start.denominator, end.denominator)
    timing.require(clock < 2**31 and end * clock < 2**52, "Split time is outside the re-encode clock")
    in_end = ceil(media_end * source_clock)
    target_ticks = int(start * clock)
    divisor = clock // source_clock
    selected = []
    keys = _sync_indices(track)
    seek = 0
    for index, _, pts, _ in track.samples():
        normalized = pts - track.media_start
        # Retain only presented source samples. Seek to a sync sample preceding
        # the first selected picture, and let the decoder consume its references.
        if index <= anchor_index and 0 <= normalized <= anchor_pts - track.media_start:
            if keys is None or index in keys:
                seek = max(seek, normalized)
        if anchor_pts <= pts and normalized < in_end:
            selected.append(normalized)
    selected.sort()
    anchor = anchor_pts - track.media_start
    timing.require(selected and selected[0] == anchor and 0 <= anchor <= start * source_clock and
                   all(a < b for a, b in zip(selected, selected[1:])),
                   "Unproved source picture order for re-encoding")
    timestamps = tuple(max(0, value * divisor - target_ticks) for value in selected)
    timing.require(timestamps[0] == 0 and all(a < b for a, b in zip(timestamps, timestamps[1:])),
                   "Split source pictures do not map to an exact part timeline")
    # Floor the seek option in FFmpeg's microsecond clock. Rounding up can
    # skip the exact sync sample just before a VFR hold.
    seek_us = seek * 1_000_000 // source_clock
    seek_arg = f"{seek_us // 1_000_000}.{seek_us % 1_000_000:06d}"
    selection = (f"trim=start_pts={anchor}:end_pts={in_end},settb=1/{clock},"
                 f"setpts='if(eq(N,0),0,PTS-{target_ticks})'")
    return Fraction(anchor, source_clock), clock, timestamps, seek_arg, selection


def _verify_decoded_pictures(source_args, path, selection, clock, timestamps, ffmpeg, run_command):
    # Decode both streams at their matching presentation timestamps. Write
    # only textual timing / PSNR statistics, never full frames, to the parent.
    graph = (f"[0:v:0]{selection},showinfo[reference];"
             f"[1:v:0]settb=1/{clock}[part];"
             "[part][reference]psnr=stats_file=-:shortest=1:repeatlast=0[checked]")
    result = run_command([ffmpeg, "-hide_banner", "-nostdin", "-loglevel", "info", "-xerror", "-copyts",
                          "-filter_complex_threads", "1", *source_args, "-threads", _THREADS, "-i", str(path),
                          "-filter_complex", graph, "-map", "[checked]", "-an", "-fps_mode", "passthrough",
                          "-threads", _THREADS, "-f", "null", "-"], _ERROR_CODE)
    source_pts = tuple(int(value) for value in re.findall(
        r"\[Parsed_showinfo[^\]]+\]\s+n:\s*\d+\s+pts:\s*(-?\d+)", result.stderr))
    timing.require(source_pts == timestamps, "Decoded source pictures do not match the verified split timeline")
    matches = re.findall(r"(?m)^n:(\d+)\s+.*?\bpsnr_avg:(\S+)", result.stdout)
    # This conservative gross mismatch check is not a quality target. The
    # prior encoder's CRF proof allows normal source-vs-output rounding loss.
    timing.require(len(matches) == len(timestamps) and all(
        int(number) == index and (value == "inf" or float(value) >= 24)
        for index, (number, value) in enumerate(matches, 1)), "Re-encoded pictures do not match the source")


def encode_part(source: Path, source_track: timing.Track, start: Fraction, end: Fraction,
                destination: Path, ffmpeg: str, ffprobe: str, run_command: RunCommand) -> GeneratedVideo:
    """Encode an independent H.264 part and establish its decoded source correspondence."""
    original = _probe(source, ffprobe, run_command)
    try:
        timing.require(original["codec_name"] == "h264" and original["pix_fmt"] == "yuv420p"
                       and original["field_order"] == "progressive"
                       and int(original["start_pts"]) == 0
                       and Fraction(original["time_base"]) == Fraction(1, source_track.timescale),
                       "Unsupported source video for independent re-encoding")
    except timing.TimingError:
        raise
    except (KeyError, ValueError, TypeError, ZeroDivisionError) as error:
        raise timing.TimingError(_METADATA_ERROR) from error
    anchor, clock, timestamps, seek, selection = _source_plan(source_track, start, end)
    source_args = ["-noautorotate", "-noaccurate_seek", "-threads", _THREADS,
                   "-ss", seek, "-i", str(source)]
    colors = [arg for field, flag in _COLOR_OPTIONS.items()
              if original.get(field) not in (None, "unknown", "unspecified")
              for arg in (flag, original[field])]
    run_command([ffmpeg, "-v", "error", "-nostdin", "-xerror", "-n", "-copyts", "-filter_threads", "1",
                 *source_args, "-map", "0:v:0", "-vf", selection, "-an", "-map_metadata", "-1",
                 "-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p", *colors,
                 "-threads", _THREADS, "-rc-lookahead", "12", "-bf", "0", "-fps_mode", "passthrough",
                 "-enc_time_base", f"1:{clock}", "-video_track_timescale", str(clock),
                 "-movie_timescale", "1000000", str(destination)], "processing.split_failed")
    stream = _probe(destination, ffprobe, run_command)
    codec = stream.get("extradata_hash")
    timing.require(_format(stream) == _format(original) and isinstance(codec, str),
                   "Re-encoding changed the video format or lost its H.264 configuration")
    _verify_decoded_pictures(source_args, destination, selection, clock, timestamps, ffmpeg, run_command)
    digest = _encoded_digest(destination, ffmpeg, run_command)
    generated = GeneratedVideo(start, anchor, clock, timestamps, _format(original), codec, digest, ffmpeg,
                               ffprobe, run_command)
    generated.verify(destination, verify_payload=False)
    return generated
