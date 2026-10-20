#!/usr/bin/env python3
"""Transactional Geosweep database snapshots on GitHub Releases.

Snapshot payloads are immutable release assets; the small release-body pointer
on db-latest is the only mutable reference. The old single-file asset on
that release remains readable for backwards compatibility.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import threading
from datetime import datetime, timezone
from contextlib import closing

FORMAT = "geosweep-snapshot-v1"
POINTER = "GEOSWEEP_SNAPSHOT_V1="
PART_PREFIX = "geosweep.db.zst.part-"
MANIFEST = "snapshot.json"
CHUNK_BYTES = 1024**3
REQUIRED_TABLES = {"queue", "caches", "cache_versions", "meta"}
TAG_PATTERN = re.compile(r"db-snapshot-\d{8}-\d+-\d+\Z")


class SnapshotError(Exception):
    """Snapshot is unavailable, malformed, or incomplete."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as src:
        for chunk in iter(lambda: src.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def database_check(path: Path) -> None:
    if not path.is_file() or path.stat().st_size < 100_000:
        raise SnapshotError("SQLite database is missing or too small")
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=30)) as db:
        db.execute("PRAGMA busy_timeout=30000")
        tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not REQUIRED_TABLES <= tables:
            raise SnapshotError(f"SQLite tables missing: {sorted(REQUIRED_TABLES - tables)}")
        if db.execute("PRAGMA quick_check").fetchall() != [("ok",)]:
            raise SnapshotError("SQLite quick_check failed")


def build_snapshot(db: Path, outdir: Path, *, level: int = 10, chunk_bytes: int = CHUNK_BYTES,
                   threads: int = 0) -> dict:
    if not (1 <= level <= 19):
        raise ValueError("Zstandard level must be 1..19")
    if chunk_bytes <= 0:
        raise ValueError("chunk_bytes must be positive")
    database_check(db)
    outdir.mkdir(parents=True, exist_ok=True)
    if any(outdir.iterdir()):
        raise SnapshotError("snapshot output directory must be empty")
    source_bytes = db.stat().st_size
    source_digest = sha256_file(db)
    command = ["zstd", f"-{level}", f"-T{threads}", "-q", "-c", str(db)]
    proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    parts = []
    total = 0
    combined = hashlib.sha256()
    outfile = None
    try:
        assert proc.stdout is not None
        for block in iter(lambda: proc.stdout.read(1024 * 1024), b""):
            offset = 0
            while offset < len(block):
                if outfile is None:
                    name = f"{PART_PREFIX}{len(parts):04d}"
                    path = outdir / name
                    outfile = path.open("xb")
                    parts.append({"name": name, "size": 0, "sha256": hashlib.sha256()})
                part = parts[-1]
                n = min(len(block) - offset, chunk_bytes - part["size"])
                data = memoryview(block)[offset:offset + n]
                outfile.write(data)
                part["sha256"].update(data)
                combined.update(data)
                part["size"] += n
                total += n
                offset += n
                if part["size"] == chunk_bytes:
                    outfile.close()
                    outfile = None
        if outfile:
            outfile.close()
            outfile = None
        assert proc.stderr is not None
        stderr = proc.stderr.read().decode(errors="replace")
        result = proc.wait()
        proc.stdout.close()
        proc.stderr.close()
        if result != 0:
            raise SnapshotError(f"zstd compression failed: {stderr[-500:]}")
        if not total or not parts:
            raise SnapshotError("zstd produced empty archive")
        manifest = {
            "format": FORMAT,
            "compression": "zstd",
            "level": level,
            "database_bytes": source_bytes,
            "database_sha256": source_digest,
            "compressed_bytes": total,
            "compressed_sha256": combined.hexdigest(),
            "chunk_bytes": chunk_bytes,
            "parts": [{"name": p["name"], "size": p["size"],
                       "sha256": p["sha256"].hexdigest()} for p in parts],
        }
        (outdir / MANIFEST).write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        return manifest
    except BaseException:
        if outfile:
            outfile.close()
        if proc.poll() is None:
            proc.kill()
        proc.wait()
        if proc.stdout:
            proc.stdout.close()
        if proc.stderr:
            proc.stderr.close()
        for part in parts:
            (outdir / part["name"]).unlink(missing_ok=True)
        raise


