use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::tokio::TokioIo;
use reqwest;
use serde_derive::{Deserialize, Serialize};
use serde_qs as qs;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;

use bytes::Bytes;

static BING_DOMAIN: &str = "https://www.bing.com";
static BING_API_PATH: &str = "/hp/api/model";
static LISTEN_ADDRESS: &str = "0.0.0.0:3000";

#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
struct RequestQueryParams {
    index_past: usize,
    get_image: bool,
}

impl Default for RequestQueryParams {
    fn default() -> Self {
        RequestQueryParams {
            index_past: 0,
            get_image: false,
        }
    }
}

// --- Wallpaper source module (deepened in candidate 1, seam added in candidate 2) ---

/// Error modes produced by the wallpaper source itself.
///
/// The interface is honest about every way this module can fail: a malformed
/// upstream contract, the upstream being unreachable, an image host being
/// unreachable, or the requested index not existing. The handler adapter
/// translates these to HTTP responses; the module does not know HTTP.
#[derive(Debug, Clone)]
enum SourceError {
    Parse(String),
    Network(String),
    ImageFetch(String),
    NotFound(usize),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Parse(msg) => write!(f, "failed to parse Bing API response: {}", msg),
            SourceError::Network(msg) => write!(f, "Bing API request failed: {}", msg),
            SourceError::ImageFetch(msg) => write!(f, "image fetch failed: {}", msg),
            SourceError::NotFound(i) => write!(f, "no wallpaper at index_past {}", i),
        }
    }
}

impl std::error::Error for SourceError {}

/// The seam for HTTP access.
///
/// Two adapters make this a real seam: `ReqwestHttp` in production and
/// `MockHttp` in tests. Callers cross the same interface either way.
trait Http: Send + Sync {
    fn get(&self, url: &str) -> Pin<Box<dyn Future<Output = Result<Bytes, HttpError>> + Send>>;
}

/// Error mode of the HTTP layer, distinct from the source's own errors so the
/// module can translate rather than leak it across the seam.
#[derive(Debug, Clone)]
enum HttpError {
    Network(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Network(msg) => write!(f, "HTTP request failed: {}", msg),
        }
    }
}

impl std::error::Error for HttpError {}

/// Production adapter: reqwest.
struct ReqwestHttp;

impl Http for ReqwestHttp {
    fn get(&self, url: &str) -> Pin<Box<dyn Future<Output = Result<Bytes, HttpError>> + Send>> {
        let url = url.to_string();
        Box::pin(async move {
            reqwest::get(&url)
                .await
                .map_err(|e| HttpError::Network(e.to_string()))?
                .bytes()
                .await
                .map_err(|e| HttpError::Network(e.to_string()))
        })
    }
}

/// Test adapter: an in-memory table of URL → bytes.
#[cfg(test)]
#[derive(Default)]
struct MockHttp {
    responses: HashMap<String, Bytes>,
}

#[cfg(test)]
impl MockHttp {
    fn new() -> Self {
        Self::default()
    }

    fn with(mut self, url: &str, bytes: Bytes) -> Self {
        self.responses.insert(url.to_string(), bytes);
        self
    }
}

#[cfg(test)]
impl Http for MockHttp {
    fn get(&self, url: &str) -> Pin<Box<dyn Future<Output = Result<Bytes, HttpError>> + Send>> {
        let url = url.to_string();
        let bytes = self.responses.get(&url).cloned();
        Box::pin(async move {
            bytes.ok_or_else(|| HttpError::Network(format!("no mock response for {}", url)))
        })
    }
}

/// Typed view of the Bing API response.
///
/// The fragile `res["MediaContents"][i]["ImageContent"]["Image"]["Url"]`
/// navigation lives here, inside the module, as field access on a struct.
///
/// Field names mirror the upstream PascalCase JSON contract on purpose: the
/// struct reads as the API, so a contract change is a one-field edit.
#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct BingResponse {
    MediaContents: Vec<MediaContent>,
}

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct MediaContent {
    ImageContent: ImageContent,
}

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct ImageContent {
    Image: Image,
}

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct Image {
    Url: String,
}

