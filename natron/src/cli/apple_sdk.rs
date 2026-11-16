//! `natron apple-sdk` — find macOS SDKs on Apple's CDN and print the
//! `natron.toml` options that pin one.

use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

use crate::cache::Cache;
use crate::providers::apple_sdk;

#[derive(Debug, Args)]
pub struct AppleSdkArgs {
    #[command(subcommand)]
    pub verb: AppleSdkVerb,
}

#[derive(Debug, Subcommand)]
pub enum AppleSdkVerb {
    /// List the SDK packages in Apple's current software-update catalog,
    /// newest first.
    List(ListArgs),
    /// Download one package and print the `[toolchain.options]` that pin it.
    Pin(PinArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Catalog to read (defaults to Apple's merged catalog).
    #[arg(long)]
    pub catalog: Option<String>,
}

#[derive(Debug, Args)]
pub struct PinArgs {
    /// Package URL (from `natron apple-sdk list`), or a mirror copy.
    pub url: String,
}

pub fn run(_config: &Option<PathBuf>, cache_dir: &Option<PathBuf>, args: AppleSdkArgs) -> Result<()> {
    match args.verb {
        AppleSdkVerb::List(a) => {
            let catalog = a.catalog.unwrap_or_else(|| apple_sdk::DEFAULT_CATALOG.to_string());
            for p in apple_sdk::catalog_packages(&catalog)? {
                // Only the SDK payloads; the tiny *_Remove_* packages are scripts.
                let file = p.url.rsplit('/').next().unwrap_or("");
                if !file.ends_with("_SDK.pkg") || p.size.unwrap_or(0) < 1 << 20 {
                    continue;
                }
                let date = p.post_date.get(..10).unwrap_or(&p.post_date);
                let mb = p.size.unwrap_or(0) as f64 / (1024.0 * 1024.0);
                println!("{date}  {:<10} {mb:>6.1} MB  {}", p.product, p.url);
            }
            println!("\nRun `natron apple-sdk pin <url>` for the options that pin one.");
            Ok(())
        }
        AppleSdkVerb::Pin(a) => {
            let cache = Cache::resolve(cache_dir.as_deref(), None)?;
            cache.ensure_layout()?;
            let pkg = crate::download::fetch(&a.url, None, &cache.downloads)?;
            let sha = apple_sdk::payload_sha256(&pkg)?;
            let sdks = apple_sdk::sdk_names(&pkg)?;
            let package = a.url.rsplit('/').next().unwrap_or("");
            for sdk in &sdks {
                println!("sdk            = \"{sdk}\"");
                println!("package        = \"{package}\"");
                println!("payload_sha256 = \"{sha}\"");
                println!("urls           = [\"{}\"]\n", a.url);
            }
            if sdks.is_empty() {
                println!("{} holds no SDKs/*.sdk directory", a.url);
            }
            Ok(())
        }
    }
}
