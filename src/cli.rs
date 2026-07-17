use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "ovenmitts",
    version,
    about = "Archival BD-R / M-DISC burning with parity and cache-proof verification"
)]
pub struct Cli {
    /// Payload files; with no subcommand this opens the TUI wizard
    pub payloads: Vec<PathBuf>,

    #[arg(long, global = true)]
    pub device: Option<String>,

    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Plain line output even on a TTY
    #[arg(long, global = true)]
    pub no_tui: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Full pipeline: preflight, parity, checksums, master, burn, verify
    Burn {
        payloads: Vec<PathBuf>,
        /// Volume label (A-Z 0-9 _, max 32 chars); default ARCHIVE_YYYYMMDD
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        speed: Option<u32>,
        /// par2 redundancy percent
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        redundancy: Option<u32>,
        #[arg(long)]
        staging: Option<PathBuf>,
        /// Format with spare areas (drive-level defect management) before burning
        #[arg(long)]
        defect_management: bool,
        #[arg(long)]
        no_parity: bool,
        /// Stop after planning; burn nothing
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Delete the staged ISO after successful verify (default keeps it for copy 2)
        #[arg(long)]
        discard_iso: bool,
    },
    /// Burn an existing ISO (bit-identical second copy) and verify it
    BurnIso {
        iso: PathBuf,
        #[arg(long)]
        speed: Option<u32>,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Verify a burned disc: image read-back vs ISO (if given) plus mounted file checksums
    Verify {
        /// Staged ISO to compare byte-for-byte; without it only mounted checksums + MD5 tags run
        #[arg(long)]
        iso: Option<PathBuf>,
    },
    /// Periodic disc health check via embedded MD5 tags (no source ISO needed)
    Check {
        /// Write the auto-detected device to the config file
        #[arg(long)]
        save: bool,
    },
    /// Show drive and media info (type, capacity, formatted state, speeds, media ID)
    Info {
        /// Write the auto-detected device to the config file
        #[arg(long)]
        save: bool,
    },
    /// Update ovenmitts to the latest release (re-runs the installer)
    Update,
    /// Capacity math without a disc: does the payload + parity fit?
    Plan {
        payloads: Vec<PathBuf>,
        /// Assume media: bd25, bd50, bd100, bd128, dvdr (default bd25)
        #[arg(long)]
        media: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        redundancy: Option<u32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn check_and_info_accept_save() {
        let cli = Cli::try_parse_from(["ovenmitts", "check", "--save"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Check { save: true })));
        let cli = Cli::try_parse_from(["ovenmitts", "info"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Info { save: false })));
    }

    #[test]
    fn update_parses_as_a_subcommand_not_a_payload() {
        let cli = Cli::try_parse_from(["ovenmitts", "update"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update)));
        assert!(cli.payloads.is_empty());
    }
}
