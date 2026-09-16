use std::{
    collections::{BTreeSet, HashMap},
    fs::create_dir_all,
    io::stdout,
    path::PathBuf,
    sync::Arc,
    thread::available_parallelism,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chariot_config::eval_config;
use chariot_core::{
    CoreContext, HOST_ARCH, cache::Cache, config::package::PackagePlatform, dependencies::resolve_repos_for_pkg, xbps::package_install,
};
use chariot_rootfs::{CachedPkgSet, DEFAULT_MANIFESTS_URL, ManifestFetchSpec, RootFS};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn};

use crate::util::ProgressBarWriter;

#[derive(Parser)]
#[command(version, next_line_help = true)]
struct ChariotOptions {
    #[arg(long, help = "path to chariot base config", default_value = "chariot_config.toml")]
    config: String,

    #[arg(long, help = "target architecture")]
    arch: String,

    #[arg(long, help = "path to chariot cache", default_value = ".chariot-cache")]
    cache: String,

    #[arg(long, help = "path to chariot rootfs", default_value = ".chariot-rootfs")]
    rootfs: String,

    #[command(subcommand)]
    command: MainCommand,
}

#[derive(Subcommand)]
enum MainCommand {
    #[command(about = "install package")]
    Install(InstallOptions),
}

#[derive(Args)]
struct InstallOptions {
    #[arg(long, help = "install a host package (tool)")]
    tool: bool,

    #[arg(long, help = "force reinstallation, even if the package is already installed")]
    force: bool,

    #[arg(required = true, help = "packages to build and install")]
    packages: Vec<String>,

    #[arg(required = true, help = "package install destination")]
    dest: String,
}

pub fn run_cli() -> Result<()> {
    let opts = ChariotOptions::parse();

    let (config, rootfs_config) = eval_config(opts.config, opts.arch).context("Failed to evaluate config")?;

    let rootfs = match RootFS::get(&opts.rootfs).context("Failed to get rootfs")? {
        None => {
            info!("No rootfs found");

            let pb = ProgressBar::no_length()
                .with_style(ProgressStyle::with_template("{elapsed:.yellow.light} | {prefix:.bold} {wide_msg:.dim}")?)
                .with_message("Downloading...")
                .with_prefix(format!("Initializing rootfs `{}`", rootfs_config.version));
            pb.enable_steady_tick(Duration::from_millis(100));

            let mut pb_writer = ProgressBarWriter::init(&pb);

            let rootfs = RootFS::init(
                &opts.rootfs,
                &ManifestFetchSpec {
                    url: rootfs_config.url.unwrap_or(String::from(DEFAULT_MANIFESTS_URL)),
                    version: rootfs_config.version,
                    hash: rootfs_config.hash,
                },
                &mut pb_writer,
            )
            .context("Failed to initialize rootfs")?;

            pb.finish_and_clear();

            info!("Successfully initialized the rootfs");
            rootfs
        }
        Some(rootfs) => {
            let hash = &rootfs.get_manifest_spec().hash;
            let version = &rootfs.get_manifest_spec().version;

            let version_match = version == &rootfs_config.version;
            let hash_match = hash == &rootfs_config.hash;

            if !version_match && !hash_match {
                bail!(
                    "Rootfs version mismatch (current `{}`, wanted `{}). Delete current rootfs at convenience",
                    hash,
                    rootfs_config.hash
                );
            }

            if !version_match {
                warn!("Suspicious rootfs, version mismatch but hashes match")
            }

            if !hash_match {
                bail!("Rootfs hash mismatch, expected `{}`, got `{}`", rootfs_config.hash, hash);
            }

            rootfs
        }
    };

    let cache = Cache::get(opts.cache).context("Failed to get cache")?;

    let rootfs = Arc::new(rootfs);
    let cache = Arc::new(cache);

    let mut binary_to_pkgset: HashMap<&str, Option<Arc<CachedPkgSet>>> = HashMap::new();
    for binary in ["bsdtar", "git", "patch", "sha256sum", "wget"] {
        let Some(pkg) = rootfs.lookup_package_of_binary(binary) else {
            bail!("This rootfs manifest is missing a required package mapping for the `{}` binary", binary);
        };

        let pb = ProgressBar::no_length()
            .with_style(ProgressStyle::with_template("{elapsed:.yellow.light} | {prefix:.bold} {wide_msg:.dim}")?)
            .with_prefix(format!("Fetching {} package set", pkg));
        pb.enable_steady_tick(Duration::from_millis(100));

        let mut pb_writer = ProgressBarWriter::init(&pb);
        binary_to_pkgset.insert(binary, CachedPkgSet::get(&rootfs, None, &BTreeSet::from([pkg.as_str()]), &mut pb_writer)?);

        pb.finish_and_clear();
    }

    let ctx = CoreContext {
        parallelism: available_parallelism()?.get(),
        root_pkgset: None,
        bsdtar_pkgset: binary_to_pkgset.remove("bsdtar").unwrap(),
        git_pkgset: binary_to_pkgset.remove("git").unwrap(),
        patch_pkgset: binary_to_pkgset.remove("patch").unwrap(),
        sha256sum_pkgset: binary_to_pkgset.remove("sha256sum").unwrap(),
        wget_pkgset: binary_to_pkgset.remove("wget").unwrap(),
        cache,
        rootfs,
    };

    match opts.command {
        MainCommand::Install(install_opts) => {
            let mut selected_packages = Vec::new();
            for package in install_opts.packages {
                let selected_package = config.packages.iter().find(|pkg| {
                    pkg.name == package
                        && pkg.platform
                            == match install_opts.tool {
                                true => PackagePlatform::Host,
                                false => PackagePlatform::Target,
                            }
                });

                selected_packages.push(match selected_package {
                    None => bail!(
                        "Could not find a {} with the name `{}`",
                        match install_opts.tool {
                            true => "tool",
                            false => "package",
                        },
                        package
                    ),
                    Some(pkg) => pkg,
                });
            }

            create_dir_all(&install_opts.dest)?;

            for selected_package in selected_packages {
                let entries = resolve_repos_for_pkg(&ctx, &mut stdout(), selected_package)?;

                package_install(
                    &ctx,
                    &selected_package.name,
                    &selected_package.version,
                    selected_package.revision,
                    match selected_package.platform {
                        PackagePlatform::Host => HOST_ARCH,
                        PackagePlatform::Target => &selected_package.global_env.target_arch,
                    },
                    entries.iter().map(|entry| entry.path()).collect(),
                    &PathBuf::from(&install_opts.dest),
                    false,
                    install_opts.force,
                    &mut stdout(),
                )?;
            }
        }
    }

    Ok(())
}
