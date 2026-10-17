import struct
import gzip
from pathlib import Path
import sqlite3
import tempfile
import unittest

from export_web import export, FIELDS

class ExportWebTests(unittest.TestCase):
    def test_mapped_and_coordless_rows(self):
        with tempfile.TemporaryDirectory() as td:
            base = Path(td)
            db = base / "source.db"
            conn = sqlite3.connect(db)
            numeric = {"latitude", "longitude", "difficulty", "terrain",
                       "cache_type", "size", "premium", "favorite_points"}
            conn.execute("CREATE TABLE caches (" +
                         ", ".join(name + (" REAL" if name in numeric else " TEXT")
                                   for name in FIELDS) + ")")
            conn.execute("CREATE TABLE meta (key TEXT, value TEXT)")
            conn.execute("CREATE TABLE queue (status TEXT)")
            conn.execute("INSERT INTO meta VALUES ('worldwide_search_total','4')")
            conn.execute("INSERT INTO queue VALUES ('pending')")
            conn.execute("INSERT INTO queue VALUES ('done')")
            values = [
                ("GC1", "Near <site>", -90, -180, 2, 1.5, 2, 1, 0, 10,
                 "Owner", "Region", "Country", "2020-01-01"),
                ("GC2", "Premium", None, None, 8, 3, 4, 2, 1, 0,
                 "Owner", "Region", "Country", "2022-02-02"),
                ("GC3", "At dateline", 90, 180, 3, 2, 3, 3, 0, 1,
                 "Other", "Region", "Country", None),
            ]
            for v in values:
                conn.execute("INSERT INTO caches VALUES (" + ",".join("?" for _ in FIELDS) + ")",v)
            conn.commit()
            conn.close()
            out = base / "export"
            export(db, out)
            import json
            info = json.loads((out / "index.json").read_text())
            self.assertEqual(info["total"], 3)
            self.assertEqual(info["located"], 2)
            self.assertEqual(info["coordless"], 1)
            self.assertEqual(info["premium"], 1)
            self.assertEqual(info["worldwide"], 4)
            self.assertEqual(info["pending"], 1)
            with gzip.open(out / "points.bin.gz", "rb") as f:
                packed = f.read()
            with gzip.open(out / "points-preview.bin.gz", "rb") as f:
                sample = f.read()
            self.assertEqual(len(packed), 2 * 12)
            self.assertEqual(len(sample), 12)
            west = struct.unpack_from("<ffHBB", packed, 0)
            east = struct.unpack_from("<ffHBB", packed, 12)
            self.assertAlmostEqual(west[0], 0)
            self.assertAlmostEqual(east[0], 1)
            self.assertEqual(west[2:], (2, 3, 4))
            self.assertEqual(east[2:], (3, 4, 6))
            self.assertEqual(sorted(info["tiles"]), [[0,0,1],[35,71,1]])
            with gzip.open(out / "catalog.json.gz", "rt", encoding="utf-8") as f:
                catalog = json.load(f)
            self.assertEqual(len(catalog), 3)
            self.assertEqual(catalog[0][1], "Near <site>")
            self.assertEqual(catalog[1][2], None)
            with gzip.open(out / "tiles/0-0.json.gz", "rt", encoding="utf-8") as f:
                self.assertEqual(json.load(f)[0][0], "GC1")
            with gzip.open(out / "tiles/35-71.json.gz", "rt", encoding="utf-8") as f:
                self.assertEqual(json.load(f)[0][0], "GC3")

if __name__ == "__main__":
    unittest.main()
