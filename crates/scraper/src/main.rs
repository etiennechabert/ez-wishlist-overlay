//! ExfilZone Assistant → ez-wishlist-overlay data pipeline.
//!
//! Run from the workspace root, e.g.:
//!
//! ```text
//! cargo run -p scraper -- --output crates/app/src/assets
//! ```

mod hideout;
mod items;
mod model;
mod output;
mod parse_ts;
mod tasks;
mod upstream;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;
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
    let hideout_ts = upstream
        .root
        .join("src")
        .join("data")
        .join("hideout-upgrades.ts");
    let tasks_ts = upstream.root.join("src").join("data").join("tasks.ts");

    let catalog = items::ItemCatalog::from_upstream(&public_data)?;
    let hideout = hideout::parse(&hideout_ts).context("hideout parse")?;
    let tasks = tasks::parse(&tasks_ts, &catalog).context("tasks parse")?;

    // Union of every item referenced by any kept upgrade or task. Used only
    // for coverage diagnostics — `data.json` ships every item in the catalog
    // so the Items DB view sees the full set.
    let mut referenced: HashSet<String> = HashSet::new();
    referenced.extend(hideout.referenced_items.iter().cloned());
    referenced.extend(tasks.referenced_items.iter().cloned());

    let items_out = catalog.build_output_items(output::ICON_DIR_NAME);
    let coverage = referenced
        .iter()
        .filter(|id| catalog.items.contains_key(*id))
        .count();
    let unknown = referenced.len() - coverage;
    if unknown > 0 {
        tracing::warn!(unknown, "referenced item IDs not present in any catalog");
    }
    // Icons are copied for every item we emit, so the Items DB view has
    // pictures next to weapons/armor/etc. — not just the wishlist subset.
    let all_item_ids: HashSet<String> = catalog.items.keys().cloned().collect();

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
        modules: hideout.modules,
        vendors: tasks.vendors,
        items: items_out,
    };

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("creating {}", args.output.display()))?;
    output::write_data_json(&args.output, &data)?;
    output::write_source_md(&args.output, &data, tasks.unparsed_objectives.len())?;

    let icons_written = if !args.skip_icons {
        let outcome = output::copy_icons(&args.output, &upstream.root, &catalog, &all_item_ids)?;
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
    println!("== Scraper summary ==");
    println!("  data version : {}", data.data_version);
    println!("  modules      : {}", data.modules.len());
    println!(
        "  upgrades     : {}",
        data.modules.iter().map(|m| m.upgrades.len()).sum::<usize>()
    );
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

fn init_logging(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("scraper={level},warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
