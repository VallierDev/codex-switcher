//! Apply the same skill mirror rules without restarting the running proxy.
#[path = "../src/skills_pull.rs"]
mod skills_pull;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("export") if args.len() == 5 => {
            let bundle = skills_pull::export(std::path::Path::new(&args[2]), std::path::Path::new(&args[3]))?;
            let count = bundle.skills.len();
            let skipped = bundle.skipped.len();
            std::fs::write(&args[4], serde_json::to_vec(&bundle)?)?;
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&args[4], std::fs::Permissions::from_mode(0o600))?;
            }
            println!("exported={count} skipped={skipped}");
        }
        Some("apply") if args.len() == 4 => {
            let home = std::path::Path::new(&args[3]);
            let config: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join(".codex-switcher/accounts.json"))?)?;
            let settings = &config["settings"];
            let primary = settings["remote_server_url"].as_str().unwrap_or_default();
            let fallback = settings["remote_server_url_fallback"].as_str().unwrap_or_default();
            let source = format!("{}|{}", primary.trim_end_matches('/'), fallback.trim_end_matches('/'));
            let blacklist: Vec<String> = serde_json::from_value(settings["skills_sync_blacklist"].clone()).unwrap_or_default();
            let bundle = serde_json::from_slice(&std::fs::read(&args[2])?)?;
            let report = skills_pull::apply(bundle, &home.join(".codex-switcher/skills"), home, std::env::consts::OS, &source, &blacklist)?;
            println!("updated={} unchanged={} skipped={} conflicts={} retired={}", report.updated.len(), report.unchanged.len(), report.skipped.len(), report.conflicts.len(), report.retired.len());
            for (name, reason) in report.skipped { println!("skipped {name}: {reason}"); }
            for name in report.conflicts { println!("conflict {name}"); }
        }
        _ => return Err("Usage: skills_mirror export ROOT HOME OUTPUT | apply BUNDLE HOME".into()),
    }
    Ok(())
}