/// The wallpaper source module.
///
/// Interface: `wallpaper_url`. Implementation: Bing fetch, typed
/// deserialization, index bounds check, origin resolution. The HTTP client is
/// injected through the `Http` seam rather than created inside.
struct BingWallpaperSource {
    http: Arc<dyn Http>,
    image_cache: Arc<ImageCache>,
}

impl BingWallpaperSource {
    fn new(http: Arc<dyn Http>, image_cache: Arc<ImageCache>) -> Self {
        Self { http, image_cache }
    }

    async fn request_bing(&self) -> Result<BingResponse, SourceError> {
        let url = format!("{}{}", BING_DOMAIN, BING_API_PATH);
        let bytes = self
            .http
            .get(&url)
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;

        serde_json::from_slice(&bytes).map_err(|e| SourceError::Parse(e.to_string()))
    }

    async fn fetch_image(&self, url: &str) -> Result<Bytes, SourceError> {
        self.http
            .get(url)
            .await
            .map_err(|e| SourceError::ImageFetch(e.to_string()))
    }

    /// Download the image at `url`, or serve it from the in-memory cache when
    /// its 24h window is still fresh. The cache is keyed by URL, so it covers
    /// historical wallpapers too and is independent of how the client asked
    /// for it.
    async fn cached_image(&self, url: &str) -> Result<Bytes, SourceError> {
        let url = url.to_string();
        self.image_cache
            .get(&url.clone(), move || {
                Box::pin(async move { self.fetch_image(&url).await })
            })
            .await
    }

    async fn wallpaper_url(&self, index_past: usize) -> Result<String, SourceError> {
        let res: BingResponse = self.request_bing().await?;

        // Bounds check replaces the silent out-of-range panic of raw indexing.
        let content = res
            .MediaContents
            .get(index_past)
            .ok_or_else(|| SourceError::NotFound(index_past))?;

        let path = &content.ImageContent.Image.Url;

        // If no origin in path, use BING_DOMAIN.
        if !path.contains("http") {
            Ok(format!("{}{}", BING_DOMAIN, path))
        } else {
            Ok(path.clone())
        }
    }
}

// --- In-memory image cache ---

/// How long a cached image blob counts as fresh before it must be
/// re-downloaded from Bing's image host.
const IMAGE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A downloaded image blob and the moment it stops being fresh.
struct CachedImage {
    bytes: Bytes,
    expires_at: Instant,
}

/// The cached state guarded by a plain `Mutex`. The outer lock is never held
/// across an await: it only glances at the tables and reserves a per-URL slot.
struct ImageCacheInner {
    entries: HashMap<String, CachedImage>,
    in_flight: HashMap<String, Arc<AsyncMutex<Option<CacheResult>>>>,
}

/// Thread-safe in-memory cache of downloaded image bytes, keyed by the
/// resolved image URL.
///
/// The key is the URL, never the client's request parameters. `index_past` is a
/// relative position that Bing advances by one every day, so keying on it would
/// leave a stale entry behind each time the feed rotates; a URL is the stable
/// identity of a wallpaper.
///
/// The TTL is a sliding 24h window: a hit reseeds the timer from the moment of
/// the hit, so a picture that keeps getting requested never expires. Failures
/// are deliberately not cached, so an expired or failed picture is downloaded
/// again on the next request instead of being served a stale error.
struct ImageCache {
    inner: Mutex<ImageCacheInner>,
}

/// The result of downloading one image: exactly what
/// [`BingWallpaperSource::fetch_image`] produces, so the cache can store and
/// hand it out to coalesced callers unchanged.
type CacheResult = Result<Bytes, SourceError>;

impl ImageCache {
    fn new() -> Self {
        Self {
            inner: Mutex::new(ImageCacheInner {
                entries: HashMap::new(),
                in_flight: HashMap::new(),
            }),
        }
    }

