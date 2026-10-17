#!/usr/bin/env python3
"""Export GeoSweep SQLite to lazy 5-degree map tiles and a worldwide search index."""
import argparse
from collections import Counter, OrderedDict
from datetime import datetime, timezone
import gzip
import json
import math
import struct
from pathlib import Path
import sqlite3
import tempfile

FIELDS = ("guid", "name", "latitude", "longitude", "cache_type",
          "difficulty", "terrain", "size", "premium", "favorite_points",
          "owner", "region", "country", "placed_utc")
def dump(row):
    return json.dumps(row, ensure_ascii=False, separators=(",", ":"))

def export(db_path: Path, output: Path):
    output.mkdir(parents=True, exist_ok=True)
    tiles_dir = output / "tiles"
    tiles_dir.mkdir(exist_ok=True)
    db = sqlite3.connect(f"file:{db_path.resolve()}?mode=ro", uri=True)
    keys = dict(db.execute("SELECT key, value FROM meta"))
    worldwide = int(keys.get("worldwide_search_total", 0))
    queue = dict(db.execute("SELECT status, COUNT(*) FROM queue GROUP BY status"))
    density = Counter()
    totals = Counter()
    lru = OrderedDict()

    with tempfile.TemporaryDirectory(prefix="geosweep-tiles-") as temp, gzip.open(
        output / "catalog.json.gz", "wt", encoding="utf-8", compresslevel=5
    ) as catalog, gzip.open(output / "points.bin.gz", "wb", compresslevel=5) as points, \
         gzip.open(output / "points-preview.bin.gz", "wb", compresslevel=5) as preview:
        temp_path = Path(temp)
        catalog.write("[")
        first = True
        sql = "SELECT " + ", ".join(FIELDS) + " FROM caches"
        for row in db.execute(sql):
            record = list(row)
            record[8] = int(record[8] or 0)
            totals["caches"] += 1
            totals["premium"] += bool(record[8])
            if not first:
                catalog.write(",")
            catalog.write(dump(record))
            first = False
            lat, lon = record[2:4]
            if lat is None or lon is None or not -90 <= lat <= 90 or not -180 <= lon <= 180:
                totals["coordless"] += 1
                continue
            totals["located"] += 1
            # Direct-to-GPU binary stream: mercator x/y, type, difficulty/terrain x2.
            phi = math.radians(max(-85.051129, min(85.051129, lat)))
            mx = (lon + 180) / 360
            my = (1 - math.log(math.tan(math.pi / 4 + phi / 2)) / math.pi) / 2
            packed = struct.pack("<ffHBB", mx, my,
                int(record[4] or 0) & 0xffff,
                max(0, min(255, round((record[5] or 0) * 2))),
                max(0, min(255, round((record[6] or 0) * 2))))
            points.write(packed)
            # Draw real individual caches immediately at world zoom while
            # the complete coordinate-only binary loads in the background.
            if totals["located"] % 12 == 1:
                preview.write(packed)
            y = min(35, max(0, int((lat + 90) // 5)))
            x = min(71, max(0, int((lon + 180) // 5)))
            tile = f"{y}-{x}"
            density[tile] += 1
            if tile not in lru:
                if len(lru) >= 64:
                    _, handle = lru.popitem(last=False)
                    handle.close()
                lru[tile] = (temp_path / f"{tile}.jsonl").open("a", encoding="utf-8")
            lru.move_to_end(tile)
            lru[tile].write(dump(record) + "\n")
        catalog.write("]")
        for handle in lru.values():
            handle.close()

        for source in sorted(temp_path.glob("*.jsonl")):
            with source.open(encoding="utf-8") as src, gzip.open(
                tiles_dir / f"{source.stem}.json.gz", "wt", encoding="utf-8", compresslevel=5
            ) as dst:
                dst.write("[")
                for n, line in enumerate(src):
                    if n:
                        dst.write(",")
                    dst.write(line.rstrip("\n"))
                dst.write("]")

    index = {
        "generated": datetime.now(timezone.utc).isoformat(),
        "total": totals["caches"],
        "located": totals["located"],
        "coordless": totals["coordless"],
        "premium": totals["premium"],
        "worldwide": worldwide,
        "pending": queue.get("pending", 0),
        "failed": queue.get("failed", 0),
        "tiles": [[int(y), int(x), count] for tile, count in sorted(density.items())
                  for y, x in [tile.split("-")]]
    }
    (output / "index.json").write_text(dump(index), encoding="utf-8")
    db.close()
    print(f"Exported {totals['caches']:,} caches in {len(density)} map tiles,"
          f" {totals['located']:,} with coordinates")

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=Path("web/data"))
    args = parser.parse_args()
    export(args.db, args.out)
