//! `ovenmitts update` — re-run the published installer to upgrade in place.
//! The installer already resolves the latest release, verifies the SHA-256
//! sidecar, replaces the binary, and no-ops when current, so update stays a
//! thin wrapper instead of a second copy of that logic. `OVENMITTS_VERSION`
//! pinning works unchanged: the environment passes straight through.

use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::tools::which;

fn installer_url() -> String {
    "https://codeberg.org/greenseer/ovenmitts/raw/branch/main/install.sh".to_string()
}

// pipefail: a failed download must surface as a failure instead of feeding
// bash empty input, which would exit 0 and look like a successful update
fn installer_pipeline() -> String {
    format!("set -o pipefail; curl -fsSL {} | bash", installer_url())
}

pub fn run() -> Result<()> {
    if which("curl").is_none() {
        bail!("curl is required to update; install it and try again");
    }
    if which("bash").is_none() {
        bail!("bash is required to update; install it and try again");
    }

    println!("Updating ovenmitts from {}\n", installer_url());
    let status = Command::new("bash")
        .arg("-c")
        .arg(installer_pipeline())
        .status()
        .context("could not launch the updater")?;

    if !status.success() {
        bail!("the updater did not finish successfully");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_fetches_the_installer_from_codeberg_main() {
        let p = installer_pipeline();
        assert!(p.contains("set -o pipefail"), "missing pipefail guard: {p}");
        assert!(
            p.contains("https://codeberg.org/greenseer/ovenmitts/raw/branch/main/install.sh"),
            "installer not fetched from the Codeberg raw main URL: {p}"
        );
        assert!(
            !p.contains("github") && !p.contains("githubusercontent"),
            "installer must not reference GitHub: {p}"
        );
        assert!(p.contains("| bash"), "installer is not piped to bash: {p}");
    }
}