    /// Return the bytes for `url`, downloading them through `fetch` on a miss.
    ///
    /// Concurrent callers for the same URL coalesce onto a single upstream
    /// fetch: the first caller to reach the cache runs `fetch` while the rest
    /// wait on the same per-URL slot and receive its result.
    #[allow(clippy::needless_lifetimes)]
    async fn get<'a, F>(&'a self, url: &str, fetch: F) -> CacheResult
    where
        // The fetch future is tied to the cache borrow: it may only be polled
        // while the caller still holds the cache, so it cannot outlive it.
        F: FnOnce() -> Pin<Box<dyn Future<Output = CacheResult> + Send + 'a>>,
    {
        // 1. Reserve a per-URL slot, after checking for a fresh cached blob.
        //    Everything here is under the outer lock, which is dropped before
        //    any await, so this is a short critical section.
        let slot: Arc<AsyncMutex<Option<CacheResult>>> = {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();

            let expired = if let Some(entry) = inner.entries.get_mut(url) {
                if entry.expires_at > now {
                    // Cache hit: slide the window from this moment.
                    entry.expires_at = now + IMAGE_CACHE_TTL;
                    return Ok(entry.bytes.clone());
                }
                true
            } else {
                false
            };
            // The mutable borrow of `entry` ends here, so the table can be
            // mutated again to drop the stale blob.
            if expired {
                inner.entries.remove(url);
            }

            if let Some(slot) = inner.in_flight.get(url) {
                Arc::clone(slot)
            } else {
                let slot = Arc::new(AsyncMutex::new(None));
                inner.in_flight.insert(url.to_string(), Arc::clone(&slot));
                slot
            }
        };

        // 2. Serialize on the per-URL slot. The first task sees `None` and
        //    fetches; everyone else sees the stored result.
        let mut guard = slot.lock().await;
        if let Some(result) = &*guard {
            return result.clone();
        }

        // 3. We are the fetcher. Run the download while holding the slot, so no
        //    other caller can start a second fetch for the same URL.
        let result = fetch().await;
        *guard = Some(result.clone());
        drop(guard);

        // 4. Publish: remember the blob on success and drop the in-flight
        //    marker. Failures are not cached, so the next request retries.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.in_flight.remove(url);
            if let Ok(bytes) = &result {
                inner.entries.insert(
                    url.to_string(),
                    CachedImage {
                        bytes: bytes.clone(),
                        expires_at: Instant::now() + IMAGE_CACHE_TTL,
                    },
                );
            }
        }

        result
    }
}

/// Translate a source failure into an HTTP response.
///
/// The status code reflects what this service can and cannot do, not a blanket
/// server failure: `400` for a bad client request, `404` for a missing index,
/// `500` for a contract change under us, and `502` when the upstream is the one
/// that is unavailable. `502` is not "this service is down"; it is the standard
/// gateway code for "this service is up and its upstream failed".
fn error_response(status: u16, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Headers", "*")
        .header("Access-Control-Allow-Method", "*")
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::copy_from_slice(message.as_bytes())))
        .unwrap()
}

