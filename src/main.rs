use http_body_util::Full;
use hyper::service::{service_fn};
use hyper::server::conn::http1;
use hyper::{Request, Response};
use hyper_util::rt::tokio::TokioIo;
use reqwest;
use serde_derive::{Deserialize, Serialize};
use serde_json;
use serde_qs as qs;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;

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

async fn request_bing() -> Result<serde_json::Value, Infallible> {
    let res_raw = reqwest::get(format!(
        "{}{}",
        BING_DOMAIN,
        BING_API_PATH,
    )).await.unwrap();

    let res: serde_json::Value = res_raw.json().await.unwrap();

    Ok(res)
}

async fn fetch_image(url: String) -> Result<Bytes, Infallible> {
    let response = reqwest::get(url).await.unwrap();
    let bytes = response.bytes().await.unwrap();
    Ok(bytes)
}

async fn handle(req: Request<impl hyper::body::Body>) -> Result<Response<Full<Bytes>>, Infallible> {
    // Processing request arguments

    let received_querystring: &str = req.uri().query().unwrap_or("");
    let received_query: RequestQueryParams = qs::from_str(received_querystring).unwrap();

    // Get image URL

    let res = request_bing().await.unwrap();

    let path = res["MediaContents"][received_query.index_past]["ImageContent"]["Image"]["Url"].as_str().unwrap().to_owned();
    println!("Got image path: {}", path);
    // If no origin in path, use BING_DOMAIN.
    let url = if !path.contains("http") {
        format!("{}{}", BING_DOMAIN, path)
    } else {
        path
    };

    let response_builder = Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Headers", "*")
        .header("Access-Control-Allow-Method", "*");
    let response;

    if received_query.get_image {
        // Get image data

        let image_bytes = fetch_image(url).await.unwrap();
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
    let addr:SocketAddr = LISTEN_ADDRESS.parse().unwrap();
    
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
            // Use HTTP/1 process connection and bring request to handle function
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(handle))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
