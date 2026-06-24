// Throwaway diagnostic: load every list from a real on-disk DB and report errors.
use libgen_core::store::Store;

#[test]
#[ignore]
fn load_real_db() {
    let path = std::env::var("REAL_DB").expect("set REAL_DB");
    let store = Store::open(&path).expect("open store");
    let lists = store.all_lists().expect("all_lists");
    println!("lists: {}", lists.len());
    for sl in &lists {
        match store.load_list(sl.id) {
            Ok(Some(l)) => {
                let books: usize = l.groups.iter().map(|g| g.books.len()).sum();
                let cands: usize = l
                    .groups
                    .iter()
                    .flat_map(|g| &g.books)
                    .map(|b| b.candidates.len())
                    .sum();
                println!(
                    "  OK list {} '{}': {} books, {} candidates",
                    sl.id, l.title, books, cands
                );
            }
            Ok(None) => println!("  list {} -> None", sl.id),
            Err(e) => println!("  ERR list {}: {e:#}", sl.id),
        }
    }
}
