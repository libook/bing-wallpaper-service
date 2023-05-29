use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use reqwest;
use serde_derive::{Deserialize, Serialize};
use serde_json;
use serde_qs as qs;
use std::convert::Infallible;
use bytes::Bytes;

static BING_DOMAIN: &str = "https://www.bing.com";
static BING_API_PATH: &str = "/HPImageArchive.aspx";
// static BING_API_QUERYSTRING: &str = "format=js&idx=0&n=1&mkt=en-US";
static LISTEN_ADDRESS: &str = "0.0.0.0:3000";

#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
struct RequestQueryParams {
    index_past: usize,
    number: usize,
    locale: String,
    get_image: bool,
}

impl Default for RequestQueryParams {
    fn default() -> Self {
        RequestQueryParams {
            index_past: 0,
            number: 1,
            locale: "en-US".to_string(),
            get_image: false,
        }
    }
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
struct BingQueryParams {
    format: String,
    idx: usize,
    n: usize,
    mkt: String,
}

impl Default for BingQueryParams {
    fn default() -> Self {
        BingQueryParams {
            format: "js".to_string(),
            idx: 0,
            n: 1,
            mkt: "en-US".to_string(),
        }
    }
}

async fn request_bing(bing_query: BingQueryParams) -> Result<serde_json::Value, Infallible> {
    let res_raw = reqwest::get(format!(
        "{}{}?{}",
        BING_DOMAIN,
        BING_API_PATH,
        serde_qs::to_string(&bing_query).unwrap(),
    )).await.unwrap();

    let res: serde_json::Value = res_raw.json().await.unwrap();

    Ok(res)
}

async fn fetch_image(url: String) -> Result<Bytes, Infallible> {
    let response = reqwest::get(url).await.unwrap();
    let bytes = response.bytes().await.unwrap();
    Ok(bytes)
}

async fn handle(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    // Processing request arguments

    let received_querystring: &str = req.uri().query().unwrap_or("");
    let received_query: RequestQueryParams = qs::from_str(received_querystring).unwrap();

    let mut bing_query = BingQueryParams {
        ..Default::default()
    };

    {
        bing_query.idx = received_query.index_past;
        bing_query.n = received_query.number;
        bing_query.mkt = received_query.locale;
    }

    // Get image URL

    let res = request_bing(bing_query).await.unwrap();

    let url = format!(
        "{}{}",
        BING_DOMAIN,
        res["images"][0]["url"].as_str().unwrap().to_owned()
    );

    let response_builder = Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Headers", "*")
        .header("Access-Control-Allow-Method", "*");
    let response;

    if received_query.get_image {
        // Get image data

        let image_bytes = fetch_image(url).await.unwrap();
        response = response_builder
            .header("Content-Type", "image/jpeg")
            .body(Body::from(image_bytes))
            .unwrap();
    } else {
        response = response_builder.body(Body::from(url)).unwrap();
    }
    Ok(response)
}

#[tokio::main]
async fn main() {
    let addr = LISTEN_ADDRESS.parse().unwrap();
    let make_service = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle)) });
    let server = Server::bind(&addr).serve(make_service);
    {
        let bound_addr = server.local_addr();
        tokio::spawn(async move {
            println!("Server started at http://{}", bound_addr.to_string());
            // do other things here
        });
    }
    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
