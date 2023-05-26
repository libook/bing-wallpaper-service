use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use reqwest;
use serde_derive::{Deserialize, Serialize};
use serde_json;
use serde_qs as qs;
use std::convert::Infallible;

static BING_DOMAIN: &str = "https://www.bing.com";
static BING_API_PATH: &str = "/HPImageArchive.aspx";
// static BING_API_QUERYSTRING: &str = "format=js&idx=0&n=1&mkt=en-US";
static LISTEN_ADDRESS: &str = "127.0.0.1:3000";

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

async fn request_bing(received_query: BingQueryParams) -> Result<serde_json::Value, Infallible> {
    let mut bing_query = BingQueryParams {
        ..Default::default()
    };

    {
        bing_query.idx = received_query.idx;
        bing_query.n = received_query.n;
        bing_query.mkt = received_query.mkt;
    }

    let res_raw = reqwest::get(format!(
        "{}{}?{}",
        BING_DOMAIN,
        BING_API_PATH,
        serde_qs::to_string(&bing_query).unwrap(),
    )).await.unwrap();

    let res: serde_json::Value = res_raw.json().await.unwrap();

    Ok(res)
}

async fn handle(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let received_querystring: &str = req.uri().query().unwrap_or("");
    let received_query: BingQueryParams = qs::from_str(received_querystring).unwrap();

    let res = request_bing(received_query).await.unwrap();

    let url = format!(
        "{}{}",
        BING_DOMAIN,
        res["images"][0]["url"].as_str().unwrap().to_owned()
    );

    let response = Response::new(Body::from(url));

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
