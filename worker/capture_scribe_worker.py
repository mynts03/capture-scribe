#!/usr/bin/env python3
"""CaptureScribe post-processing worker.

The worker only consumes a validated Source Master described by a recording
manifest. It never changes or deletes that source. Derived video and transcript
files are published without overwriting existing user files, then returned as
structured JSON for the Rust product layer to record in the manifest.
"""

from __future__ import annotations

import argparse
import contextvars
import csv
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import uuid
from bisect import bisect_left, bisect_right
from collections.abc import Iterator
from decimal import Decimal, InvalidOperation
from fractions import Fraction
from itertools import islice
from math import ceil
from pathlib import Path
from typing import Any

if __package__:
    from . import split_timing, split_video_encode
else:
    import split_timing
    import split_video_encode

MAX_SPLIT_MINUTES = 4_294_967_295
TRAILING_SILENCE_TOLERANCE_SECONDS = 0.25
SILENCE_EVENT_PATTERN = re.compile(
    r"silence_(start|end):\s*(-?\d+(?:\.\d+)?)"
)
publication_context: contextvars.ContextVar[dict[str, Any] | None] = contextvars.ContextVar("publication_context", default=None)


def record_publication(destination: Path) -> None:
    """Receipt only after exclusive public link succeeds; never parse prose.

    The receipt proves publication, not decoding or continued availability.
    Rust supplies a fresh per-run directory; old direct worker callers still
    work without a receipt context. A receipt failure never deletes the output.
    """
    context = publication_context.get()
    if context is None:
        return
    stat = destination.stat()
    identity = f"unix:{stat.st_dev}:{stat.st_ino}" if os.name == "posix" else f"metadata:{stat.st_size}:{stat.st_mtime_ns}"
    receipt = {"format": "capturescribe.publication", "schemaVersion": 1,
        "runId": context["runId"], "jobId": context["jobId"], "manifestPath": context["manifestPath"],
        "sourceMaster": context["sourceMaster"], "artifact": {
            "role": "transcript" if destination.suffix == ".txt" else "user_output",
            "path": str(destination), "validation": "publication-only",
            "sourceTimelineOffsetSeconds": None, "fileIdentity": identity}}
    path = context["directory"] / f"published-{uuid.uuid4().hex}.json"
    try:
        # One immutable receipt per file, fsynced before the next publication.
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(receipt, stream, ensure_ascii=False)
            stream.flush()
            os.fsync(stream.fileno())
    except Exception as error:
        raise MessageError("processing.receipt_failed", f"Published output retained; receipt failed: {error}") from error


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


class MessageError(Exception):
    def __init__(self, code: str | None, raw: str, **params: Any):
        super().__init__(raw)
        self.message = {"raw": raw}
        if code:
            self.message["code"] = code
        if params:
            self.message["params"] = params


class SplitPreparationError(MessageError):
    """A private split failed before any part was published."""


def error_message(error: Exception) -> dict[str, Any]:
    if isinstance(error, MessageError):
        return dict(error.message)
    return {"raw": f"{type(error).__name__}: {error}"}


def progress(code: str, **params: Any) -> None:
    message: dict[str, Any] = {"code": code}
    if params:
        message["params"] = params
    log(json.dumps({"event": "progress", "message": message}, ensure_ascii=False))


def subprocess_message(details: str, fallback_code: str) -> MessageError:
    # Parse only the explicit protocol; never infer a cause from prose.
    for line in reversed(details.splitlines()):
        try:
            event = json.loads(line)
        except (ValueError, TypeError):
            continue
        if isinstance(event, dict) and event.get("event") == "error":
            message = event.get("message")
            if isinstance(message, dict) and isinstance(message.get("raw"), str):
                code = message.get("code")
                params = message.get("params", {})
                if not isinstance(params, dict):
                    params = {}
                return MessageError(code if isinstance(code, str) else None, details, **params)
    return MessageError(fallback_code, details)


def run_command(command: list[str], code: str) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip() or f"Process exited with status {result.returncode}"
        raise subprocess_message(details, code)
    return result


def probe_duration_decimal(video: Path, ffprobe: str) -> Decimal:
    result = run_command(
        [
            ffprobe,
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            str(video),
        ],
        "processing.probe_failed",
    )
    try:
        duration = Decimal(result.stdout.strip())
    except InvalidOperation as error:
        raise MessageError("processing.duration_invalid", f"動画時間を解釈できません: {video}", path=str(video)) from error
    if not duration.is_finite() or duration <= 0:
        raise MessageError("processing.duration_invalid", f"有効な動画時間を取得できません: {video}", path=str(video))
    return duration


def probe_duration(video: Path, ffprobe: str) -> float:
    return float(probe_duration_decimal(video, ffprobe))


def split_minutes(value: Any) -> int:
    if type(value) is not int or not 1 <= value <= MAX_SPLIT_MINUTES:
        raise MessageError("processing.split_invalid", "Split minutes must be integers in 1..=u32::MAX")
    return value


def should_split(enabled: bool, duration: Decimal, threshold: int, segment: int) -> bool:
    return enabled and duration >= threshold * 60 and duration > segment * 60


def find_trailing_silence_start(output: str, duration: float) -> float | None:
    """Return a complete silence interval that reaches the media end."""
    if duration <= 0:
        return None

    intervals: list[tuple[float, float]] = []
    open_start: float | None = None
    for event_type, raw_timestamp in SILENCE_EVENT_PATTERN.findall(output):
        timestamp = float(raw_timestamp)
        if event_type == "start":
            if open_start is not None:
                return None
            open_start = timestamp
            continue

        if open_start is None or timestamp < open_start:
            return None
        intervals.append((open_start, timestamp))
        open_start = None

    if open_start is not None or not intervals:
        return None
    last_start, last_end = intervals[-1]
    if abs(duration - last_end) > TRAILING_SILENCE_TOLERANCE_SECONDS:
        return None
    return last_start


def allocate_path(directory: Path, stem: str, suffix: str) -> Path:
    candidate = directory / f"{stem}{suffix}"
    if not os.path.lexists(candidate):
        return candidate
    number = 2
    while True:
        candidate = directory / f"{stem} ({number}){suffix}"
        if not os.path.lexists(candidate):
            return candidate
        number += 1


def publish_without_overwrite(temporary: Path, destination: Path) -> None:
    """Atomically create a destination while retaining unrelated files."""
    try:
        os.link(temporary, destination)
    except FileExistsError as error:
        raise MessageError("processing.output_exists", str(error), path=str(destination)) from error
    record_publication(destination)
    temporary.unlink()


def publish_allocated(temporary: Path, directory: Path, stem: str, suffix: str) -> Path:
    while True:
        destination = allocate_path(directory, stem, suffix)
        try:
            publish_without_overwrite(temporary, destination)
            return destination
        except MessageError as error:
            if error.message.get("code") != "processing.output_exists":
                raise
            # Exclusive link refused a newly created file. Allocate again;
            # the unpublished temporary file remains intact.


