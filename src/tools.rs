use std::path::PathBuf;

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct Tools {
    pub xorriso: PathBuf,
    pub par2: Option<PathBuf>,
    pub par2_version: Option<String>,
    pub udisksctl: Option<PathBuf>,
    pub veracrypt: Option<PathBuf>,
    pub eject: Option<PathBuf>,
    pub mediainfo: Option<PathBuf>,
    pub dvdisaster: Option<PathBuf>,
}

impl Tools {
    /// Minimal Tools: just an xorriso path, every optional tool absent.
    pub fn bare(xorriso: impl Into<std::path::PathBuf>) -> Tools {
        Tools {
            xorriso: xorriso.into(),
            par2: None,
            par2_version: None,
            udisksctl: None,
            veracrypt: None,
            eject: None,
            mediainfo: None,
            dvdisaster: None,
        }
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join(name))
        .find(|c| c.is_file() && is_executable(c))
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// For commands that work without xorriso (plan): placeholder path whose
/// spawn failure routes callers onto their synthetic-media fallback.
pub fn lenient() -> Tools {
    let mut t = Tools::bare("xorriso");
    t.par2 = which("par2");
    t
}

pub fn discover() -> Result<Tools> {
    let Some(xorriso) = which("xorriso") else {
        bail!(
            "xorriso not found - it is the one required tool \
             (Debian/Ubuntu: sudo apt install xorriso)"
        );
    };
    let par2 = which("par2");
    let par2_version = par2.as_ref().and_then(|p| {
        let out =
            crate::proc::output_deadline(p, &["-V".into()], crate::proc::SHORT_OP_DEADLINE).ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        text.lines().next().map(|l| l.trim().to_string())
    });
    Ok(Tools {
        xorriso,
        par2,
        par2_version,
        udisksctl: which("udisksctl"),
        veracrypt: which("veracrypt"),
        eject: which("eject"),
        mediainfo: which("dvd+rw-mediainfo"),
        dvdisaster: which("dvdisaster"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }
}
