//! Dump the ViewLibrary JSON for a given DB, so the headless UI harness can be
//! driven with REAL data (the exact shape the front end receives) rather than an
//! idealized fixture. Usage: `cargo run -p libgen-app --example dump_vm -- <db>`.
use libgen_app_lib::viewmodel::build_with_id;
use libgen_core::store::Store;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: dump_vm <db-path>");
    let store = Store::open(&path)?;
    let mut lists = Vec::new();
    for sl in store.all_lists()? {
        lists.push(build_with_id(format!("L{}", sl.id), &sl.list));
    }
    let out = serde_json::json!({ "lists": lists, "current": "__all__" });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
