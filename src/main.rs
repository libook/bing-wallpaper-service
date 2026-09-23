use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::tokio::TokioIo;
use reqwest;
use serde_derive::{Deserialize, Serialize};
use serde_qs as qs;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::TcpListener;

#[cfg(test)]
use std::collections::HashMap;

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
#[derive(Debug)]
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
#[derive(Debug)]
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
}

impl BingWallpaperSource {
    fn new(http: Arc<dyn Http>) -> Self {
        Self { http }
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

    async fn fetch_image(&self, url: String) -> Result<Bytes, SourceError> {
        self.http
            .get(&url)
            .await
            .map_err(|e| SourceError::ImageFetch(e.to_string()))
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
        // Get image data

        let image_bytes = match source.fetch_image(url).await {
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

    loop {
        // Accept TCP connection
        let (tcp, _) = listener.accept().await?;

        // Convert TcpStream to type hyper need
        let io = TokioIo::new(tcp);

        // Process the connection in a new task
        tokio::task::spawn(async move {
            let source = BingWallpaperSource::new(Arc::new(ReqwestHttp));

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
    use super::{handle, BingWallpaperSource, MockHttp, Request, SourceError};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use std::sync::Arc;

    fn source_with(response: Bytes) -> BingWallpaperSource {
        let url = format!("{}{}", super::BING_DOMAIN, super::BING_API_PATH);
        let mock = MockHttp::new().with(&url, response);
        BingWallpaperSource::new(Arc::new(mock))
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
        let source = BingWallpaperSource::new(Arc::new(mock));
        let bytes = source
            .fetch_image("https://example.com/img.webp".to_string())
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
        let source = BingWallpaperSource::new(Arc::new(MockHttp::new()));
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
        let source = BingWallpaperSource::new(Arc::new(mock));
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
        let source = BingWallpaperSource::new(Arc::new(mock));

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
        let source = BingWallpaperSource::new(Arc::new(mock));
        let resp = handle(get_request("/?index_past=0&get_image=true"), &source)
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }
}