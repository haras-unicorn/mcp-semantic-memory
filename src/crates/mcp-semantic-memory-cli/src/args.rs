//! The command line.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// The arguments of the program.
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Args {
  /// A TOML, YAML or JSON file to read the configuration from. Environment
  /// variables win over it.
  #[arg(long, global = true, value_name = "PATH")]
  pub config: Option<PathBuf>,

  /// What to do.
  #[command(subcommand)]
  pub command: Command,
}

/// What the program can do.
#[derive(Debug, Subcommand)]
pub enum Command {
  /// Apply the migrations and exit.
  Migrate,
  /// Apply the migrations, then serve over stdio.
  Run,
  /// Find the interests and forget what faded. Only reports what it would do
  /// unless `--apply` is given.
  Consolidate {
    /// Write the result instead of only reporting it.
    #[arg(long)]
    apply: bool,
  },
  /// Embed every memory again, after changing the embedding model.
  Reembed,
}

#[cfg(test)]
mod tests {
  use super::*;

  const PROGRAM: &str = "mcp-semantic-memory";

  #[test]
  fn the_config_path_may_come_before_or_after_the_subcommand() {
    for arguments in [
      [PROGRAM, "--config", "a.toml", "run"],
      [PROGRAM, "run", "--config", "a.toml"],
    ] {
      let args = Args::try_parse_from(arguments).unwrap();
      assert_eq!(args.config, Some(PathBuf::from("a.toml")));
      assert!(matches!(args.command, Command::Run));
    }
  }

  #[test]
  fn there_is_no_config_path_by_default() {
    let args = Args::try_parse_from([PROGRAM, "migrate"]).unwrap();
    assert_eq!(args.config, None);
    assert!(matches!(args.command, Command::Migrate));
  }

  #[test]
  fn consolidate_only_applies_when_asked() {
    let dry = Args::try_parse_from([PROGRAM, "consolidate"]).unwrap();
    assert!(matches!(dry.command, Command::Consolidate { apply: false }));
    let apply =
      Args::try_parse_from([PROGRAM, "consolidate", "--apply"]).unwrap();
    assert!(matches!(
      apply.command,
      Command::Consolidate { apply: true }
    ));
  }

  #[test]
  fn reembed_takes_no_arguments() {
    let args = Args::try_parse_from([PROGRAM, "reembed"]).unwrap();
    assert!(matches!(args.command, Command::Reembed));
  }

  #[test]
  fn a_subcommand_is_required() {
    assert!(Args::try_parse_from([PROGRAM]).is_err());
  }
}
