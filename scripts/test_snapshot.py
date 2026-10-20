"""Offline snapshot safety tests; no GitHub calls or live Geosweep database."""

import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import snapshot  # noqa: E402


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.workspace = tempfile.TemporaryDirectory()
        self.addCleanup(self.workspace.cleanup)
        self.root = Path(self.workspace.name)
        self.db = self.root / "original.db"
        conn = sqlite3.connect(self.db)
        with conn:
            conn.executescript("""
                CREATE TABLE queue (id TEXT PRIMARY KEY, status TEXT);
                CREATE TABLE caches (guid TEXT PRIMARY KEY, raw_json TEXT);
                CREATE TABLE cache_versions (guid TEXT, raw_json TEXT);
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            """)
            for n in range(700):
                conn.execute("INSERT INTO caches VALUES (?, ?)",
                             (f"GC{n:05d}", json.dumps({"name": "repeat" * 40,
                                                          "guid": n, "notes": "a" * 100})))
        conn.close()
        self.assertGreater(self.db.stat().st_size, 100_000)
        self.parts = self.root / "parts"

    def build(self):
        return snapshot.build_snapshot(self.db, self.parts, level=3, chunk_bytes=4096,
                                       threads=1)

    def test_round_trip_parts_and_checksums(self):
        expected = hashlib.sha256(self.db.read_bytes()).hexdigest()
        manifest = self.build()
        self.assertGreater(len(manifest["parts"]), 1)
        self.assertEqual(snapshot.parse_manifest(self.parts), manifest)
        snapshot.verify_parts(self.parts, manifest)
        output = self.root / "restored.db"
        snapshot.restore(self.parts, output)
        self.assertEqual(hashlib.sha256(output.read_bytes()).hexdigest(), expected)

    def test_corrupt_part_is_rejected_without_overwriting_destination(self):
        manifest = self.build()
        dest = self.root / "dest.db"
        dest.write_bytes(b"existing database stays untouched")
        path = self.parts / manifest["parts"][0]["name"]
        with path.open("r+b") as f:
            f.seek(0)
            f.write(b"BROKEN!")
        with self.assertRaisesRegex(snapshot.SnapshotError, "hash mismatch"):
            snapshot.restore(self.parts, dest)
        self.assertEqual(dest.read_bytes(), b"existing database stays untouched")

    def test_missing_part_and_modified_manifest_rejected(self):
        manifest = self.build()
        (self.parts / manifest["parts"][-1]["name"]).unlink()
        with self.assertRaisesRegex(snapshot.SnapshotError, "missing or incorrect part"):
            snapshot.restore(self.parts, self.root / "output.db")
        manifest["parts"][0]["name"] = "../../malicious-path"
        (self.parts / snapshot.MANIFEST).write_text(json.dumps(manifest))
        with self.assertRaisesRegex(snapshot.SnapshotError, "unexpected part name"):
            snapshot.restore(self.parts, self.root / "output.db")

    def test_legacy_single_file_restore(self):
        archive = self.root / "legacy"
        archive.mkdir()
        import subprocess
        with (archive / "geosweep.db.zst").open("wb") as out:
            subprocess.run(["zstd", "-q", "-3", "-c", str(self.db)], check=True, stdout=out)
        dest = self.root / "restored.db"
        snapshot.restore(archive, dest, legacy=True)
        self.assertEqual(dest.read_bytes(), self.db.read_bytes())

    def test_pointer_parsing_and_validation(self):
        tag = "db-snapshot-20261019-123-1"
        self.assertEqual(snapshot.parse_pointer(f"{snapshot.POINTER}{tag}\nnotes"), tag)
        self.assertIsNone(snapshot.parse_pointer("Old rolling snapshot"))
        for value in ("../../x", "db-snapshot-20261019-abc-1", "db-latest"):
            with self.subTest(value=value):
                with self.assertRaises(snapshot.SnapshotError):
                    snapshot.parse_pointer(snapshot.POINTER + value)

    def test_failed_latest_lookup_never_starts_blank(self):
        dest = self.root / "existing.db"
        dest.write_bytes(b"keep this")
        with patch.object(snapshot, "release_info", side_effect=snapshot.SnapshotError("503")):
            with self.assertRaisesRegex(snapshot.SnapshotError, "503"):
                snapshot.fetch_latest(dest)
        self.assertEqual(dest.read_bytes(), b"keep this")

    def test_promotion_only_after_verified_upload(self):
        commands = []
        pointer = []
        manifest = {"assets": []}
        tag = f"db-snapshot-{snapshot.datetime.now(snapshot.timezone.utc):%Y%m%d}-54321-1"
        def fake_gh(*args):
            commands.append(args)
            if args[:2] == ("release", "edit") and args[2] == "db-latest":
                pointer.append(args)
            if args[:2] == ("release", "list"):
                return "[]"
            return ""
        def fake_info(which):
            if which == "db-latest":
                body = f"{snapshot.POINTER}{tag}" if pointer else "Old legacy snapshot"
                return {"body": body, "assets": []}
            return manifest
        with patch.object(snapshot, "gh", side_effect=fake_gh), patch.object(
            snapshot, "release_info", side_effect=fake_info
        ), patch.object(snapshot, "assert_remote_assets", side_effect=snapshot.SnapshotError("bad hash")):
            with self.assertRaisesRegex(snapshot.SnapshotError, "bad hash"):
                snapshot.publish(self.db, run_id="54321", attempt="1")
        self.assertFalse(pointer)
        self.assertTrue(any(cmd[:3] == ("release", "upload", tag) for cmd in commands))

    def test_publish_success_promotes_after_asset_digest_checks(self):
        calls = []
        state = {"body": "Old legacy snapshot", "assets": [], "draft": False}

        def fake_gh(*args):
            calls.append(args)
            if args[:3] == ("release", "view", "db-latest"):
                return json.dumps({"body": state["body"], "assets": []})
            if args[:2] == ("release", "view"):
                return json.dumps({"body": "", "assets": state["assets"]})
            if args[:2] == ("release", "create"):
                state["draft"] = True
            if args[:2] == ("release", "upload"):
                state["assets"] = [
                    {"name": Path(f).name, "size": Path(f).stat().st_size,
                     "state": "uploaded", "digest": "sha256:" + snapshot.sha256_file(Path(f))}
                    for f in args[3:]
                ]
            if args[:3] == ("release", "edit", "db-latest"):
                self.assertFalse(state["draft"], "pointer promoted before snapshot was published")
                state["body"] = args[args.index("--notes") + 1]
            elif args[:2] == ("release", "edit") and "--draft=false" in args:
                state["draft"] = False
            if args[:2] == ("release", "list"):
                return "[]"
            return ""

        with patch.object(snapshot, "gh", side_effect=fake_gh):
            tag = snapshot.publish(self.db, run_id="98765", attempt="2")
        self.assertEqual(snapshot.parse_pointer(state["body"]), tag)
        self.assertFalse(state["draft"])
        ops = [(call[1], call[2]) for call in calls if call[:1] == ("release",)
               and call[1] in ("create", "upload", "edit")]
        self.assertLess(ops.index(("upload", tag)), ops.index(("edit", "db-latest")))

    def test_fetch_latest_multipart_round_trip(self):
        manifest = self.build()
        tag = "db-snapshot-20261019-4444-1"
        available = [self.parts / snapshot.MANIFEST] + [
            self.parts / p["name"] for p in manifest["parts"]]
        assets = [{"name": p.name, "size": p.stat().st_size,
                   "state": "uploaded", "digest": "sha256:" + snapshot.sha256_file(p)}
                  for p in available]

        def fake_gh(*args):
            if args[:3] == ("release", "view", "db-latest"):
                return json.dumps({"body": snapshot.POINTER + tag, "assets": []})
            if args[:3] == ("release", "view", tag):
                return json.dumps({"body": "", "assets": assets})
            if args[:3] == ("release", "download", tag):
                import shutil
                file = args[args.index("-p") + 1]
                directory = Path(args[args.index("-D") + 1])
                shutil.copyfile(self.parts / file, directory / file)
                return ""
            raise AssertionError(f"unexpected gh call {args}")

        destination = self.root / "fetch-result.db"
        with patch.object(snapshot, "gh", side_effect=fake_gh):
            snapshot.fetch_latest(destination)
        self.assertEqual(self.db.read_bytes(), destination.read_bytes())

    def test_remote_digest_mismatch_rejected(self):
        manifest = self.build()
        expected = [snapshot.MANIFEST, *[p["name"] for p in manifest["parts"]]]
        assets = [{"name": name, "size": (self.parts / name).stat().st_size,
                   "state": "uploaded", "digest": "sha256:" + "0" * 64} for name in expected]
        with self.assertRaisesRegex(snapshot.SnapshotError, "digest differs"):
            snapshot.assert_remote_assets({"assets": assets}, self.parts, expected)


if __name__ == "__main__":
    unittest.main()
