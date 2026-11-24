//! Verify production UI assets without starting the GUI or touching user state.
fn main() {
    assert!(cfg!(feature = "custom-protocol"), "build with --features custom-protocol");
    let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
    let index = context.assets().get(&"index.html".into()).expect("production index.html missing");
    let index = std::str::from_utf8(&index).expect("index.html is not UTF-8");
    assert!(index.contains("assets/"), "built frontend scripts missing");
    for path in index.split('"').filter_map(|s| s.strip_prefix("/assets/")) {
        assert!(context.assets().get(&format!("assets/{path}").into()).is_some(), "missing asset: {path}");
    }
    println!("PRODUCTION_UI_ASSETS_VERIFIED");
}
