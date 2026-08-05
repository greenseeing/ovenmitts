//! Shared process-execution layer. Every external tool is spawned through
//! [`command`] so its output is locale-independent: `LC_ALL=C` forces `.`
//! decimal separators and English keywords, without which the progress and
//! capacity parsers (`38.2%` vs `38,2%`, `23.3g`, `MB written`) silently
//! misread under a non-C locale — and a misread capacity feeds the fit gate.

use std::path::Path;
use std::process::{Command, Stdio};

/// A `Command` for `bin` with a C locale and no stdin. Callers add args and
/// choose how to capture output (`.output()`, `.status()`, piped streaming).
pub(crate) fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    // LC_ALL overrides LANG and every LC_* category, so one variable is enough.
    cmd.env("LC_ALL", "C").stdin(Stdio::null());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_forces_c_locale() {
        // A child that echoes its own LC_ALL must see "C" regardless of the
        // caller's environment.
        std::env::set_var("LC_ALL", "de_DE.UTF-8");
        let out = command(Path::new("/bin/sh"))
            .arg("-c")
            .arg("printf %s \"$LC_ALL\"")
            .output()
            .unwrap();
        std::env::remove_var("LC_ALL");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "C");
    }
}
