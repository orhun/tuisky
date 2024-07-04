use clap::Parser;
use color_eyre::Result;
use std::fs;
use std::path::PathBuf;
use tuisky::app::App;
use tuisky::config::Config;
use tuisky::utils::{get_config_dir, initialize_panic_handler};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
/// TUI Client for Bluesky.
struct Args {
    /// Path to the configuration file.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Maximum number of columns to display.
    /// The number of columns will be determined by the terminal width.
    #[arg(short, long)]
    num_columns: Option<usize>,
    /// Development mode
    #[arg(short, long)]
    dev: bool,
}

impl Args {
    fn config_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.config {
            Ok(path.clone())
        } else {
            Self::default_config_path()
        }
    }
    fn default_config_path() -> Result<PathBuf> {
        let config_dir = get_config_dir()?;
        fs::create_dir_all(&config_dir)?;
        Ok(config_dir.join("tuisky.config.json"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = if args.config_path()?.exists() {
        toml::from_str(&fs::read_to_string(args.config_path()?)?)?
    } else {
        Config::default()
    };
    config.set_default_keybindings();
    if let Some(num_columns) = args.num_columns {
        config.num_columns = Some(num_columns);
    }
    config.dev |= args.dev;

    if let Err(e) = tui_logger::init_logger(log::LevelFilter::Debug) {
        panic!("failed to initialize logger: {e}");
    }
    tui_logger::set_default_level(log::LevelFilter::Debug);

    initialize_panic_handler()?;

    App::new(config).run().await
}
