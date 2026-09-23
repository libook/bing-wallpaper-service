# CONTEXT

## Domain

This service is a thin proxy over Microsoft's Bing homepage wallpaper feed.

### Wallpaper

The image served by this service. Sourced from the Bing homepage wallpaper API.

### Bing API

The upstream feed at `https://www.bing.com/hp/api/model`. Returns a JSON document whose `MediaContents` array holds one entry per wallpaper.

### index_past

Query parameter. `0` means the newest wallpaper, `1` the previous one, and so on. Indexes into the `MediaContents` array.

### get_image

Query parameter. When `true`, the service responds with the image bytes directly; when `false`, with the image URL.

### Wallpaper URL

The resolved, fully-qualified URL of a wallpaper. The Bing API may return a path without an origin; the service prepends `https://www.bing.com` when the path does not already contain `http`.

### Wallpaper source

The module that resolves a wallpaper URL from the Bing API. Implemented as `BingWallpaperSource` with interface `wallpaper_url(index_past) -> Result<String, SourceError>`. Its implementation owns the Bing fetch, typed `BingResponse` deserialization, index bounds check, and origin resolution. The HTTP client is injected through the `Http` seam (`ReqwestHttp` in production, `MockHttp` in tests) rather than created inside.

The interface is honest about every way the module can fail: `Parse` (upstream contract changed — our problem), `Network` (Bing unreachable), `ImageFetch` (image host unreachable), `NotFound` (index out of range). The handler adapter translates these to HTTP responses with precise status codes (`400` bad query, `404` missing, `500` contract change, `502` upstream unavailable) rather than panicking.

## Out of scope

- `www.bing.com` belongs to Microsoft. Use of this project for purposes that violate local laws is prohibited.