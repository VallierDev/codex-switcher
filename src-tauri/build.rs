fn main() {
    println!("cargo:rerun-if-env-changed=CODEX_SWITCHER_ANTIGRAVITY_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=CODEX_SWITCHER_ANTIGRAVITY_CLIENT_SECRET");
    tauri_build::build()
}
