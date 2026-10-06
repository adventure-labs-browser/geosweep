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

Automation: private repo + daily GitHub Action, DB in a rolling
`db-latest` release (+ 7 dated backups).