def parse_manifest(directory: Path) -> dict:
    try:
        manifest = json.loads((directory / MANIFEST).read_text())
    except (ValueError, OSError) as exc:
        raise SnapshotError(f"missing or invalid snapshot manifest: {exc}") from exc
    if not isinstance(manifest, dict) or manifest.get("format") != FORMAT or manifest.get("compression") != "zstd":
        raise SnapshotError("unsupported snapshot format/compression")
    parts = manifest.get("parts")
    if not isinstance(parts, list) or not parts or len(parts) > 10000:
        raise SnapshotError("invalid snapshot parts list")
    for key in ("database_bytes", "compressed_bytes", "chunk_bytes"):
        if type(manifest.get(key)) is not int or manifest[key] <= 0:
            raise SnapshotError(f"invalid manifest {key}")
    for key in ("database_sha256", "compressed_sha256"):
        if not isinstance(manifest.get(key), str) or not re.fullmatch(r"[0-9a-f]{64}", manifest[key]):
            raise SnapshotError(f"invalid manifest {key}")
    for index, part in enumerate(parts):
        if not isinstance(part, dict) or part.get("name") != f"{PART_PREFIX}{index:04d}":
            raise SnapshotError("unexpected part name or order")
        if type(part.get("size")) is not int or not (0 < part["size"] <= manifest["chunk_bytes"]):
            raise SnapshotError("invalid part size")
        if not isinstance(part.get("sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", part["sha256"]):
            raise SnapshotError("invalid part digest")
        if index < len(parts) - 1 and part["size"] != manifest["chunk_bytes"]:
            raise SnapshotError("non-final chunk is short")
    if sum(p["size"] for p in parts) != manifest["compressed_bytes"]:
        raise SnapshotError("compressed byte count does not match parts")
    return manifest


def verify_parts(directory: Path, manifest: dict) -> None:
    overall = hashlib.sha256()
    for part in manifest["parts"]:
        path = directory / part["name"]
        if not path.is_file() or path.stat().st_size != part["size"]:
            raise SnapshotError(f"missing or incorrect part size: {part['name']}")
        digest = hashlib.sha256()
        with path.open("rb") as src:
            for block in iter(lambda: src.read(1024 * 1024), b""):
                digest.update(block)
                overall.update(block)
        if digest.hexdigest() != part["sha256"]:
            raise SnapshotError(f"part hash mismatch: {part['name']}")
    if overall.hexdigest() != manifest["compressed_sha256"]:
        raise SnapshotError("compressed archive hash mismatch")


def decompress_verified(parts: list[Path], tmp_path: Path, expected_bytes: int | None,
                        expected_hash: str | None) -> None:
    proc = subprocess.Popen(["zstd", "-q", "-d", "-c"], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    feed_error = []

    def feed() -> None:
        try:
            assert proc.stdin is not None
            for part in parts:
                with part.open("rb") as src:
                    for block in iter(lambda: src.read(1024 * 1024), b""):
                        proc.stdin.write(block)
            proc.stdin.close()
        except (OSError, BrokenPipeError) as exc:
            feed_error.append(exc)
            if proc.stdin:
                proc.stdin.close()

    writer = threading.Thread(target=feed)
    writer.start()
    count = 0
    digest = hashlib.sha256()
    assert proc.stdout is not None
    with tmp_path.open("wb") as out:
        for block in iter(lambda: proc.stdout.read(1024 * 1024), b""):
            out.write(block)
            digest.update(block)
            count += len(block)
    writer.join()
    assert proc.stderr is not None
    stderr = proc.stderr.read().decode(errors="replace")
    result = proc.wait()
    proc.stdout.close()
    proc.stderr.close()
    if result or feed_error:
        raise SnapshotError(f"zstd extraction failed: {stderr[-500:]} {feed_error}")
    if expected_bytes is not None and count != expected_bytes:
        raise SnapshotError("restored SQLite size mismatch")
    if expected_hash is not None and digest.hexdigest() != expected_hash:
        raise SnapshotError("restored SQLite hash mismatch")


def restore(directory: Path, destination: Path, *, legacy: bool = False) -> None:
    if legacy:
        archive = directory / "geosweep.db.zst"
        if not archive.is_file():
            raise SnapshotError("legacy archive missing")
        parts, dbbytes, dbhash = [archive], None, None
    else:
        manifest = parse_manifest(directory)
        verify_parts(directory, manifest)
        parts = [directory / p["name"] for p in manifest["parts"]]
        dbbytes = manifest["database_bytes"]
        dbhash = manifest["database_sha256"]
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=f".{destination.name}.", suffix=".tmp",
                               dir=destination.parent)
    os.close(fd)
    tmp = Path(name)
    try:
        decompress_verified(parts, tmp, dbbytes, dbhash)
        database_check(tmp)
        os.replace(tmp, destination)
    finally:
        tmp.unlink(missing_ok=True)


def gh(*args: str) -> str:
    completed = subprocess.run(["gh", *args], check=False, capture_output=True, text=True)
    if completed.returncode:
        raise SnapshotError(f"gh {' '.join(args[:3])} failed ({completed.returncode}): {completed.stderr[-600:]}")
    return completed.stdout


def release_info(tag: str) -> dict:
    return json.loads(gh("release", "view", tag, "--json", "body,assets"))


def parse_pointer(body: str) -> str | None:
    if not body.startswith(POINTER):
        if body.startswith("GEOSWEEP_SNAPSHOT_V1"):
            raise SnapshotError("malformed db-latest snapshot pointer")
        return None
    tag = body[len(POINTER):].splitlines()[0]
    if not TAG_PATTERN.fullmatch(tag):
        raise SnapshotError("invalid db-latest snapshot pointer")
    return tag


def assert_remote_assets(release: dict, directory: Path, filenames: list[str]) -> None:
    assets = {asset["name"]: asset for asset in release.get("assets", [])}
    if set(assets) != set(filenames):
        raise SnapshotError("release assets do not match expected snapshot files")
    for filename in filenames:
        local = directory / filename
        remote = assets[filename]
        if remote.get("state") != "uploaded" or remote.get("size") != local.stat().st_size:
            raise SnapshotError(f"incomplete release upload: {filename}")
        if remote.get("digest") != "sha256:" + sha256_file(local):
            raise SnapshotError(f"GitHub asset digest differs: {filename}")


def fetch_latest(destination: Path) -> None:
    # A missing release or network error is NEVER interpreted as an empty DB.
    latest = release_info("db-latest")
    tag = parse_pointer(latest.get("body") or "")
    with tempfile.TemporaryDirectory(prefix="geosweep-restore-") as tmp_name:
        scratch = Path(tmp_name)
        if tag:
            release = release_info(tag)
            gh("release", "download", tag, "-p", MANIFEST, "-D", str(scratch))
            manifest = parse_manifest(scratch)
            expected = [MANIFEST, *(p["name"] for p in manifest["parts"])]
            # Validate names, sizes and SHA-256 against immutable GitHub assets
            # before downloading the large payloads.
            assets = {a["name"]: a for a in release.get("assets", [])}
            if set(assets) != set(expected):
                raise SnapshotError("unexpected or missing remote snapshot assets")
            for p in manifest["parts"]:
                asset = assets[p["name"]]
                if (asset.get("state") != "uploaded" or asset.get("size") != p["size"]
                        or asset.get("digest") != "sha256:" + p["sha256"]):
                    raise SnapshotError(f"invalid remote snapshot part: {p['name']}")
            if assets[MANIFEST].get("digest") != "sha256:" + sha256_file(scratch / MANIFEST):
                raise SnapshotError("invalid remote manifest digest")
            for p in manifest["parts"]:
                gh("release", "download", tag, "-p", p["name"], "-D", str(scratch))
            restore(scratch, destination)
        else:
            legacy = {asset["name"]: asset for asset in latest.get("assets", [])}
            if "geosweep.db.zst" not in legacy:
                raise SnapshotError("db-latest contains neither pointer nor legacy archive")
            asset = legacy["geosweep.db.zst"]
            if asset.get("state") != "uploaded" or not asset.get("digest", "").startswith("sha256:"):
                raise SnapshotError("legacy asset isn't verified on GitHub")
            gh("release", "download", "db-latest", "-p", "geosweep.db.zst", "-D", str(scratch))
            if (scratch / "geosweep.db.zst").stat().st_size != asset["size"] or (
                    "sha256:" + sha256_file(scratch / "geosweep.db.zst") != asset["digest"]):
                raise SnapshotError("legacy archive download checksum mismatch")
            restore(scratch, destination, legacy=True)


def snapshots_to_prune(releases: list[dict], *, current: str,
                       previous: str | None, keep: int) -> list[str]:
    """Prune repeat runs, preserving one snapshot from each of 7 UTC dates."""
    ordered = [r["tagName"] for r in sorted(releases, key=lambda r: r["createdAt"], reverse=True)
               if TAG_PATTERN.fullmatch(r["tagName"])]
    protected = {current, previous} - {None}
    newest_each_day = {}
    for release_tag in ordered:
        day = release_tag.removeprefix("db-snapshot-")[:8]
        if day not in newest_each_day:
            newest_each_day[day] = release_tag
    to_keep = set(list(newest_each_day.values())[:keep]) | protected
    return [release_tag for release_tag in ordered if release_tag not in to_keep]


def publish(db: Path, *, run_id: str, attempt: str, keep: int = 7) -> str:
    if not run_id.isdecimal() or not attempt.isdecimal():
        raise SnapshotError("GitHub run id and attempt must be numeric")
    if keep < 2:
        raise SnapshotError("at least two dated backups must be retained")
    # Require the existing pointer before creating any new backup. No automatic
    # bootstrap or restoration from zero on an API error.
    latest = release_info("db-latest")
    previous = parse_pointer(latest.get("body") or "")
    tag = f"db-snapshot-{datetime.now(timezone.utc):%Y%m%d}-{run_id}-{attempt}"
    with tempfile.TemporaryDirectory(prefix="geosweep-pack-") as tmp_name:
        directory = Path(tmp_name)
        manifest = build_snapshot(db, directory)
        files = [MANIFEST, *(p["name"] for p in manifest["parts"])]
        print(f"Snapshot {tag}: {manifest['database_bytes']} raw bytes -> "
              f"{manifest['compressed_bytes']} compressed bytes in {len(manifest['parts'])} parts",
              flush=True)
        gh("release", "create", tag, "--draft", "--title", f"Geosweep checkpoint {tag}",
           "--notes", "Immutable verified database snapshot; referenced by db-latest.")
        gh("release", "upload", tag, *(str(directory / f) for f in files))
        assert_remote_assets(release_info(tag), directory, files)
        # Only published, verified snapshots can become the latest checkpoint.
        gh("release", "edit", tag, "--draft=false")
        pointer = f"{POINTER}{tag}\nRolling Geosweep checkpoint. Database parts are in the referenced immutable release."
        gh("release", "edit", "db-latest", "--notes", pointer)
        if parse_pointer(release_info("db-latest").get("body") or "") != tag:
            raise SnapshotError("db-latest pointer did not persist")
        print(f"Promoted {tag} to db-latest", flush=True)
    # Deletion is best-effort, and never runs until the new pointer is checked.
    try:
        snapshots = json.loads(gh("release", "list", "--limit", "200", "--json", "tagName,createdAt"))
        for obsolete in snapshots_to_prune(snapshots, current=tag, previous=previous, keep=keep):
            try:
                gh("release", "delete", obsolete, "-y", "--cleanup-tag")
            except SnapshotError as exc:
                print(f"Warning: could not prune {obsolete}: {exc}", file=sys.stderr)
    except (SnapshotError, ValueError, KeyError) as exc:
        print(f"Warning: snapshot pruning skipped: {exc}", file=sys.stderr)
    return tag


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    pack = commands.add_parser("pack")
    pack.add_argument("db", type=Path)
    pack.add_argument("directory", type=Path)
    pack.add_argument("--level", type=int, default=10)
    pack.add_argument("--chunk-bytes", type=int, default=CHUNK_BYTES)
    pack.add_argument("--threads", type=int, default=0)
    verify = commands.add_parser("verify")
    verify.add_argument("directory", type=Path)
    unpack = commands.add_parser("restore")
    unpack.add_argument("directory", type=Path)
    unpack.add_argument("db", type=Path)
    unpack.add_argument("--legacy", action="store_true")
    retrieve = commands.add_parser("fetch-latest")
    retrieve.add_argument("db", type=Path)
    push = commands.add_parser("publish")
    push.add_argument("db", type=Path)
    push.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    push.add_argument("--attempt", default=os.environ.get("GITHUB_RUN_ATTEMPT", ""))
    push.add_argument("--keep", type=int, default=7)
    args = parser.parse_args()
    try:
        if args.command == "pack":
            print(json.dumps(build_snapshot(args.db, args.directory, level=args.level,
                                            chunk_bytes=args.chunk_bytes, threads=args.threads), indent=2))
        elif args.command == "verify":
            directory = args.directory
            verify_parts(directory, parse_manifest(directory))
            print("Snapshot archive SHA-256 checks passed")
        elif args.command == "restore":
            restore(args.directory, args.db, legacy=args.legacy)
        elif args.command == "fetch-latest":
            fetch_latest(args.db)
        elif args.command == "publish":
            publish(args.db, run_id=args.run_id, attempt=args.attempt, keep=args.keep)
        return 0
    except (SnapshotError, OSError, subprocess.SubprocessError, ValueError, sqlite3.DatabaseError) as exc:
        print(f"Snapshot error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
