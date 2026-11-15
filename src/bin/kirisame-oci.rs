//! `kirisame-oci`: the OCI runtime shim registered with Docker. It joins
//! containers to kirisame pod namespaces, then execs the real runtime.
//! See `kirisame::oci` for the details.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kirisame::oci;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let inv = oci::parse_invocation(&args);
    let runtime = std::env::var("KIRISAME_OCI_RUNTIME").unwrap_or_else(|_| "runc".into());

    if matches!(inv.subcommand.as_deref(), Some("create") | Some("run")) {
        let bundle = inv.bundle.clone().unwrap_or_else(|| ".".into());
        let config = Path::new(&bundle).join("config.json");
        match patch(&config) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        }
    }

    let err = Command::new(&runtime).args(&args).exec();
    eprintln!("kirisame-oci: exec {runtime}: {err}");
    ExitCode::from(127)
}

/// Rewrites config.json in place when the container belongs to a pod.
fn patch(config: &Path) -> Result<(), String> {
    let text = std::fs::read(config)
        .map_err(|e| format!("kirisame-oci: read {}: {e}", config.display()))?;
    let mut spec: serde_json::Value = serde_json::from_slice(&text)
        .map_err(|e| format!("kirisame-oci: parse {}: {e}", config.display()))?;
    let exists = |p: &str| Path::new(p).exists();
    if oci::rewrite_spec(&mut spec, &exists)? {
        let out =
            serde_json::to_vec(&spec).map_err(|e| format!("kirisame-oci: encode spec: {e}"))?;
        let tmp = config.with_extension("json.kirisame");
        std::fs::write(&tmp, out)
            .map_err(|e| format!("kirisame-oci: write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, config)
            .map_err(|e| format!("kirisame-oci: replace {}: {e}", config.display()))?;
    }
    Ok(())
}
