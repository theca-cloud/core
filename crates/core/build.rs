// Rebuilds when a migration is added or changed: `sqlx::migrate!` embeds them,
// and cargo doesn't see new files by itself (sqlx's `migrate build-script`).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
