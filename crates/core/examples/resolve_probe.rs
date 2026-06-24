//! Verify resolve_to_edge against the live network: given a mirror get.php URL, it
//! should return the cdnN.booksdl.lc edge URL (redirect-disabled resolution).
//! Usage: cargo run -p libgen-core --example resolve_probe -- '<get.php url>'
use libgen_core::download::resolve_to_edge;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: resolve_probe <get.php url>");
    let cancel = CancellationToken::new();
    let edge = resolve_to_edge(&url, &cancel).await;
    println!("input : {url}");
    println!("edge  : {edge}");
    println!(
        "result: {}",
        if edge != url && edge.contains(".booksdl.lc") {
            "OK — resolved to a cdn edge"
        } else if edge == url {
            "unchanged (no booksdl redirect / already edge / resolve failed)"
        } else {
            "changed but not a booksdl edge"
        }
    );
}
