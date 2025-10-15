pub mod cli;
pub mod discovery;
pub mod search;
pub mod session;
pub mod tui;
pub mod util;

use anyhow::Result;

pub const DEFAULT_LIMIT: usize = 20;

pub fn run() -> Result<()> {
    cli::run()
}
