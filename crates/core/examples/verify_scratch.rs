//! Verify the NEW download behavior end-to-end against the live edge: seed a partial
//! .part, then run the (fixed) download_to. If the edge ignores Range (HTTP 200) the fix
//! must restart from scratch and the md5 must verify; if the edge hangs on the resume it
//! should fail (server-side), not loop. This exercises the real fixed code path
//! (resolve_to_edge + edge rotation + 200→scratch), not just a raw curl.
//!
//! Usage: cargo run -p libgen-core --example verify_scratch -- <md5> <get.php-url> <partial_bytes> [timeout_secs]
use libgen_core::download::{download_to, part_path, DownloadTarget};

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: verify_scratch <md5> <get.php-url> <partial_bytes> [timeout_secs]");
        std::process::exit(2);
    }
    let (md5, url) = (a[1].clone(), a[2].clone());
    let partial: u64 = a[3].parse().unwrap();
    let timeout = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(120u64);
    let dest = std::env::temp_dir().join(format!("verify-{md5}.bin"));
    let part = part_path(&dest);
    let _ = std::fs::remove_file(&dest);
    std::fs::write(&part, vec![0u8; partial as usize]).unwrap(); // seed the stuck partial
    let target = DownloadTarget {
        url,
        host: "libgen.vg".into(),
        expected_md5: Some(md5.clone()),
        total_bytes: None,
    };
    println!(
        "seeded {partial}-byte partial; running fixed download_to (resume_offset={partial}) …"
    );
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        download_to(&target, &dest, partial),
    )
    .await;
    match res {
        Ok(Ok(n)) => println!(
            "✅ COMPLETED {n} bytes, md5 VERIFIED — fix restarted from scratch and finished"
        ),
        Ok(Err(e)) => println!("❌ FAILED (no loop): {e}"),
        Err(_) => println!("⏱  timed out after {timeout}s (edge hangs on resume — server-side)"),
    }
    let _ = std::fs::remove_file(&dest);
    let _ = std::fs::remove_file(&part);
}