async fn handle(
    req: Request<impl hyper::body::Body>,
    source: &BingWallpaperSource,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Processing request arguments

    let received_querystring: &str = req.uri().query().unwrap_or("");
    let received_query: RequestQueryParams = match qs::from_str(received_querystring) {
        Ok(q) => q,
        // Bad request: the client sent parameters this service cannot interpret.
        Err(e) => return Ok(error_response(400, &e.to_string())),
    };

    // Get image URL

    let url = match source.wallpaper_url(received_query.index_past).await {
        Ok(url) => url,
        Err(err) => {
            // The status reflects what failed and who owns it.
            let status = match err {
                SourceError::NotFound(_) => 404,
                SourceError::Parse(_) => 500,
                SourceError::Network(_) => 502,
                SourceError::ImageFetch(_) => 502,
            };
            return Ok(error_response(status, &err.to_string()));
        }
    };
    println!("Got image path: {}", url);

    let response_builder = Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Headers", "*")
        .header("Access-Control-Allow-Method", "*");
    let response;

    if received_query.get_image {
        // Get image data. The in-memory cache keeps the last 24h of blobs per
        // image URL (keyed by URL, not by the client's query), so historical
        // wallpapers are cached too and repeated viewers do not re-download
        // from Bing. The Bing API itself is intentionally not cached, so a
        // request for index_past=0 still resolves to today's new wallpaper.

        let image_bytes = match source.cached_image(&url).await {
            Ok(bytes) => bytes,
            // Upstream image host unavailable: the service is up, the host is not.
            Err(err) => return Ok(error_response(502, &err.to_string())),
        };
        response = response_builder
            .header("Content-Type", "image/webp")
            .body(Full::new(image_bytes))
            .unwrap();
    } else {
        response = response_builder.body(Full::new(Bytes::from(url))).unwrap();
    }
    Ok(response)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = LISTEN_ADDRESS.parse().unwrap();

    // Bind prot and listen
    let listener = TcpListener::bind(addr).await?;
    println!("Listening on http://{}", addr);

    // In-memory image cache shared by every connection task, so concurrent
    // clients coalesce onto one download per image URL.
    let image_cache = Arc::new(ImageCache::new());

    loop {
        // Accept TCP connection
        let (tcp, _) = listener.accept().await?;

        // Convert TcpStream to type hyper need
        let io = TokioIo::new(tcp);

        // Process the connection in a new task
        let image_cache = Arc::clone(&image_cache);
        tokio::task::spawn(async move {
            let source = BingWallpaperSource::new(Arc::new(ReqwestHttp), image_cache);

            // Use HTTP/1 process connection and bring request to handle function
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|req| handle(req, &source)))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{handle, BingWallpaperSource, Http, HttpError, ImageCache, MockHttp, Request, SourceError};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn source_with(response: Bytes) -> BingWallpaperSource {
        let url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let mock = MockHttp::new().with(&url, response);
        BingWallpaperSource::new(Arc::new(mock), Arc::new(ImageCache::new()))
    }

    /// Test adapter that records which URLs were fetched and how many times,
    /// so the cache's request-coalescing and freshness behaviour can be
    /// asserted without touching the network.
    #[cfg(test)]
    #[derive(Default)]
    struct CountingHttp {
        responses: HashMap<String, Bytes>,
        fetch_count: AtomicUsize,
        fetched_urls: Mutex<Vec<String>>,
    }

    #[cfg(test)]
    impl CountingHttp {
        fn new() -> Self {
            Self::default()
        }

        fn with(mut self, url: &str, bytes: Bytes) -> Self {
            self.responses.insert(url.to_string(), bytes);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.fetched_urls.lock().unwrap().clone()
        }
    }

    #[cfg(test)]
    impl Http for CountingHttp {
        fn get(&self, url: &str) -> Pin<Box<dyn Future<Output = Result<Bytes, HttpError>> + Send>> {
            let url = url.to_string();
            let bytes = self.responses.get(&url).cloned();
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            self.fetched_urls.lock().unwrap().push(url.clone());
            Box::pin(async move {
                // A slow host so concurrent callers overlap in time.
                tokio::time::sleep(Duration::from_millis(100)).await;
                bytes.ok_or_else(|| HttpError::Network(format!("no mock response for {}", url)))
            })
        }
    }

    /// Build a counting mock whose Bing API response advertises `image_url` as
    /// the (only) wallpaper, with `image` as its bytes.
    fn counting_http(image_url: &str, image: Bytes) -> Arc<CountingHttp> {
        let api_url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let json = format!(
            r#"{{"MediaContents":[{{"ImageContent":{{"Image":{{"Url":"{}"}}}}}}]}}"#,
            image_url
        );
        Arc::new(
            CountingHttp::new()
                .with(&api_url, Bytes::from(json))
                .with(image_url, image),
        )
    }

    fn source(image_url: &str, image: Bytes) -> (BingWallpaperSource, Arc<CountingHttp>) {
        let counting = counting_http(image_url, image);
        let source = BingWallpaperSource::new(counting.clone(), Arc::new(ImageCache::new()));
        (source, counting)
    }

    #[tokio::test]
    async fn wallpaper_url_prepends_domain_when_path_has_no_origin() {
        let json = r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"/th?id=OHR.HiddenBeach_ZH-CN8410568637_1920x1080.jpg&rf=LaDigue_1920x1080.jpg&pid=hp"}}}]}"#;
        let source = source_with(Bytes::from(json.as_bytes()));
        let url = source.wallpaper_url(0).await.unwrap();
        assert!(url.starts_with("https://www.bing.com/th"));
    }

    #[tokio::test]
    async fn wallpaper_url_keeps_origin_when_path_already_has_one() {
        let json = r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"https://example.com/img.webp"}}}]}"#;
        let source = source_with(Bytes::from(json.as_bytes()));
        let url = source.wallpaper_url(0).await.unwrap();
        assert_eq!(url, "https://example.com/img.webp");
    }

    #[tokio::test]
    async fn wallpaper_url_returns_not_found_for_index_out_of_range() {
        let source = source_with(Bytes::from(
            r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"a"}}}]}"#,
        ));
        let err = source.wallpaper_url(5).await.unwrap_err();
        assert!(matches!(err, SourceError::NotFound(5)));
    }

    #[tokio::test]
    async fn fetch_image_returns_bytes_from_mock() {
        let image = Bytes::from_static(b"IMAGE");
        let mock = MockHttp::new().with("https://example.com/img.webp", image.clone());
        let source = BingWallpaperSource::new(Arc::new(mock), Arc::new(ImageCache::new()));
        let bytes = source
            .fetch_image("https://example.com/img.webp")
            .await
            .unwrap();
        assert_eq!(bytes, image);
    }

    fn get_request(uri: &str) -> Request<Full<Bytes>> {
        Request::builder().uri(uri).body(Full::new(Bytes::new())).unwrap()
    }

    #[tokio::test]
    async fn handle_returns_400_for_unparseable_query() {
        let source = source_with(Bytes::from(
            r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"a"}}}]}"#,
        ));
        let resp = handle(get_request("/?index_past=not-a-number"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn handle_returns_404_for_index_out_of_range() {
        let source = source_with(Bytes::from(
            r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"a"}}}]}"#,
        ));
        let resp = handle(get_request("/?index_past=5"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn handle_returns_502_when_upstream_is_unreachable() {
        // Empty mock: no response for the Bing API URL → network failure.
        let source = BingWallpaperSource::new(Arc::new(MockHttp::new()), Arc::new(ImageCache::new()));
        let resp = handle(get_request("/?index_past=0"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[tokio::test]
    async fn handle_returns_200_and_url_when_request_is_valid() {
        let json = r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"https://example.com/img.webp"}}}]}"#;
        let source = source_with(Bytes::from(json.as_bytes()));
        let resp = handle(get_request("/?index_past=0"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn handle_returns_500_for_malformed_upstream_response() {
        // Bing API responds 200 but with body that is not the expected JSON.
        let api_url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let mock = MockHttp::new().with(&api_url, Bytes::from_static(b"not json"));
        let source = BingWallpaperSource::new(Arc::new(mock), Arc::new(ImageCache::new()));
        let resp = handle(get_request("/?index_past=0"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
    }

    #[tokio::test]
    async fn handle_returns_image_bytes_when_get_image_is_true() {
        let api_url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let json = r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"https://example.com/img.webp"}}}]}"#;
        let image = Bytes::from_static(b"IMG");
        let mock = MockHttp::new()
            .with(&api_url, Bytes::from(json.as_bytes()))
            .with("https://example.com/img.webp", image.clone());
        let source = BingWallpaperSource::new(Arc::new(mock), Arc::new(ImageCache::new()));

        let resp = handle(get_request("/?index_past=0&get_image=true"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("Content-Type").unwrap(), "image/webp");
        let collected = BodyExt::collect(resp.into_body()).await.unwrap();
        assert_eq!(collected.to_bytes(), image);
    }

    #[tokio::test]
    async fn handle_returns_502_when_image_host_is_unreachable() {
        // Bing responds fine, but the image host has no response for the URL.
        let api_url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let json = r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"https://example.com/img.webp"}}}]}"#;
        let mock = MockHttp::new().with(&api_url, Bytes::from(json.as_bytes()));
        let source = BingWallpaperSource::new(Arc::new(mock), Arc::new(ImageCache::new()));
        let resp = handle(get_request("/?index_past=0&get_image=true"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    // --- Image cache behaviour ---

    #[tokio::test]
    async fn cached_image_serves_cached_bytes_and_slides_ttl() {
        let (source, counting) = source("https://example.com/img.webp", Bytes::from_static(b"IMG"));

        let first = source.cached_image("https://example.com/img.webp").await.unwrap();
        assert_eq!(first, Bytes::from_static(b"IMG"));
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 1);

        let expires_before = source
            .image_cache
            .inner
            .lock()
            .unwrap()
            .entries
            .get("https://example.com/img.webp")
            .unwrap()
            .expires_at;

        // Enough distance that a reseeded window is observably later.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let second = source.cached_image("https://example.com/img.webp").await.unwrap();
        assert_eq!(second, Bytes::from_static(b"IMG"));
        // Cache hit: no second upstream fetch.
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 1);

        let expires_after = source
            .image_cache
            .inner
            .lock()
            .unwrap()
            .entries
            .get("https://example.com/img.webp")
            .unwrap()
            .expires_at;
        assert!(expires_after > expires_before, "TTL should slide forward on hit");
    }

    #[tokio::test]
    async fn concurrent_requests_for_same_url_fetch_once() {
        let image = Bytes::from_static(b"IMG");
        let counting = counting_http("https://example.com/img.webp", image.clone());
        // All callers must share one cache for coalescing to happen.
        let shared_cache = Arc::new(ImageCache::new());

        let barrier = Arc::new(tokio::sync::Barrier::new(5));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let http: Arc<dyn Http> = counting.clone();
            let source = BingWallpaperSource::new(http, Arc::clone(&shared_cache));
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                source.cached_image("https://example.com/img.webp").await
            }));
        }
        for h in handles {
            let bytes = h.await.unwrap().unwrap();
            assert_eq!(bytes, image);
        }
        // Single flight: exactly one upstream fetch despite five concurrent callers.
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 1);
        assert_eq!(counting.calls(), vec!["https://example.com/img.webp".to_string()]);
    }

    #[tokio::test]
    async fn expired_entry_is_fetched_again() {
        let image = Bytes::from_static(b"IMG");
        let counting = counting_http("https://example.com/img.webp", image.clone());
        let cache = Arc::new(ImageCache::new());
        let source = BingWallpaperSource::new(counting.clone(), Arc::clone(&cache));

        source.cached_image("https://example.com/img.webp").await.unwrap();
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 1);

        // Force the entry to expire.
        cache
            .inner
            .lock()
            .unwrap()
            .entries
            .get_mut("https://example.com/img.webp")
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);

        source.cached_image("https://example.com/img.webp").await.unwrap();
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetch_error_is_not_cached() {
        // No image response: the image host is unreachable.
        let api_url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let json = r#"{"MediaContents":[{"ImageContent":{"Image":{"Url":"https://example.com/img.webp"}}}]}"#;
        let counting = Arc::new(CountingHttp::new().with(&api_url, Bytes::from(json.as_bytes())));
        let cache = Arc::new(ImageCache::new());
        let source = BingWallpaperSource::new(counting.clone(), Arc::clone(&cache));

        let err = source.cached_image("https://example.com/img.webp").await.unwrap_err();
        assert!(matches!(err, SourceError::ImageFetch(_)));
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 1);

        // A second request must re-fetch rather than serving a cached failure.
        source.cached_image("https://example.com/img.webp").await.unwrap_err();
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 2);
        assert!(cache.inner.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn url_only_request_does_not_populate_image_cache() {
        let image = Bytes::from_static(b"IMG");
        let counting = counting_http("https://example.com/img.webp", image.clone());
        let cache = Arc::new(ImageCache::new());
        let source = BingWallpaperSource::new(counting.clone(), Arc::clone(&cache));

        let resp = handle(get_request("/?index_past=0"), &source).await.unwrap();
        assert_eq!(resp.status(), 200);

        // The URL path never downloads the image, so the cache stays empty and
        // the image host was never contacted.
        assert!(cache.inner.lock().unwrap().entries.is_empty());
        assert!(cache.inner.lock().unwrap().in_flight.is_empty());
        assert_eq!(counting.fetch_count.load(Ordering::SeqCst), 1);
        assert_eq!(counting.calls(), vec![format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH)]);
    }
}