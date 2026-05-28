//! ExfilZone Assistant → ez-wishlist-overlay data pipeline.
//!
//! Run from the workspace root, e.g.:
//!
//! ```text
//! cargo run -p scraper -- --output crates/app/src/assets
//! ```

mod items;
mod model;
mod output;
mod parse_ts;
mod tasks;
mod upstream;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use tracing_subscriber::EnvFilter;

use crate::model::GameData;

const DEFAULT_REPO: &str = "https://github.com/zelengeo/exfil-zone-assistant";
const DEFAULT_REF: &str = "master";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Scrape ExfilZone Assistant data for the wishlist overlay app"
)]
struct Args {
    /// Output directory for data.json + icons/ + SOURCE.md.
    #[arg(long, default_value = "crates/app/src/assets")]
    output: PathBuf,

    /// Upstream git URL.
    #[arg(long, default_value = DEFAULT_REPO)]
    repo: String,

    /// Branch / tag / commit to check out.
    #[arg(long, default_value = DEFAULT_REF)]
    r#ref: String,

    /// Use a pre-cloned upstream at this directory instead of cloning.
    #[arg(long)]
    upstream: Option<PathBuf>,

    /// Don't delete the cloned upstream directory on success.
    #[arg(long)]
    keep_temp: bool,

    /// Skip icon copying (data.json only). Faster for iteration.
    #[arg(long)]
    skip_icons: bool,

    /// Require --upstream; refuse to clone. CI-friendly.
    #[arg(long)]
    no_network: bool,

    /// Verbose logging.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);

    let upstream = if let Some(p) = &args.upstream {
        tracing::info!(path = %p.display(), "using existing upstream checkout");
        upstream::Upstream::from_existing(p)?
    } else if args.no_network {
        anyhow::bail!("--no-network was set but --upstream wasn't provided");
    } else {
        let into = std::env::temp_dir().join("ez-wishlist-overlay-scraper");
        tracing::info!(repo = %args.repo, ref_ = %args.r#ref, into = %into.display(), "cloning");
        upstream::Upstream::clone(&args.repo, &args.r#ref, &into)?
    };

    let public_data = upstream.root.join("public").join("data");
    let tasks_ts = upstream.root.join("src").join("data").join("tasks.ts");

    let catalog = items::ItemCatalog::from_upstream(&public_data)?;
    let tasks = tasks::parse(&tasks_ts, &catalog).context("tasks parse")?;

    // Hideout module data is hand-validated against in-game screenshots (see
    // hideout_screenshots/CLAUDE.md). The scraper must never overwrite it,
    // so we read the existing data.json and carry its `modules` field over.
    let existing_modules = read_existing_modules(&args.output.join("data.json"));

    let items_out = catalog.build_output_items_misc(output::ICON_DIR_NAME);
    let emitted_ids: HashSet<String> = items_out.iter().map(|i| i.id.clone()).collect();

    let version = upstream::upstream_package_version(&upstream.root).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not read upstream package.json version");
        "unknown".to_string()
    });
    let commit_short = upstream.commit.chars().take(7).collect::<String>();
    let data = GameData {
        data_version: format!("{version}+{commit_short}"),
        scraped_at: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string()),
        source_repo: args.repo.clone(),
        source_commit: upstream.commit.clone(),
        modules: existing_modules,
        vendors: tasks.vendors,
        items: items_out,
    };

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("creating {}", args.output.display()))?;
    output::write_data_json(&args.output, &data)?;
    output::write_source_md(&args.output, &data, tasks.unparsed_objectives.len())?;

    let icons_written = if !args.skip_icons {
        let outcome = output::copy_icons(&args.output, &upstream.root, &catalog, &emitted_ids)?;
        if !outcome.icons_missing.is_empty() {
            tracing::warn!(
                count = outcome.icons_missing.len(),
                "items without usable icons (will render as placeholders in app)"
            );
        }
        outcome.icons_written
    } else {
        0
    };

    if !tasks.unparsed_objectives.is_empty() {
        let log_path = args.output.join("unparsed-objectives.log");
        let lines: Vec<String> = tasks
            .unparsed_objectives
            .iter()
            .map(|u| format!("[{}] {} :: {}", u.task_id, u.objective, u.reason))
            .collect();
        std::fs::write(&log_path, lines.join("\n"))?;
        tracing::info!(path = %log_path.display(), "wrote unparsed-objectives log");
    }

    println!();
    let module_count = data.modules.as_array().map(|a| a.len()).unwrap_or(0);
    let upgrade_count: usize = data
        .modules
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|m| {
                    m.get("upgrades")
                        .and_then(|u| u.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);
    println!("== Scraper summary ==");
    println!("  data version : {}", data.data_version);
    println!("  modules      : {}", module_count);
    println!("  upgrades     : {}", upgrade_count);
    println!("  vendors      : {}", data.vendors.len());
    println!(
        "  tasks        : {}",
        data.vendors.iter().map(|v| v.tasks.len()).sum::<usize>()
    );
    println!("  items        : {}", data.items.len());
    println!("  icons        : {}", icons_written);
    println!("  unparsed obj : {}", tasks.unparsed_objectives.len());
    println!("  output       : {}", args.output.display());

    if !args.keep_temp {
        upstream.cleanup();
    }

    Ok(())
}

/// Read the `modules` value from an existing `data.json`, returning an empty
/// array if the file doesn't exist or can't be parsed. Hideout module data
/// is authored by hand (via the hideout_screenshots skill) and must survive
/// scraper re-runs — we pass it through as opaque JSON to preserve any
/// fields the skill has added that aren't in our typed schema.
fn read_existing_modules(path: &Path) -> serde_json::Value {
    let empty = serde_json::Value::Array(Vec::new());
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => {
            tracing::info!(path = %path.display(), "no existing data.json — modules will be empty");
            return empty;
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "could not parse existing data.json; modules will be empty");
            return empty;
        }
    };
    let modules = parsed
        .get("modules")
        .cloned()
        .unwrap_or_else(|| empty.clone());
    let count = modules.as_array().map(|a| a.len()).unwrap_or(0);
    tracing::info!(count, "carried over hideout modules from existing data.json");
    modules
}

fn init_logging(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("scraper={level},warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
