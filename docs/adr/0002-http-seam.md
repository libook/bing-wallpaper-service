# HTTP client injected through an `Http` seam

The wallpaper source does not create its HTTP client. `BingWallpaperSource`
holds an `Arc<dyn Http>` and two adapters satisfy the interface: `ReqwestHttp` in
production, `MockHttp` in tests.

One adapter would be a hypothetical seam; two adapters make it a real one. The
seam is what lets the source's behaviour be tested without network access —
`wallpaper_url` is exercised through the same interface production code crosses,
so the tests and production share one surface.

## Considered options

- **Call `reqwest::get` directly from the source.** Rejected: the source then
  creates its own dependency, and every test must hit the network. Nothing is
  testable past the handler.
- **Generic over `H: Http` instead of `Arc<dyn Http>`.** Rejected: the type
  parameter would leak into the handler's signature and `main`, exposing the
  concrete adapter type in the module's interface. Erasing to a trait object
  keeps the interface small.

## Consequences

- `Http` is a boxed-future trait (`fn get(&self, url) -> Pin<Box<dyn Future + Send>>`)
  rather than `async_trait`, so the seam needs no new crate.
- `MockHttp` is `#[cfg(test)]`; it does not exist in a production build.
- The source's error type stays focused on the domain (`Parse`, `Network`,
  `ImageFetch`, `NotFound`) instead of carrying HTTP-layer detail.