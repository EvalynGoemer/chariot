use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{create_dir_all, exists, read_to_string, write},
    io::{self, stdout},
    path::{Path, PathBuf},
    sync::Arc,
    thread::available_parallelism,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chariot_config::{DEFAULT_BASE_CONFIG_PATH, DEFAULT_LUA_CONFIG_PATH, base::read_base_config, lua::eval_lua_config};
use chariot_core::{
    CoreContext, DEFAULT_TARGET_PREFIX, collect_all_hashes,
    config::{GlobalEnvironment, package::PackagePlatform},
    dependencies::resolve_repos_for_pkg,
    store::Store,
    workdir::WorkDirectoryParent,
    xbps::package_install,
};
use chariot_rootfs::{CachedPkgSet, DEFAULT_MANIFESTS_URL, ManifestFetchSpec, RootFS};
use chariot_util::fs::make_path;
use clap::{Args, CommandFactory, Parser, Subcommand, value_parser};
use clap_complete::{Shell, generate};
use dialoguer::Confirm;
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::{cli::support::setup_lua_lsp, util::ProgressBarWriter};

mod support;

const SUBDIR_STORE: &str = "store";
const SUBDIR_WORKDIRS: &str = "workdirs";
const FILENAME_STATE: &str = "state.toml";

#[derive(Parser)]
#[command(version, next_line_help = true)]
struct ChariotOptions {
    #[arg(long, help = "path to chariot base config", default_value = DEFAULT_BASE_CONFIG_PATH)]
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

#[derive(Serialize, Deserialize, PartialEq)]
struct InputState {
    arch: String,
    options: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    known_input_states: Vec<InputState>,
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

    let options = HashMap::from_iter(install_opts.options);

    let cache_path = PathBuf::from(opts.cache);
    make_path(&cache_path).context("Failed to create cache directory")?;

    let state_path = cache_path.join(FILENAME_STATE);
    let mut state = if exists(&state_path)? {
        let state_data = read_to_string(&state_path).context("Failed to read state file")?;
        toml::from_str::<State>(&state_data).context("Failed to parse state file")?
    } else {
        State::default()
    };

    let input_permutation = InputState {
        arch: install_opts.arch.clone(),
        options: options.clone(),
    };

    if !state.known_input_states.contains(&input_permutation) {
        let ok = Confirm::new()
            .default(true)
            .with_prompt("Detected a new architecture, option (key or value), or permutation of these. Proceed?")
            .interact()?;

        if !ok {
            bail!("Canceled by user");
        }

        state.known_input_states.push(input_permutation);

        let state_data = toml::to_string(&state).context("Failed to serialize state file")?;
        write(&state_path, state_data).context("Failed to write state file")?;
    }

    let base_config = read_base_config(&opts.config).context("Failed to get base config")?;
    let target_prefix = base_config.target_prefix.unwrap_or(String::from(DEFAULT_TARGET_PREFIX));

    let global_environment = Arc::new(GlobalEnvironment {
        global_environment_variables: BTreeMap::new(),
        rootfs_manifest_hash: base_config.rootfs.hash.clone(),
        target_prefix: target_prefix.clone(),
        target_arch: install_opts.arch,
    });

    let config = eval_lua_config(
        base_config.lua_root.unwrap_or(PathBuf::from(DEFAULT_LUA_CONFIG_PATH)),
        global_environment,
        options,
    )
    .context("Failed to evaluate lua config")?;

    let rootfs = match RootFS::get(&opts.rootfs).context("Failed to get rootfs")? {
        None => {
            info!("No rootfs found");

            let pb = ProgressBar::no_length()
                .with_style(ProgressStyle::with_template("{elapsed:.yellow.light} | {prefix:.bold} {wide_msg:.dim}")?)
                .with_message("Downloading...")
                .with_prefix(format!("Initializing rootfs `{}`", base_config.rootfs.version));
            pb.enable_steady_tick(Duration::from_millis(100));

            let mut pb_writer = ProgressBarWriter::init(&pb);

            let rootfs = RootFS::init(
                &opts.rootfs,
                &ManifestFetchSpec {
                    url: base_config.rootfs.url.unwrap_or(String::from(DEFAULT_MANIFESTS_URL)),
                    version: base_config.rootfs.version,
                    hash: base_config.rootfs.hash.clone(),
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

            let version_match = version == &base_config.rootfs.version;
            let hash_match = hash == &base_config.rootfs.hash;

            if !version_match && !hash_match {
                bail!(
                    "Rootfs version mismatch (current `{}`, wanted `{}). Delete current rootfs at convenience",
                    hash,
                    base_config.rootfs.hash
                );
            }

            if !version_match {
                warn!("Suspicious rootfs, version mismatch but hashes match")
            }

            if !hash_match {
                bail!("Rootfs hash mismatch, expected `{}`, got `{}`", base_config.rootfs.hash, hash);
            }

            rootfs
        }
    };

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
        store: store.clone(),
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
            selected_package.get_arch(),
            entries.iter().map(|entry| entry.path()).collect(),
            &PathBuf::from(&install_opts.dest),
            false,
            install_opts.force,
            &mut stdout(),
        )?;
    }

    prune_store(&store, state, base_config.rootfs.hash, target_prefix, &opts.config)?;

    Ok(())
}

fn prune_store(store: &Arc<Store>, state: State, rootfs_manifest_hash: String, target_prefix: String, config_path: impl AsRef<Path>) -> Result<()> {
    let mut all_hashes = HashSet::new();
    for input_state in state.known_input_states {
        let config = eval_lua_config(
            &config_path,
            Arc::new(GlobalEnvironment {
                global_environment_variables: BTreeMap::new(),
                rootfs_manifest_hash: rootfs_manifest_hash.clone(),
                target_arch: input_state.arch,
                target_prefix: target_prefix.clone(),
            }),
            input_state.options,
        )
        .context("Failed to evaluate config")?;
        let hashes = collect_all_hashes(&config);
        all_hashes.extend(hashes.iter());
    }

    store.prune_store(all_hashes).context("Failed to prune store")?;

    Ok(())
}
