# In-memory image cache keyed by URL, with a sliding 24h TTL

When a client requests `get_image=true`, the service responds with the image
bytes instead of a URL. Without a cache, every such request re-downloads the
blob from Bing's image host. The service now keeps downloaded image bytes in a
shared in-memory cache so repeated requests for the same wallpaper do not
re-hit the upstream.

## The cache, in one paragraph

`ImageCache` is an `Arc`-shared `Mutex` over two tables: `entries` (URL →
`{bytes, expires_at}`) and `in_flight` (URL → per-URL `tokio::sync::Mutex`).
The outer lock is never held across an await. On a request the cache first checks
for a fresh entry (a hit slides the 24h window from that moment), then reserves
a per-URL slot. The first caller to reach a cold URL runs the download while
holding the slot; concurrent callers for the same URL wait on that slot and
receive the same result. On success the blob is published to `entries`; on
failure nothing is published, so the next request retries.

## Considered options

### Cache key: the resolved URL, not the client's request parameters

- **Key by `index_past` (and `get_image`).** Rejected. `index_past` is a
  *relative* position: Bing advances the feed by one image every day, so the
  picture at `index_past=0` tomorrow is a different URL than today. Keying on it
  would leave a stale entry behind each day and would conflate "the picture at
  position 0" with "the picture at position 0 on a different day".
- **Key by the resolved image URL.** Accepted. A URL is the stable identity of
  a wallpaper — it does not shift when the feed rotates, and the same URL can
  be reached through different `index_past` values over time.

### Scope: image bytes only, or also the Bing API response?

- **Cache the Bing API response too** (`MediaContents`, the `index_past` → URL
  resolution). Rejected. The whole point of the cache is to reduce upstream
  load, but Bing publishes one new homepage image per day. Caching the API
  response for 24h would mean `index_past=0` resolves to *yesterday's* image for
  up to a day — a client asking for the newest wallpaper could not get today's
  until the cache expired. The API response is a few KB and changes daily, so
  re-fetching it on every request is cheap and keeps the newest image available
  immediately.
- **Cache image bytes only.** Accepted. The API stays uncached; the image cache
  is keyed by URL, so when Bing rotates the feed the new image has a new URL
  and is a natural cache miss.

### TTL: sliding window, or fixed 24h from first fetch?

- **Fixed 24h from first fetch.** Rejected. A popular wallpaper would expire in
  the middle of a busy period and re-download while still being requested.
- **Sliding 24h, reseeded on every hit.** Accepted. A picture that keeps getting
  requested never expires; a picture nobody asks for expires 24h after its last
  request and frees its bytes.

### Concurrency: per-URL single-flight, or a plain `Mutex<HashMap>`?

- **Plain `Mutex<HashMap>`, allow duplicate fetches.** Rejected for this service
  — the earlier design round established that concurrent clients should
  coalesce, and the cost of a second fetch is not just a duplicate download but
  two simultaneous connections to Bing's image host.
- **Per-URL `tokio::sync::Mutex` slot.** Accepted. The first caller holds the
  slot across the download; waiters block on the same slot and get the stored
  result. Different URLs still fetch concurrently. The slot doubles as the
  "in flight" marker in the outer table, so the outer lock is only touched to
  look up or publish.

### Failures: cache them, or not?

- **Cache failures too.** Rejected. If the image host is down, caching the
  error would serve `502` to every client for up to 24h even after the host
  recovers — indistinguishable from "this service is down".
- **Do not cache failures.** Accepted. A failed download publishes nothing; the
  next request retries. The single-flight slot still lets concurrent callers
  share the same failure, so a momentary outage is not amplified into many
  upstream requests.

## Consequences

- `get_image=false` requests never touch the cache and never download the image
  — the URL path is unchanged in behaviour, only the `get_image=true` path is
  cached.
- The cache is shared across all connection tasks via `Arc`, so it is a
  genuine multi-client cache rather than a per-connection one.
- `SourceError` gained `Clone` (it was `Debug` only) so the single-flight slot
  can hand the same result to multiple waiters.
- The cache is unbounded. Bing's feed is a handful of images under 2 MB each, so
  total memory is bounded in practice; the structure keeps the two tables
  separate so an LRU cap can be added later without touching the fetch path.
- `fetch_image` stays as the raw download primitive and is reused by
  `cached_image`, so the cache stores exactly what the source produces
  (`Result<Bytes, SourceError>`) with no translation layer.