def trim_trailing_silence(
    source: Path,
    work_dir: Path,
    duration: float,
    ffmpeg: str,
) -> tuple[Path, dict[str, Any]]:
    detection = subprocess.run(
        [
            ffmpeg,
            "-hide_banner",
            "-nostats",
            "-i",
            str(source),
            "-map",
            "0:a:0?",
            "-af",
            "silencedetect=n=-50dB:d=2",
            "-f",
            "null",
            "-",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if detection.returncode != 0:
        return source, {
            "status": "skipped",
            "reason": "silence_detection_failed",
            "raw": detection.stdout,
        }

    silence_start = find_trailing_silence_start(detection.stdout, duration)
    if silence_start is None:
        return source, {
            "status": "not_needed",
            "reason": "no_complete_trailing_silence",
        }

    trim_end = min(duration, silence_start + 3.0)
    if duration - trim_end <= 0.05:
        return source, {
            "status": "not_needed",
            "reason": "trailing_silence_within_three_seconds",
        }

    output = work_dir / "trimmed.mp4"
    progress("processing.silence_found", seconds=f"{trim_end:.3f}")
    run_command(
        [
            ffmpeg,
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(source),
            "-map",
            "0",
            "-t",
            f"{trim_end:.6f}",
            "-c",
            "copy",
            str(output),
        ],
        "processing.trim_failed",
    )
    if not output.is_file() or output.stat().st_size == 0:
        raise MessageError("processing.trim_failed", "末尾無音処理の出力が生成されませんでした。")
    return output, {
        "status": "created",
        "silenceStartSeconds": silence_start,
        "trimEndSeconds": trim_end,
    }


def validate_trim_output(source: Path, output: Path, trim_end: float) -> None:
    """Guard the existing trim route without changing its cut or remux."""
    original = split_timing.read_movie(source)
    trimmed = split_timing.read_movie(output)
    if len(original.videos) != 1 or len(trimmed.videos) != 1 or len(original.audios) != len(trimmed.audios):
        raise split_timing.TimingError("Trim changed the track layout")
    wanted = Fraction(f"{trim_end:.6f}")
    video = original.videos[0]
    target = min(wanted, video.presentation_end)
    retained = []
    for index, _, pts, duration in video.samples():
        start = video.presentation_start + Fraction(pts - video.media_start, video.timescale)
        if video.presentation_start <= start < target:
            retained.append((index, start, start + Fraction(duration, video.timescale)))
    actual = trimmed.videos[0]
    if not retained or actual.sample_count != len(retained) or retained[-1][0] != len(retained) - 1:
        raise split_timing.TimingError("Trim did not retain exactly the requested video pictures")
    same_video_packets(video, actual, Fraction(0), video.samples())
    _, last_start, last_end = max(retained, key=lambda sample: sample[1])
    # A copy trim can retain the final frame intersecting the cut. Its end
    # must still fall in that original frame's range, not in a mux overrun.
    allowed_end = min(video.presentation_end, max(target, last_end))
    tolerance = Fraction(1, trimmed.timescale)
    if not last_start < actual.presentation_end < allowed_end + tolerance:
        raise split_timing.TimingError("Trimmed video edit exceeds the requested source range")
    for before, after in zip(original.audios, trimmed.audios):
        if before.timescale != after.timescale:
            raise split_timing.TimingError("Trim changed the audio sample clock")
        expected = min(wanted, before.presentation_end)
        _, _, _, frame_duration = before.sample_at(expected - Fraction(1, before.timescale))
        packet_width = Fraction(frame_duration, before.timescale)
        if (abs(before.presentation_start - after.presentation_start) >= tolerance
            or not expected - tolerance < after.presentation_end < expected + packet_width + tolerance):
            raise split_timing.TimingError("Trimmed audio edit is outside the expected presentation range")
        rate = before.timescale
        stop = before.media_start + (expected - before.presentation_start) * rate
        if stop > before.media_duration:
            if stop - before.media_duration >= Fraction(rate, before.movie_timescale):
                raise split_timing.TimingError("Original audio edit exceeds its backing samples")
            stop = Fraction(before.media_duration)
        required = ceil(stop) - before.media_start
        if (required <= 0 or after.media_start > before.media_start
            or split_timing.audio_boundary(before, before.media_start) != split_timing.audio_boundary(after, after.media_start)
            or split_timing.audio_boundary(before, before.media_start + required - 1) !=
                split_timing.audio_boundary(after, after.media_start + required - 1)):
            raise split_timing.TimingError("Trimmed audio presentation does not match the source boundary packets")


def allocate_split_destinations(source: Path, count: int) -> list[Path]:
    group = 1
    while True:
        group_suffix = "" if group == 1 else f" ({group})"
        destinations = [
            source.parent / f"{source.stem}_part{part}{group_suffix}{source.suffix}"
            for part in range(1, count + 1)
        ]
        if not any(os.path.lexists(path) for path in destinations):
            return destinations
        group += 1


def split_video(
    source_master: Path,
    processing_input: Path,
    work_dir: Path,
    ffmpeg: str,
    ffprobe: str,
    segment_seconds: int,
    *,
    warnings: list[dict[str, Any]] | None = None,
) -> tuple[list[Path], list[dict[str, Any]]]:
    source_identity = source_master.stat()
    notices = []
    try:
        temporary_parts, timings = prepare_split_parts(
            processing_input, work_dir, ffmpeg, ffprobe, segment_seconds, source_master, notices=notices,
        )
    except MessageError as error:
        message = error.message
        raise SplitPreparationError(message.get("code"), message["raw"], **message.get("params", {})) from error
    except split_timing.TimingError as error:
        raise SplitPreparationError("processing.split_validation_failed", str(error)) from error
    ensure_source_identity(source_master, source_identity)
    outputs = publish_split_parts(source_master, temporary_parts) if temporary_parts else []
    if warnings is not None:
        warnings.extend(notices)
    return outputs, timings


def prepare_n3_parts(processing_input: Path, work_dir: Path, ffmpeg: str, ffprobe: str,
                     segment_seconds: int, source_master: Path, *,
                     cuts: list[Fraction] | None = None) -> tuple[list[Path], list[dict[str, Any]]]:
    # FFmpeg takes integer microseconds. Round down so a fractional key at
    # the requested boundary is eligible. Verify the effective cut separately.
    segment_args = ["-segment_time", str(segment_seconds)]
    if cuts:
        cut_ticks = [int(cut // Fraction(1, 1_000_000)) for cut in cuts]
        segment_args = ["-segment_times", ",".join(f"{tick // 1_000_000}.{tick % 1_000_000:06d}" for tick in cut_ticks)]
    segment_pattern = work_dir / "segment%03d.mp4"
    segment_list = work_dir / "segments.csv"
    run_command(
        [
            ffmpeg,
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-copyts",
            "-i",
            str(processing_input),
            "-map",
            "0:v:0",
            "-c",
            "copy",
            "-f",
            "segment",
            *segment_args,
            "-reference_stream",
            "v:0",
            "-segment_start_number",
            "1",
            "-avoid_negative_ts",
            "disabled",
            "-segment_format_options",
            "avoid_negative_ts=disabled:movie_timescale=1000000",
            "-segment_list",
            str(segment_list),
            "-reset_timestamps",
            "1",
            str(segment_pattern),
        ],
        "processing.split_failed",
    )
    temporary_parts = numeric_parts(work_dir.glob("segment*.mp4"))
    if not temporary_parts:
        raise MessageError("processing.split_failed", "時間分割ファイルが生成されませんでした。")

    if len(temporary_parts) == 1:
        return [], []  # Nothing public yet; caller uses its ordinary non-split path.
    temporary_parts = split_audio_for_video_parts(
        processing_input, temporary_parts, segment_list, work_dir, ffmpeg, ffprobe,
        processing_input != source_master,
    )
    timings = validate_split_parts(source_master, processing_input, temporary_parts, segment_list, ffmpeg, ffprobe)
    return temporary_parts, timings


def prepare_split_parts(processing_input: Path, work_dir: Path, ffmpeg: str, ffprobe: str,
                        segment_seconds: int, source_master: Path, *,
                        notices: list[dict[str, Any]] | None = None) -> tuple[list[Path], list[dict[str, Any]]]:
    parts, timings = prepare_n3_parts(processing_input, work_dir, ffmpeg, ffprobe, segment_seconds, source_master)
    # Provisional. Keep usable N3 parts by their length between actual cut
    # points. Nominal cut points guide the split of an overlong N3 part.
    tolerance = min(Fraction(2), Fraction(segment_seconds * 3, 100))
    limit = segment_seconds + tolerance
    if timings and all(Fraction(str(timing["durationSeconds"])) <= limit for timing in timings):
        return parts, timings
    if not parts and Fraction(probe_duration_decimal(processing_input, ffprobe)) <= limit:
        return parts, timings
    try:
        return improve_split_precision(
            processing_input, work_dir, ffmpeg, ffprobe, segment_seconds, tolerance, source_master, parts, timings,
        )
    except (MessageError, split_timing.TimingError) as error:
        if notices is not None:
            notices.append(error_message(MessageError(
                "processing.split_precision_limited", f"Requested split precision was not verified: {error}",
                seconds=str(segment_seconds),
            )))
        return parts, timings


def precision_cut_plan(starts: list[Fraction], end: Fraction, interval: int, tolerance: Fraction,
                       keys: list[Fraction]) -> list[Fraction]:
    """Return one set of output boundaries, replacing obsolete delayed cuts."""
    planned = [starts[0]]
    limit = interval + tolerance
    replacing_delayed_cut = False
    for n3_start, right in zip(starts, [*starts[1:], end]):
        if replacing_delayed_cut and right - n3_start > limit:
            raise split_timing.TimingError("Replacing a delayed cut would extend into another overlong N3 part")
        left = planned[-1]
        # Prefer the next nominal cut, skipping a grid point already met by
        # a slightly early key. A retained N3 cut can still end a valid part.
        target = ((left + tolerance) // interval + 1) * interval
        inserted = False
        while right - left > limit:
            first = max(bisect_right(keys, left), bisect_left(keys, target - tolerance))
            last = min(bisect_left(keys, right), bisect_right(keys, min(target + tolerance, left + limit)))
            available = keys[first:last]
            left = min(available, key=lambda key: (abs(key - target), key)) if available else Fraction(target)
            planned.append(left)
            inserted = True
            target = ((left + tolerance) // interval + 1) * interval
        obsolete = right != end and inserted and right < target - tolerance
        if obsolete:
            if replacing_delayed_cut:
                raise split_timing.TimingError("Replacing a delayed cut would affect more than its next N3 part")
            replacing_delayed_cut = True
            continue
        replacing_delayed_cut = False
        planned.append(right)
    return planned


def exact_n3_starts(input_path: Path, parts: list[Path], listing: Path, ffprobe: str,
                    origin: Fraction) -> list[Fraction]:
    starts = [Fraction(0)]
    if parts:
        rows = split_listing(parts, listing)
        for part, row in zip(parts[1:], rows[1:]):
            checksum = packet_values(packet_probe(part, ffprobe))[0][1]
            starts.append(packet_at(input_path, ffprobe, Fraction(row[1]), checksum) - origin)
    return starts


def improve_split_precision(processing_input: Path, work_dir: Path, ffmpeg: str, ffprobe: str,
                            interval: int, tolerance: Fraction, source_master: Path,
                            parts: list[Path], timings: list[dict[str, Any]]) -> tuple[list[Path], list[dict[str, Any]]]:
    input_probe = packet_probe(processing_input, ffprobe)
    origin = Fraction(input_probe["format"]["start_time"])
    streams = json.loads(run_command([ffprobe, "-v", "error", "-show_streams", "-of", "json", str(processing_input)],
        "processing.split_timeline_failed").stdout)["streams"]
    audio = [stream for stream in streams if stream["codec_type"] == "audio"]
    movie = split_timing.read_movie(processing_input)
    if (len(movie.videos) != 1 or movie.videos[0].presentation_start != 0 or len(movie.audios) != len(audio)
        or any(stream["codec_type"] not in {"video", "audio"} for stream in streams)):
        raise split_timing.TimingError("Cannot prove a unique zero-based source video timeline")
    video_track = movie.videos[0]
    keys = split_video_encode.sync_times(video_track)
    if not keys or keys[0] != 0 or packet_values(input_probe)[0][0] != origin:
        raise split_timing.TimingError("Cannot prove a zero-based video sync point")
    end = max(track.presentation_end for track in (video_track, *movie.audios))
    starts = exact_n3_starts(processing_input, parts, work_dir / "segments.csv", ffprobe, origin)
    if starts != sorted(set(starts)) or starts[-1] >= end:
        raise split_timing.TimingError("Invalid N3 cut order")
    planned = precision_cut_plan(starts, end, interval, tolerance, keys)
    if planned == [*starts, end]:
        raise split_timing.TimingError("The N3 output duration exceeds the verified cut span")
    precision_dir = work_dir / "precision"
    precision_dir.mkdir()

    windows = list(zip(planned, planned[1:]))
    original = dict(zip(zip(starts, [*starts[1:], end]), zip(parts, timings)))
    sync = set(keys)
    replacement_copies = [span for span in windows if span not in original and span[0] in sync
                          and (span[1] in sync or span[1] == end)]
    if replacement_copies:
        key_dir = precision_dir / "keys"
        key_dir.mkdir()
        verified = [point for point in planned[:-1] if point in sync]
        copied_parts, copied_timings = prepare_n3_parts(
            processing_input, key_dir, ffmpeg, ffprobe, interval, source_master,
            cuts=[point + origin for point in verified[1:]],
        )
        observed = exact_n3_starts(processing_input, copied_parts, key_dir / "segments.csv", ffprobe, origin)
        if observed != verified or len(copied_parts) != len(verified):
            raise split_timing.TimingError("N3 did not split at the selected nearby keyframes")
        copy_windows = dict(zip(zip(observed, [*observed[1:], end]), zip(copied_parts, copied_timings)))
        if not all(span in copy_windows for span in replacement_copies):
            raise split_timing.TimingError("N3 copy output does not match the planned window")
        original.update((span, copy_windows[span]) for span in replacement_copies)

    if all(span in original for span in windows):
        return [original[span][0] for span in windows], [original[span][1] for span in windows]

    # Compare source origins before mapping a newly encoded part.
    source_probe = packet_probe(source_master, ffprobe)
    if (packet_values(source_probe)[0][:2] != packet_values(input_probe)[0][:2]
        or Fraction(source_probe["format"]["start_time"]) != origin or origin != 0):
        raise split_timing.TimingError("Cannot prove the generated Source Timeline offset")
    audio_probe = None
    if audio:
        if len(audio) != 1 or audio[0].get("codec_name") != "aac" or audio[0].get("sample_rate") != "48000":
            raise split_timing.TimingError("Generated video requires a proved single AAC 48 kHz track")
        audio_probe = probe_audio_boundary(processing_input, ffprobe)
    result_parts, result_timings = [], []
    for number, (start, stop) in enumerate(windows, 1):
        if (start, stop) in original:
            part, timing = original[(start, stop)]
        else:
            if start >= video_track.presentation_end:
                raise split_timing.TimingError("No source picture exists at the requested cut")
            encoded = precision_dir / f"segment{number:03d}.mp4"
            generated = split_video_encode.encode_part(
                processing_input, video_track, start, min(stop, video_track.presentation_end),
                encoded, ffmpeg, ffprobe, run_command,
            )
            part = finalize_split_part(
                processing_input, encoded, precision_dir, ffmpeg, ffprobe, movie, audio, audio_probe,
                number, start, stop, final=stop == end, trim_tail=processing_input != source_master and stop == end,
                require_audio_cap=True,
            )
            generated.verify(part)
            timing = {"sourceTimelineOffsetSeconds": float(start), "sourceVideoStartSeconds": float(start),
                "videoStartSeconds": 0.0, "durationSeconds": float(probe_duration_decimal(part, ffprobe)),
                "muxerStartSeconds": float(start), "mappingBasis": "verified-generated-frame-pts"}
        result_parts.append(part)
        result_timings.append(timing)
    return result_parts, result_timings


def split_audio_for_video_parts(processing_input: Path, videos: list[Path], listing: Path,
                                work_dir: Path, ffmpeg: str, ffprobe: str, trimmed: bool = False) -> list[Path]:
    """N3: independent video cuts, retained audio preroll, finite presented range.

    Only private work files are made here. Stored AAC packets may overlap for
    decoder warmup; their MP4 presentation ranges must not. No header editing
    or re-encoding. Use the trimmed input's declared audio end, not just its
    (possibly longer) video duration. Preserve every audio track independently.
    """
    info = json.loads(run_command([ffprobe, "-v", "error", "-show_streams", "-show_format", "-of", "json",
        str(processing_input)], "processing.split_timeline_failed").stdout)
    audio = [s for s in info["streams"] if s["codec_type"] == "audio"]
    if any(s["codec_type"] not in {"video", "audio"} for s in info["streams"]):
        raise MessageError("processing.split_validation_failed", "Unsupported non-audio/video track; source retained")
    rows = split_listing(videos, listing)
    anchor_probe = packet_probe(processing_input, ffprobe)
    anchor = packet_values(anchor_probe)[0]
    origin = Fraction(anchor_probe["format"]["start_time"])
    starts = []
    for index, (video, row) in enumerate(zip(videos, rows)):
        checksum = packet_values(packet_probe(video, ffprobe))[0][1]
        pts = anchor[0] if index == 0 else packet_at(processing_input, ffprobe, Fraction(row[1]), checksum)
        if index == 0 and checksum != anchor[1]:
            raise MessageError("processing.split_timeline_failed", "First video packet differs from input")
        starts.append(pts - origin)
    source_movie = split_timing.read_movie(processing_input)
    if len(source_movie.videos) != 1 or len(source_movie.audios) != len(audio):
        raise split_timing.TimingError("MP4 tracks do not match the split input")
    source_video = source_movie.videos[0]
    source_samples = source_video.samples()
    ends = starts[1:] + [max(track.presentation_end for track in (source_video, *source_movie.audios))]
    source_audio_probe = None
    if len(audio) == 1 and audio[0].get("codec_name") == "aac" and audio[0].get("sample_rate") == "48000":
        source_audio_probe = probe_audio_boundary(processing_input, ffprobe)
    results = []
    offset = 0
    for index, (video, start, end) in enumerate(zip(videos, starts, ends), 1):
        final = index == len(videos)
        trim_tail = trimmed and final
        segment_movie = split_timing.read_movie(video)
        segment = segment_movie.videos[0]
        same_video_packets(source_video, segment, start, source_samples, offset)
        offset += segment.sample_count
        results.append(finalize_split_part(
            processing_input, video, work_dir, ffmpeg, ffprobe, source_movie, audio, source_audio_probe,
            index, start, end, final=final, trim_tail=trim_tail, segment_movie=segment_movie,
        ))
    if offset != source_video.sample_count:
        raise split_timing.TimingError("Split did not preserve the full input video packet sequence")
    return results


def finalize_split_part(processing_input: Path, video: Path, work_dir: Path, ffmpeg: str, ffprobe: str,
                        source_movie: split_timing.Movie, audio: list[dict[str, Any]],
                        source_audio_probe: dict[str, Any] | None, index: int,
                        start: Fraction, end: Fraction, *, final: bool, trim_tail: bool,
                        require_audio_cap: bool = False, segment_movie: split_timing.Movie | None = None) -> Path:
    segment_movie = segment_movie if segment_movie is not None else split_timing.read_movie(video)
    segment = segment_movie.videos[0]
    video_end = min(end, source_movie.videos[0].presentation_end) - start
    tracks = []
    for number, stream in enumerate(audio):
        try:
            base = Fraction(stream["time_base"])
            audio_end = source_movie.audios[number].presentation_end
            duration = min(end, audio_end) - start
            ticks = round(duration / base)
            if base <= 0 or duration <= 0 or ticks <= 0:
                raise ValueError("Empty presented audio range")
        except (KeyError, ValueError, ZeroDivisionError) as error:
            raise MessageError("processing.split_timeline_failed", "Audio presentation range unavailable") from error
        track = work_dir / f"audio-{index}-{number}.mp4"
        run_command([ffmpeg, "-hide_banner", "-loglevel", "error", "-n", "-ss", f"{float(start):.9f}",
            "-i", str(processing_input), "-map", f"0:a:{number}", "-t", f"{float(duration):.9f}",
            "-c", "copy", "-copypriorss:a", "1", "-bsf:a",
            f"setts=duration=min(DURATION\\,max(1\\,{ticks}-PTS))", "-avoid_negative_ts", "disabled",
            "-use_editlist", "1", "-movie_timescale", "1000000", str(track)], "processing.split_failed")
        tracks.append(track)
    audio_movies = [split_timing.read_movie(track) for track in tracks]
    cap = None
    if source_audio_probe is not None:
        try:
            cap = split_timing.audio_cap(
                source_movie, audio_movies[0], start, min(end, source_movie.audios[0].presentation_end),
                source_audio_probe, probe_audio_boundary(tracks[0], ffprobe),
            )
        except split_timing.TimingError:
            # Unproved AAC timing is not a license to shorten audio.
            # The ordinary audio still has to pass output validation.
            pass

    if require_audio_cap and tracks and cap is None:
        raise split_timing.TimingError("Cannot prove AAC presentation for the new split boundary")
    if final and cap is not None:
        video_end = max(video_end, Fraction(cap.ticks, 48000))
    if not tracks and near_movie_end(segment_movie, video_end):
        return video

    combined = work_dir / "av-parts" / video.name
    # Without an AAC cap the mux determines the actual audio endpoint.
    # Observe it before changing the final video if Method B needs a hold.
    video_filter = None if final and tracks and cap is None else video_end_filter(
        video, segment_movie, video_end, ffprobe,
    )
    mux_split_part(video, tracks, combined, ffmpeg, video_filter, cap.filter if cap is not None else None)
    output_movie = split_timing.read_movie(combined)
    if final and tracks and cap is None:
        video_end = max(video_end, *(track.presentation_end for track in output_movie.audios))
    if video_filter is None and not near_movie_end(output_movie, video_end):
        # Corrections always use the original segment input. A second
        # unguarded mux of a corrected file can reintroduce the overrun.
        video_filter = video_end_filter(video, segment_movie, video_end, ffprobe, force=True)
        retry = work_dir / "corrected-parts" / video.name
        mux_split_part(video, tracks, retry, ffmpeg, video_filter, cap.filter if cap is not None else None)
        combined.unlink()
        combined = retry
        output_movie = split_timing.read_movie(combined)
    expected = video_end
    if video_filter is not None:
        expected = (video_end * segment.timescale).numerator // (video_end * segment.timescale).denominator
        expected = Fraction(expected, segment.timescale)
    if not near_movie_end(output_movie, expected):
        raise split_timing.TimingError("Final MP4 video edit does not match the verified end")
    same_video_packets(segment, output_movie.videos[0], Fraction(0), segment.samples())
    if segment.sample_count != output_movie.videos[0].sample_count:
        raise split_timing.TimingError("Final mux changed the video sample count")
    if cap is not None:
        cap.verify(output_movie, probe_audio_boundary(combined, ffprobe))
    validate_audio_out(source_movie, audio_movies, output_movie, start, end, cap is not None,
                       strict_end=trim_tail, source_probe=source_audio_probe)
    return combined


def probe_audio_boundary(path: Path, ffprobe: str) -> dict[str, Any]:
    return json.loads(run_command(
        split_timing.audio_probe_command(ffprobe, path), "processing.split_validation_failed",
    ).stdout)


def near_movie_end(movie: split_timing.Movie, expected: Fraction) -> bool:
    if len(movie.videos) != 1 or movie.videos[0].presentation_start != 0:
        raise split_timing.TimingError("No unique zero-based video edit")
    return abs(movie.videos[0].presentation_end - expected) < Fraction(1, movie.timescale)


def same_video_packets(source: split_timing.Track, candidate: split_timing.Track,
                       start: Fraction, source_samples: Iterator[tuple[int, int, int, int]],
                       offset: int = 0) -> None:
    """Prove the copied sample span and its presentation and decode clocks.

    Check each sample's size and timestamps from metadata and hash the first
    and last presented access units, without reading every encoded frame.
    """
    if offset + candidate.sample_count > source.sample_count or candidate.presentation_start != 0:
        raise split_timing.TimingError("Invalid copied video sample span")
    parent = islice(source_samples, candidate.sample_count)
    left_scale = source.timescale
    right_scale = candidate.timescale
    shift = (start - source.presentation_start) * left_scale * right_scale
    shift += source.media_start * right_scale - candidate.media_start * left_scale
    if shift.denominator != 1:
        raise split_timing.TimingError("Video edit and packet clocks do not align")
    shift = int(shift)
    latest = None
    compared = 0
    for (left_index, left_dts, left_pts, _), (right_index, right_dts, right_pts, _) in zip(parent, candidate.samples()):
        if (left_pts * right_scale - right_pts * left_scale != shift
            or left_dts * right_scale - right_dts * left_scale != shift
            or source.sample_size(left_index) != candidate.sample_size(right_index)):
            raise split_timing.TimingError("Copied video packet size or PTS/DTS changed")
        if latest is None or right_pts > latest[0]:
            latest = (right_pts, left_index, right_index)
        compared += 1
    if (compared != candidate.sample_count or latest is None or source.packet(offset) != candidate.packet(0)
        or source.packet(latest[1]) != candidate.packet(latest[2])):
        raise split_timing.TimingError("Copied first or last video packet did not match")
    last_start = Fraction(latest[0] - candidate.media_start, right_scale)
    if not 0 <= last_start < candidate.presentation_end:
        raise split_timing.TimingError("Final video edit does not present the last copied picture")


def video_end_filter(path: Path, movie: split_timing.Movie, end: Fraction, ffprobe: str,
                     *, force: bool = False) -> str | None:
    if near_movie_end(movie, end) and not force:
        return None
    video = movie.videos[0]
    result = run_command([ffprobe, "-v", "error", "-select_streams", "v:0", "-show_packets", "-show_streams",
        "-show_entries", "stream=time_base:packet=pts,dts,duration,flags", "-of", "json", str(path)],
        "processing.split_validation_failed")
    probe = json.loads(result.stdout)
    try:
        base = Fraction(probe["streams"][0]["time_base"])
        target = int(end // base)
        packets = probe["packets"]
        if base != Fraction(1, video.timescale) or len(packets) != video.sample_count or target >= 2**52:
            raise ValueError("Unsupported video clock or sample count")
        previous = -2**52
        for packet, (_, dts, pts, _) in zip(packets, video.samples()):
            if (packet["pts"] != pts - video.media_start or packet["dts"] != dts - video.media_start
                or not previous < packet["dts"] < 2**52 or not -2**52 < packet["pts"] < target
                or not 0 < packet["duration"] < 2**52 or "D" in packet["flags"]):
                raise ValueError("Video filter timestamps or durations could not be proved")
            previous = packet["dts"]
    except (IndexError, KeyError, TypeError, ValueError, ZeroDivisionError) as error:
        raise split_timing.TimingError(str(error)) from error
    return (f"setts=pts=PTS:dts=DTS:duration=if(eq(DTS\\,{previous})\\,{target}-PTS\\,"
            f"min(DURATION\\,{target}-PTS))")


def mux_split_part(video: Path, tracks: list[Path], output: Path, ffmpeg: str,
                   video_filter: str | None, audio_filter: str | None) -> None:
    output.parent.mkdir(exist_ok=True)
    filters = []
    if video_filter is not None:
        filters += ["-bsf:v", video_filter]
    if audio_filter is not None:
        filters += ["-bsf:a", audio_filter]
    run_command([ffmpeg, "-hide_banner", "-loglevel", "error", "-n", "-copyts", "-i", str(video),
        *[arg for track in tracks for arg in ["-i", str(track)]], "-map", "0:v:0",
        *[arg for number in range(1, len(tracks) + 1) for arg in ["-map", f"{number}:a:0"]],
        "-c", "copy", "-avoid_negative_ts", "disabled", "-use_editlist", "1", "-movie_timescale", "1000000",
        *filters, str(output)], "processing.split_failed")


def validate_audio_out(source: split_timing.Movie, intermediates: list[split_timing.Movie],
                       output: split_timing.Movie, start: Fraction, end: Fraction, bounded: bool,
                       *, strict_end: bool = False, source_probe: dict[str, Any] | None = None) -> None:
    if len(output.audios) != len(source.audios) or len(intermediates) != len(source.audios):
        raise split_timing.TimingError("Final mux changed the audio track count")
    for original, intermediate, actual in zip(source.audios, intermediates, output.audios):
        cut = intermediate.audios[0]
        split_timing.verify_audio_copy(original, cut, actual, start, end, bounded, strict_end=strict_end,
                                       aac_source=(source, source_probe) if source_probe is not None else None)


def split_listing(parts: list[Path], listing: Path) -> list[list[str]]:
    rows = list(csv.reader(listing.read_text(encoding="utf-8").splitlines()))
    if len(rows) != len(parts) or any(len(row) != 3 for row in rows) or [Path(row[0]).name for row in rows] != [p.name for p in parts]:
        raise MessageError("processing.split_timeline_failed", "Segment list and numeric output order disagree")
    return rows


def validate_video_start(part: Path, ffmpeg: str) -> None:
    # Exit 0 with ZERO video frames used to pass a part with a 2s empty edit.
    decoded = run_command([ffmpeg, "-v", "error", "-xerror", "-copyts", "-i", str(part), "-map", "0:v:0",
        "-t", "1", "-fps_mode", "passthrough", "-f", "framemd5", "-"], "processing.split_validation_failed").stdout
    try:
        base = Fraction(re.search(r"^#tb 0:\s*(\S+)", decoded, re.MULTILINE)[1])
        frames = [row.split(",") for row in decoded.splitlines() if row and not row.startswith("#")]
        if not frames or abs(int(frames[0][2]) * base) > Fraction(1, 1_000_000):
            raise ValueError("No video presented at the beginning of the part")
    except (TypeError, IndexError, ValueError, ZeroDivisionError) as error:
        raise MessageError("processing.split_validation_failed", "No verified initial video frame") from error


def numeric_parts(paths) -> list[Path]:
    def number(path):
        match = re.fullmatch(r"segment(\d+)\.mp4", path.name)
        if not match:
            raise MessageError("processing.split_validation_failed", "Unexpected segment file name")
        return int(match[1])
    return sorted(paths, key=number)


def packet_probe(path: Path, ffprobe: str, interval: str = "%+#1") -> dict[str, Any]:
    result = run_command([ffprobe, "-v", "error", "-select_streams", "v:0", "-read_intervals", interval,
        "-show_packets", "-show_streams", "-show_format", "-show_data_hash", "sha256", "-show_entries",
        "format=start_time:stream=time_base:packet=pts,flags,data_hash", "-of", "json", str(path)], "processing.split_timeline_failed")
    return json.loads(result.stdout)


def packet_values(probe: dict[str, Any]) -> list[tuple[Fraction, str, str]]:
    try:
        base = Fraction(probe["streams"][0]["time_base"])
        return [(int(p["pts"]) * base, p["data_hash"], p["flags"]) for p in probe["packets"]]
    except (KeyError, ValueError, IndexError, ZeroDivisionError) as error:
        raise MessageError("processing.split_timeline_failed", "Video packet PTS/hash is unavailable") from error


def packet_at(path: Path, ffprobe: str, timestamp: Fraction, checksum: str) -> Fraction:
    # The muxer's CSV is rounded to microseconds. It only locates a packet;
    # the value persisted below comes from the observed integer PTS/time_base.
    start = max(0.0, float(timestamp) - 1)
    end = float(timestamp) + 1
    probe = packet_probe(path, ffprobe, f"{start:.6f}%{end:.6f}")
    matches = [pts for pts, digest, _ in packet_values(probe)
        if digest == checksum and abs(pts - timestamp) <= Fraction(1, 1_000_000)]
    if len(matches) != 1:
        raise MessageError("processing.split_timeline_failed", f"No unique source packet at {timestamp}: {path}")
    return matches[0]


def validate_split_parts(source: Path, processing_input: Path, parts: list[Path], listing: Path,
                         ffmpeg: str, ffprobe: str) -> list[dict[str, Any]]:
    source_probe = packet_probe(source, ffprobe)
    input_probe = packet_probe(processing_input, ffprobe)
    source_anchor = packet_values(source_probe)[0]
    input_anchor = packet_values(input_probe)[0]
    if source_anchor[1] != input_anchor[1]:
        raise MessageError("processing.split_timeline_failed", "Trimmed input does not retain the first source video packet")
    source_origin = Fraction(source_probe["format"]["start_time"])
    rows = split_listing(parts, listing)
    source_streams = media_streams(processing_input, ffprobe)
    timings = []
    for index, (part, row) in enumerate(zip(parts, rows)):
        part_streams = media_streams(part, ffprobe)
        if part_streams != source_streams:
            raise MessageError("processing.split_validation_failed", "Video/audio streams changed during segmentation")
        duration = probe_duration_decimal(part, ffprobe)
        validate_video_start(part, ffmpeg)
        if any(stream[0] == "audio" for stream in part_streams):
            run_command([ffmpeg, "-v", "error", "-xerror", "-i", str(part), "-map", "0:a",
                "-t", "1", "-f", "null", "-"], "processing.split_validation_failed")
        part_probe = packet_probe(part, ffprobe)
        local_pts, checksum, flags = packet_values(part_probe)[0]
        if "K" not in flags:
            raise MessageError("processing.split_validation_failed", "A segment does not start on a video keyframe")
        input_pts = input_anchor[0] if index == 0 else packet_at(processing_input, ffprobe, Fraction(row[1]), checksum)
        source_pts = packet_at(source, ffprobe, input_pts + source_anchor[0] - input_anchor[0], checksum)
        if index == 0 and checksum != input_anchor[1]:
            raise MessageError("processing.split_timeline_failed", "First split packet differs from its source")
        local_origin = Fraction(part_probe["format"]["start_time"])
        offset = source_pts - source_origin - (local_pts - local_origin)
        timings.append({"sourceTimelineOffsetSeconds": float(offset), "sourceVideoStartSeconds": float(source_pts - source_origin),
            "videoStartSeconds": float(local_pts - local_origin), "durationSeconds": float(duration),
            "muxerStartSeconds": float(Decimal(row[1])), "mappingBasis": "matching-video-packet-pts-sha256"})
    return timings


def media_streams(path: Path, ffprobe: str) -> list[tuple]:
    result = run_command([ffprobe, "-v", "error", "-show_streams", "-show_entries",
        "stream=codec_type,codec_name,width,height,sample_rate,channels", "-of", "json", str(path)], "processing.split_validation_failed")
    streams = json.loads(result.stdout)["streams"]
    if sum(s["codec_type"] == "video" for s in streams) != 1:
        raise MessageError("processing.split_validation_failed", "Exactly one video stream is required for verified splitting")
    return [(s["codec_type"], s.get("codec_name"), s.get("width"), s.get("height"), s.get("sample_rate"), s.get("channels")) for s in streams]


def publish_split_parts(source_master: Path, temporary_parts: list[Path]) -> list[Path]:
    while True:
        destinations = allocate_split_destinations(source_master, len(temporary_parts))
        published: list[Path] = []
        try:
            for temporary, destination in zip(temporary_parts, destinations):
                publish_without_overwrite(temporary, destination)
                published.append(destination)
            return published
        except Exception as error:
            if isinstance(error, MessageError) and not published and error.message.get("code") == "processing.output_exists":
                continue
            if not isinstance(error, MessageError):
                error = MessageError("processing.split_failed", f"{type(error).__name__}: {error}")
            error.message["raw"] += f"; partial outputs retained: {[str(p) for p in published]}"
            raise error  # Never delete published files during rollback.


def format_duration(seconds: float) -> str:
    total = max(0, int(seconds))
    hours, remainder = divmod(total, 3600)
    minutes, seconds = divmod(remainder, 60)
    return f"{hours:02d}:{minutes:02d}:{seconds:02d}"


def transcribe_source_master(
    video: Path,
    work_dir: Path,
    ffmpeg: str,
    ffprobe: str,
    transcriber: Path,
    locale: str,
) -> tuple[Path, str]:
    transcript_work = Path(tempfile.mkdtemp(prefix="transcript_", dir=work_dir))
    audio = transcript_work / "audio.wav"
    raw = transcript_work / "raw.txt"
    completed = transcript_work / "completed.txt"
    run_command(
        [
            ffmpeg,
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(video),
            "-map",
            "0:a:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            str(audio),
        ],
        "transcription.audio_extract_failed",
    )
    recognition = run_command(
        [str(transcriber), str(audio), str(raw), "--locale", locale],
        "transcription.failed",
    )
    try:
        actual_locale = json.loads(recognition.stdout)["locale"]
    except (ValueError, KeyError, TypeError) as error:
        raise MessageError("transcription.locale_invalid", "Speech helper did not confirm its recognition locale") from error
    if actual_locale != locale:
        raise MessageError("transcription.locale_invalid", f"Requested {locale}, helper reported {actual_locale}")
    if not raw.is_file():
        raise MessageError("transcription.empty", "Apple Speechの文字起こし結果が空でした。")

    duration = probe_duration(video, ffprobe)
    body = raw.read_text(encoding="utf-8").strip()
    if not body:
        raise MessageError("transcription.empty", "Apple Speechの文字起こし結果が空でした。")
    completed.write_text(
        f"動画名：{video.name}\n"
        f"Source Timeline：00:00:00〜{format_duration(duration)}\n\n"
        f"{body}\n",
        encoding="utf-8",
    )
    return publish_allocated(completed, video.parent, f"{video.stem}_文字起こし", ".txt"), actual_locale


def recognition_locale(value: Any) -> str:
    if value not in ("ja-JP", "en-US"):
        raise MessageError("transcription.locale_invalid", f"Unsupported product recognition locale: {value}")
    return value


def read_manifest(path: str) -> tuple[Path, dict[str, Any]]:
    requested = Path(path).expanduser()
    if requested.is_symlink() or (requested.parent.name == ".capturescribe" and requested.parent.is_symlink()):
        raise MessageError("processing.input_failed", "Manifest and management directory must not be symbolic links")
    resolved = requested.resolve()
    return resolved, json.loads(resolved.read_text(encoding="utf-8"))


def source_master_from_manifest(manifest: dict[str, Any], manifest_path: Path | None = None) -> Path:
    if manifest.get("schemaVersion") != 2:
        raise MessageError("manifest.unsupported", "未対応の録画manifest schemaです。", version=str(manifest.get("schemaVersion")))
    for artifact in manifest.get("artifacts", []):
        if artifact.get("role") == "source_master" and artifact.get("state") == "validated":
            stored = Path(artifact["path"]).expanduser()
            source = stored.resolve()
            if source.is_file():
                if manifest_path is not None and manifest_path.parent.name == ".capturescribe":
                    filename = manifest_path.name
                    stem = filename.removesuffix(".capture.json")
                    expected = manifest_path.parent.parent / f"{stem}.mp4"
                    stat = source.stat()
                    physical_id = (f"unix:{stat.st_dev}:{stat.st_ino}" if os.name == "posix"
                                   else f"metadata:{stat.st_size}:{stat.st_mtime_ns}")
                    if (stem == filename or stored != expected or source != expected
                            or artifact.get("physicalIdentity") != physical_id):
                        raise MessageError("processing.source_unvalidated", "Managed Source Master location or identity changed")
                return source
            raise MessageError("processing.source_missing", f"Source Masterが見つかりません: {source}", path=str(source))
    raise MessageError("processing.source_unvalidated", "検証済みSource Masterがmanifestにありません。")


def ensure_source_identity(source: Path, identity: os.stat_result) -> None:
    try:
        stat = source.stat()
    except OSError as error:
        raise MessageError("processing.source_missing", f"Source Master is unavailable: {source}", path=str(source)) from error
    if (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns) != (
        identity.st_dev, identity.st_ino, identity.st_size, identity.st_mtime_ns,
    ):
        raise MessageError("processing.source_unvalidated", "Source Master changed during private processing")


def process(args: argparse.Namespace) -> dict[str, Any]:
    context = None
    run_id = getattr(args, "run_id", None)
    receipt_directory = getattr(args, "receipt_directory", None)
    if run_id or receipt_directory:
        if not run_id or not re.fullmatch(r"[A-Za-z0-9-]{1,160}", run_id) or not receipt_directory:
            raise MessageError("processing.receipt_failed", "Invalid publication receipt context")
        directory = Path(receipt_directory)
        if not directory.is_absolute() or directory.is_symlink() or not directory.is_dir():
            raise MessageError("processing.receipt_failed", "Invalid publication receipt directory")
        path, manifest = read_manifest(args.manifest)
        source = source_master_from_manifest(manifest, path)
        source_info = next(a for a in manifest["artifacts"] if a.get("role") == "source_master" and a.get("state") == "validated")
        if (getattr(args, "expected_job_id", None) != manifest.get("jobId")
                or getattr(args, "expected_source_master", None) != str(source)
                or getattr(args, "expected_source_identity", None) != source_info.get("physicalIdentity")):
            raise MessageError("processing.input_failed", "Recording identity changed before worker execution")
        context = {"directory": directory, "runId": run_id, "jobId": manifest.get("jobId"),
            "manifestPath": str(path), "sourceMaster": str(source), "manifest": manifest}
    token = publication_context.set(context)
    try:
        return process_media(args)
    finally:
        publication_context.reset(token)


def process_media(args: argparse.Namespace) -> dict[str, Any]:
    threshold = split_minutes(args.split_threshold_minutes)
    segment = split_minutes(args.split_segment_minutes)
    locale = recognition_locale(getattr(args, "transcription_locale", None)) if args.transcribe else None
    transcription = {"requestedLocale": locale, "usedLocales": []} if args.transcribe else None
    started = time.monotonic()
    progress("processing.source_reading")
    context = publication_context.get()
    if context:
        manifest_path, manifest = Path(context["manifestPath"]), context["manifest"]
    else:
        manifest_path, manifest = read_manifest(args.manifest)
    source_master = source_master_from_manifest(manifest, manifest_path)
    source_identity = source_master.stat()
    source_duration = probe_duration(source_master, args.ffprobe)
    artifacts: list[dict[str, Any]] = []
    warnings: list[dict[str, Any]] = []

    with tempfile.TemporaryDirectory(
        prefix=f".{source_master.stem}_processing_",
        dir=source_master.parent,
    ) as directory:
        work_dir = Path(directory)
        # Keep only private video intermediates here. Public outputs and
        # receipts are outside this directory; transcription needs the source.
        with tempfile.TemporaryDirectory(prefix="video-", dir=work_dir) as video_directory:
            video_work = Path(video_directory)
            processing_input = source_master
            trim_result: dict[str, Any] = {"status": "disabled"}
            rejected_trim = False
            if args.trim:
                progress("processing.silence_analyzing")
                processing_input, trim_result = trim_trailing_silence(
                    source_master,
                    video_work,
                    source_duration,
                    args.ffmpeg,
                )
                if trim_result["status"] == "skipped":
                    warnings.append(error_message(MessageError("processing.silence_skipped", trim_result.get("raw") or "無音検出に失敗したため末尾処理をスキップしました。")))
                elif trim_result["status"] == "created":
                    try:
                        validate_trim_output(source_master, processing_input, trim_result["trimEndSeconds"])
                    except split_timing.TimingError as error:
                        ensure_source_identity(source_master, source_identity)
                        rejected_trim = True
                        processing_input = source_master
                        trim_result.update(status="skipped", reason="unsafe_timing")
                        warnings.append(error_message(MessageError("processing.trim_validation_failed", str(error))))

            processed_duration = probe_duration_decimal(processing_input, args.ffprobe)
            video_outputs: list[Path] = []
            split_timings: list[dict[str, Any]] = []
            split_result: dict[str, Any] = {
                "status": "not_needed" if args.split else "disabled",
                "thresholdSeconds": threshold * 60,
                "segmentSeconds": segment * 60,
                "targetDurationSeconds": str(processed_duration),
            }
            if rejected_trim and args.split:
                split_result.update(status="skipped", reason="unsafe_trim")
                warnings.append(error_message(MessageError(
                    "processing.split_skipped_after_trim", "Split was skipped because the trim output could not be verified.",
                )))
            elif should_split(args.split, processed_duration, threshold, segment):
                progress("processing.splitting", threshold=threshold, segment=segment)
                warning_count = len(warnings)
                try:
                    video_outputs, split_timings = split_video(
                        source_master, processing_input, video_work, args.ffmpeg, args.ffprobe, segment * 60,
                        warnings=warnings,
                    )
                except SplitPreparationError as error:
                    ensure_source_identity(source_master, source_identity)
                    warnings.append(error_message(error))
                    split_result.update(status="skipped", reason="unsafe_split")
                    if trim_result["status"] == "created":
                        trim_result.update(status="skipped", reason="split_failed")
                        warnings.append(error_message(MessageError(
                            "processing.trim_not_saved_after_split", "The verified trim was not published after the split failed.",
                        )))
                    processing_input = source_master
                else:
                    if video_outputs:
                        split_result.update(status="created", outputCount=len(video_outputs),
                            parts=[dict(path=str(path), **timing) for path, timing in zip(video_outputs, split_timings)])
                    else:
                        split_result["reason"] = "single_segment"
                    if len(warnings) != warning_count:
                        split_result["precision"] = "limited"
                        if not video_outputs:
                            split_result.update(status="skipped", reason="precision_unavailable")
            if not video_outputs:
                progress("processing.video_finalizing")
                if processing_input == source_master:
                    video_outputs = [source_master]
                else:
                    ensure_source_identity(source_master, source_identity)
                    destination = publish_allocated(
                        processing_input,
                        source_master.parent,
                        f"{source_master.stem}_processed",
                        source_master.suffix,
                    )
                    video_outputs = [destination]

        offsets = {str(path): timing["sourceTimelineOffsetSeconds"] for path, timing in zip(video_outputs, split_timings)}
        for path in video_outputs:
            if path != source_master:
                artifacts.append(
                    {
                        "role": "user_output",
                        "path": str(path),
                        "state": "available",
                        "sourceTimelineOffsetSeconds": offsets.get(str(path), 0.0),
                    }
                )

        transcripts: list[dict[str, str]] = []
        if args.transcribe:
            transcriber = Path(args.transcriber).expanduser().resolve()
            if not transcriber.is_file():
                warnings.append(error_message(MessageError("transcription.helper_missing", f"Apple Speech helperが見つかりません: {transcriber}", path=str(transcriber))))
            else:
                # Recognition always uses the validated original, not a trimmed
                # input or the list/count/timeline offsets of split outputs.
                progress("transcription.full_source", fileName=source_master.name)
                try:
                    transcript, actual_locale = transcribe_source_master(
                        source_master, work_dir, args.ffmpeg, args.ffprobe, transcriber, locale,
                    )
                except Exception as error:  # Preserve completed video outputs.
                    message = error_message(error)
                    message.setdefault("params", {})["fileName"] = source_master.name
                    warnings.append(message)
                else:
                    transcripts.append({"videoPath": str(source_master), "path": str(transcript), "recognitionLocale": actual_locale})
                    transcription["usedLocales"].append(actual_locale)
                    artifacts.append(
                        {
                            "role": "transcript",
                            "path": str(transcript),
                            "state": "available",
                            "sourceTimelineOffsetSeconds": 0.0,
                        }
                    )

    progress("processing.result_finalizing")
    return {
        "status": "partial" if warnings else "completed",
        "manifestPath": str(manifest_path),
        "sourceMaster": str(source_master),
        "sourceDurationSeconds": source_duration,
        "videoOutputs": [str(path) for path in video_outputs],
        "transcripts": transcripts,
        "transcription": transcription,
        "trim": trim_result,
        "split": split_result,
        "artifacts": artifacts,
        "warnings": warnings,
        "elapsedSeconds": time.monotonic() - started,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    parser.add_argument("--transcriber", default="")
    parser.add_argument("--trim", action="store_true")
    parser.add_argument("--split", action="store_true")
    parser.add_argument("--split-threshold-minutes", type=int, required=True)
    parser.add_argument("--split-segment-minutes", type=int, required=True)
    parser.add_argument("--transcribe", action="store_true")
    parser.add_argument("--transcription-locale")
    parser.add_argument("--run-id")
    parser.add_argument("--receipt-directory")
    parser.add_argument("--expected-job-id")
    parser.add_argument("--expected-source-master")
    parser.add_argument("--expected-source-identity")
    return parser.parse_args()


def main() -> int:
    try:
        result = process(parse_args())
    except Exception as error:
        log(json.dumps({"event": "error", "message": error_message(error)}, ensure_ascii=False))
        return 1
    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
