# geosweep

Traditional geocache **location** mirror: worldwide discovery crawl of
geocaching.com + daily refresh. Locations only (code, coords, type, D/T)
— no descriptions, no logs.

## Source

Website search API (`/api/proxy/web/search/v2`), box queries with
take/skip pagination — the same endpoint family c:geo's live map uses.
Box queries tile exactly, so unlike circular searches there are no
geometric coverage gaps. Window rule: `skip + take` past ~10000 errors,
so cells above ~9500 split instead of paginating.

Basic accounts see premium-only caches **without coordinates** (teaser
rows, ready to backfill). A premium account fills them in with zero
code changes.

## Never-delete

Field changes append to `cache_versions`. Discovery inserts dedup by GC
code, so re-walking is idempotent. (Removal detection is future work:
absence from an overlapping re-walked box is not conclusive proof.)

## Run

```bash
export GC_USER=... GC_PASS=...
cargo run --release -- --db data/geosweep.db crawl
cargo run --release -- --db data/geosweep.db refresh   # daily
cargo run --release -- --db data/geosweep.db stats
cargo run --release -- --db data/geosweep.db verify
```

Automation: a GitHub Action with automatic checkpoint-to-checkpoint
continuation, DB in a rolling `db-latest` release (+ 7 dated backups).

## Web viewer

The `web/` static viewer adapts LabSweep's sidebar and MapLibre map for
geocaches: world density, clusters/heat/points, nearby browsing, search by
GC code/name/owner, difficulty/terrain filters, basic cache metadata, and
Geocaching.com links. LabSweep-specific stages, reviews, ratings, and
adventure history are intentionally removed.

Generate data locally with:

```sh
python3 scripts/export_web.py --db data/geosweep.db --out web/data
python3 -m http.server --directory web 8000
```

The Pages workflow exports from the latest rolling database once daily
or on demand. Individual cache locations are loaded as geographic shards
when zooming in. Worldwide text search loads a compressed catalog on
demand. Premium caches lacking coordinates are searchable by unchecking
the coordinates filter.
