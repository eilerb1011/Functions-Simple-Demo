use spin_sdk::http::{IntoResponse, Request, Response};
use spin_sdk::http_component;

#[http_component]
fn handle_trending(_req: Request) -> anyhow::Result<impl IntoResponse> {
    let html = "
        <p>☔ <b>STORM PREP 101</b> - Buy edge-powered umbrellas today!</p>
        <p>🏖️ <b>BEACH GETAWAY</b> - Escape the cold with Linode Travel.</p>
    ";
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/html")
        .body(html)
        .build())
}
fn main() {}
