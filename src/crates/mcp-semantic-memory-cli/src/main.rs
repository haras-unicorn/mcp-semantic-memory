//! The command line program: reads the arguments and the configuration, then
//! runs what was asked for.

mod args;
mod log;
mod run;

use anyhow::Result;
use clap::Parser;
use mcp_semantic_memory::config::Config;

use crate::args::Args;

#[tokio::main]
async fn main() -> Result<()> {
  log::init()?;

  let args = Args::parse();
  let config = Config::load(args.config.as_deref())?;
  run::run(args.command, config).await
}
