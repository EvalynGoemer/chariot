use std::{
    collections::{BTreeSet, HashMap},
    fs::create_dir_all,
    io::{self, stdout},
    path::PathBuf,
    sync::Arc,
    thread::available_parallelism,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chariot_config::eval_config;
use chariot_core::{
    CoreContext, HOST_ARCH, config::package::PackagePlatform, dependencies::resolve_repos_for_pkg, store::Store, workdir::WorkDirectoryParent,
    xbps::package_install,
};
use chariot_rootfs::{CachedPkgSet, DEFAULT_MANIFESTS_URL, ManifestFetchSpec, RootFS};
use clap::{Args, CommandFactory, Parser, Subcommand, value_parser};
use clap_complete::{Shell, generate};
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn};

use crate::{cli::support::setup_lua_lsp, util::ProgressBarWriter};

mod support;

const SUBDIR_STORE: &str = "store";
const SUBDIR_WORKDIRS: &str = "workdirs";

#[derive(Parser)]
#[command(version, next_line_help = true)]
struct ChariotOptions {
    #[arg(long, help = "path to chariot base config", default_value = "chariot_config.toml")]
    config: String,

    #[arg(long, help = "path to chariot cache", default_value = ".chariot-cache")]
    cache: String,

    #[arg(long, help = "path to chariot rootfs", default_value = ".chariot-rootfs")]
    rootfs: String,

    #[command(subcommand)]
    command: MainCommand,
}

#[derive(Subcommand)]
enum MainCommand {
    #[command(about = "miscellaneous support tooling")]
    Support {
        #[command(subcommand)]
        command: SupportCommand,
    },

    #[command(about = "install package")]
    Install(InstallOptions),
}

#[derive(Subcommand)]
enum SupportCommand {
    #[command(about = "generate lua lsp configuration")]
    SetupLSP,

    #[command(about = "generate shell completions for chariot")]
    Completions {
        #[arg(help = "shell to generate completions for", value_parser = value_parser!(Shell))]
        shell: Shell,
    },
}

#[derive(Args)]
struct InstallOptions {
    #[arg(long, env = "CHARIOT_ARCH", help = "target architecture")]
    arch: String,

    #[arg(long, env = "CHARIOT_OPTIONS", help = "options", value_parser = parse_kv, value_delimiter = ',')]
    options: Vec<(String, String)>,

    #[arg(long, help = "install a host package (tool)")]
    tool: bool,

    #[arg(long, help = "force reinstallation, even if the package is already installed")]
    force: bool,

    #[arg(required = true, help = "packages to build and install")]
    packages: Vec<String>,

    #[arg(required = true, help = "package install destination")]
    dest: String,
}

fn parse_kv(str: &str) -> Result<(String, String), String> {
    let pos = str.find('=').ok_or_else(|| format!("invalid KEY=VALUE: no `=` found in `{}`", str))?;
    Ok((str[..pos].to_string(), str[pos + 1..].to_string()))
}

pub fn run_cli() -> Result<()> {
    let opts = ChariotOptions::parse();

    let install_opts = match opts.command {
        MainCommand::Install(install_opts) => install_opts,
        MainCommand::Support {
            command: SupportCommand::SetupLSP,
        } => return setup_lua_lsp(),
        MainCommand::Support {
            command: SupportCommand::Completions { shell },
        } => {
            generate(shell, &mut ChariotOptions::command(), "chariot".to_string(), &mut io::stdout());
            return Ok(());
        }
    };

    let (config, rootfs_config) =
        eval_config(opts.config, install_opts.arch, HashMap::from_iter(install_opts.options)).context("Failed to evaluate config")?;

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

    let cache_path = PathBuf::from(opts.cache);

    let store = Store::get(cache_path.join(SUBDIR_STORE)).context("Failed to get store")?;
    let workdir_parent = WorkDirectoryParent::get(cache_path.join(SUBDIR_WORKDIRS)).context("Failed to get workdirs")?;

    let rootfs = Arc::new(rootfs);
    let store = Arc::new(store);
    let workdir_parent = Arc::new(workdir_parent);

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
        store,
        workdir_parent,
        rootfs,
    };

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

    Ok(())
}
