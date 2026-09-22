use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{OpenOptions, create_dir_all, write},
    io::{self, Read, Seek, SeekFrom, Write, stdout},
    path::{Path, PathBuf},
    sync::Arc,
    thread::available_parallelism,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chariot_config::{DEFAULT_BASE_CONFIG_PATH, DEFAULT_LUA_CONFIG_PATH, base::read_base_config, lua::eval_lua_config};
use chariot_core::{
    CoreContext, DEFAULT_TARGET_PREFIX,
    buildcache::BuildCache,
    collect_all_hashes,
    config::{GlobalEnvironment, package::PackagePlatform},
    dependencies::resolve_repos_for_pkg,
    ledger::Ledger,
    resolve_effective_hashes,
    store::Store,
    workdir::WorkDirectoryParent,
    xbps::package_install,
};
use chariot_rootfs::{CachedPkgSet, DEFAULT_MANIFESTS_URL, ManifestFetchSpec, RootFS};
use chariot_util::{
    fs::{force_rm, make_path},
    lock::{FileLockKind, open_file_locked},
};
use clap::{Args, CommandFactory, Parser, Subcommand, value_parser};
use clap_complete::{Shell, generate};
use dialoguer::Confirm;
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::{cli::support::setup_lua_lsp, config::parse_cli_config, util::ProgressBarWriter};

mod support;

const DEFAULT_CACHE_PATH: &str = ".chariot-cache";
const DEFAULT_ROOTFS_PATH: &str = ".chariot-rootfs";

const CACHE_SUBDIR_STORE: &str = "store";
const CACHE_SUBDIR_BUILD_CACHE: &str = "builddirs";
const CACHE_SUBDIR_WORKDIRS: &str = "workdirs";
const CACHE_SUBDIR_LOCAL_SOURCES: &str = "localsrc";
const CACHE_FILENAME_LEDGER: &str = "ledger.db";
const CACHE_FILENAME_STATE: &str = "state.json";
const CACHE_FILENAME_GITIGNORE: &str = ".gitignore";

const ARG_CACHE_HELP: &str = "path to chariot cache";
const ARG_CACHE_ENV: &str = "CHARIOT_CACHE_PATH";
const ARG_BASECONFIG_HELP: &str = "path to chariot base config";
const ARG_BASECONFIG_ENV: &str = "CHARIOT_BASE_CONFIG_PATH";

#[derive(Parser)]
#[command(version, next_line_help = true)]
struct ChariotOptions {
    #[arg(long, help = "path to local config", default_value = ".chariot.toml")]
    local_config: String,

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

    #[command(about = "store support commands")]
    Store(StoreOptions),

    #[command(about = "ledger support commands")]
    Ledger(LedgerOptions),

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
struct StoreOptions {
    #[arg(long, env = ARG_CACHE_ENV, help = ARG_CACHE_HELP,  default_value = DEFAULT_CACHE_PATH)]
    cache: PathBuf,

    #[arg(long, env = ARG_BASECONFIG_ENV, help = ARG_BASECONFIG_HELP,  default_value = DEFAULT_BASE_CONFIG_PATH)]
    base_config: PathBuf,

    #[command(subcommand)]
    command: StoreCommand,
}

#[derive(Subcommand)]
enum StoreCommand {
    #[command(about = "evaluate configuration for all known profiles and prunes dangling store entries")]
    Prune,

    #[command(about = "deletes all store entries")]
    Purge,
}

#[derive(Args)]
struct LedgerOptions {
    #[arg(long, env = ARG_CACHE_ENV, help = ARG_CACHE_HELP,  default_value = DEFAULT_CACHE_PATH)]
    cache: PathBuf,

    #[command(subcommand)]
    command: LedgerCommand,
}

#[derive(Subcommand)]
enum LedgerCommand {
    #[command(about = "lists all entries in the ledger")]
    List,
}

#[derive(Args)]
struct InstallOptions {
    #[arg(long, env = ARG_CACHE_ENV, help = ARG_CACHE_HELP, default_value = DEFAULT_CACHE_PATH)]
    cache: PathBuf,

    #[arg(long, help = "path to chariot rootfs", default_value = DEFAULT_ROOTFS_PATH)]
    rootfs: PathBuf,

    #[arg(long, env = ARG_BASECONFIG_ENV, help = ARG_BASECONFIG_HELP, default_value = DEFAULT_BASE_CONFIG_PATH)]
    base_config: PathBuf,

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
    cached_hashes: HashMap<usize, HashSet<(String, u128)>>,
}

fn parse_kv(str: &str) -> Result<(String, String), String> {
    let pos = str.find('=').ok_or_else(|| format!("invalid KEY=VALUE: no `=` found in `{}`", str))?;
    Ok((str[..pos].to_string(), str[pos + 1..].to_string()))
}

fn with_state<T>(state_path: &Path, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
    let mut state_file = open_file_locked(
        state_path,
        OpenOptions::new().create(true).write(true).read(true),
        FileLockKind::Exclusive,
    )
    .context("Failed to open state file")?;

    let mut state_data = String::new();
    state_file.read_to_string(&mut state_data).context("Failed to read state file")?;

    let mut state = if state_data.is_empty() {
        State::default()
    } else {
        serde_json::from_str::<State>(&state_data).context("Failed to parse state file")?
    };

    let result = f(&mut state)?;

    let out = serde_json::to_vec(&state).context("Failed to serialize state file")?;
    state_file.seek(SeekFrom::Start(0)).context("Failed to seek state file")?;
    state_file.write_all(&out).context("Failed to write state file")?;
    state_file.set_len(out.len() as u64).context("Failed to truncate state file")?;

    Ok(result)
}

pub fn run_cli() -> Result<()> {
    let opts = ChariotOptions::parse();

    let local_config = parse_cli_config(&opts.local_config).context("Failed to parse local config")?;

    let install_opts = match opts.command {
        MainCommand::Install(install_opts) => install_opts,
        MainCommand::Store(StoreOptions {
            cache: cache_path,
            base_config: base_config_path,
            command: store_command,
        }) => {
            let store = Store::get(cache_path.join(CACHE_SUBDIR_STORE)).context("Failed to get store")?;
            let ledger = Ledger::get(cache_path.join(CACHE_FILENAME_LEDGER)).context("Failed to get ledger")?;

            let local_sources_path = cache_path.join(CACHE_SUBDIR_LOCAL_SOURCES);
            force_rm(&local_sources_path)?;

            match store_command {
                StoreCommand::Prune => {
                    let base_config = read_base_config(&base_config_path).context("Failed to read base config")?;
                    let target_prefix = base_config.target_prefix.unwrap_or(String::from(DEFAULT_TARGET_PREFIX));

                    with_state(&cache_path.join(CACHE_FILENAME_STATE), |state| {
                        for (idx, input_state) in state.known_input_states.iter().enumerate() {
                            let global_environment = Arc::new(GlobalEnvironment {
                                global_environment_variables: BTreeMap::new(),
                                rootfs_manifest_hash: base_config.rootfs.hash.clone(),
                                target_prefix: target_prefix.clone(),
                                target_arch: input_state.arch.clone(),
                            });

                            let lua_config_path = base_config.lua_root.clone().unwrap_or(PathBuf::from(DEFAULT_LUA_CONFIG_PATH));
                            let config = eval_lua_config(&lua_config_path, global_environment, input_state.options.clone(), &local_sources_path)
                                .context("Failed to evaluate lua config")?;

                            state.cached_hashes.insert(
                                idx,
                                collect_all_hashes(&config)
                                    .into_iter()
                                    .map(|(cat, hash)| (cat.to_string(), hash))
                                    .collect(),
                            );
                        }

                        let live_recipe_hashes = state
                            .cached_hashes
                            .iter()
                            .map(|(_, hashes)| hashes.into_iter())
                            .flatten()
                            .map(|(cat, hash)| (cat.as_str(), *hash))
                            .collect::<HashSet<_>>();

                        store.prune_store(resolve_effective_hashes(&ledger, live_recipe_hashes.iter().copied())?)?;
                        ledger.prune(live_recipe_hashes)?;

                        Ok(())
                    })?;
                }
                StoreCommand::Purge => {
                    store.prune_store(HashSet::new()).context("Failed to purge store")?;
                    ledger.prune(HashSet::new()).context("Failed to purge ledger")?;
                }
            }

            return Ok(());
        }
        MainCommand::Ledger(LedgerOptions {
            cache: cache_path,
            command: ledger_command,
        }) => {
            let ledger = Ledger::get(cache_path.join(CACHE_FILENAME_LEDGER)).context("Failed to get ledger")?;

            match ledger_command {
                LedgerCommand::List => {
                    let records = ledger.list().context("Failed to list ledger records")?;
                    let max_category_length = records.iter().map(|(cat, ..)| cat.len()).max().unwrap_or(0).max(8);
                    info!(
                        "{:<cat_width$} {:<32} {:<32}",
                        "category",
                        "input_hash",
                        "effective_hash",
                        cat_width = max_category_length
                    );
                    info!("{}", "-".repeat(max_category_length + 66));
                    for (category, hash, effective_hash) in records {
                        info!(
                            "{:<cat_width$} {:<32x} {:<32x}",
                            category,
                            hash,
                            effective_hash,
                            cat_width = max_category_length
                        );
                    }
                }
            }

            return Ok(());
        }
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

    let cache_path = PathBuf::from(install_opts.cache);
    make_path(&cache_path).context("Failed to create cache directory")?;

    write(cache_path.join(CACHE_FILENAME_GITIGNORE), "# Generated by Chariot\n*").context("Failed to write gitignore")?;

    let local_sources_path = cache_path.join(CACHE_SUBDIR_LOCAL_SOURCES);
    force_rm(&local_sources_path)?;

    let (base_config, config, cached_hashes) = with_state(&cache_path.join(CACHE_FILENAME_STATE), |state| {
        let input_state = InputState {
            arch: install_opts.arch.clone(),
            options: options.clone(),
        };

        let input_state_index = match state.known_input_states.iter().position(|state| state == &input_state) {
            Some(index) => index,
            None => {
                let ok = Confirm::new()
                    .default(true)
                    .with_prompt("Detected a new profile (profile describes a specific permutation of architecture and options). Proceed?")
                    .interact()?;

                if !ok {
                    bail!("Canceled by user");
                }

                let len = state.known_input_states.len();
                state.known_input_states.push(input_state);
                len
            }
        };

        let base_config = read_base_config(&install_opts.base_config).context("Failed to get base config")?;
        let target_prefix = base_config.target_prefix.clone().unwrap_or(String::from(DEFAULT_TARGET_PREFIX));

        let global_environment = Arc::new(GlobalEnvironment {
            global_environment_variables: BTreeMap::new(),
            rootfs_manifest_hash: base_config.rootfs.hash.clone(),
            target_prefix: target_prefix.clone(),
            target_arch: install_opts.arch,
        });

        let lua_config_path = base_config.lua_root.clone().unwrap_or(PathBuf::from(DEFAULT_LUA_CONFIG_PATH));
        let config = eval_lua_config(&lua_config_path, global_environment, options, local_sources_path).context("Failed to evaluate lua config")?;

        state.cached_hashes.insert(
            input_state_index,
            collect_all_hashes(&config)
                .into_iter()
                .map(|(cat, hash)| (cat.to_string(), hash))
                .collect(),
        );

        Ok((
            base_config,
            config,
            state
                .cached_hashes
                .clone()
                .into_iter()
                .map(|(_, hashes)| hashes.into_iter())
                .flatten()
                .collect::<Vec<_>>(),
        ))
    })?;

    let rootfs = Arc::new(match RootFS::get(&install_opts.rootfs).context("Failed to get rootfs")? {
        None => {
            info!("No rootfs found");

            let pb = ProgressBar::no_length()
                .with_style(ProgressStyle::with_template("{elapsed:.yellow.light} | {prefix:.bold} {wide_msg:.dim}")?)
                .with_message("Downloading...")
                .with_prefix(format!("Initializing rootfs `{}`", base_config.rootfs.version));
            pb.enable_steady_tick(Duration::from_millis(100));

            let mut pb_writer = ProgressBarWriter::init(&pb);

            let rootfs = RootFS::init(
                &install_opts.rootfs,
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
    });

    let store = Arc::new(Store::get(cache_path.join(CACHE_SUBDIR_STORE)).context("Failed to get store")?);
    let ledger = Arc::new(Ledger::get(cache_path.join(CACHE_FILENAME_LEDGER)).context("Failed to get ledger")?);
    let workdir_parent = Arc::new(WorkDirectoryParent::get(cache_path.join(CACHE_SUBDIR_WORKDIRS)).context("Failed to get workdirs")?);
    let build_cache = Arc::new(BuildCache::get(cache_path.join(CACHE_SUBDIR_BUILD_CACHE)).context("Failed to get build cache")?);

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

    let mut build_cache_enabled = HashSet::new();
    for (platform, name, pkg) in local_config
        .pkgs
        .iter()
        .map(|(name, pkg)| (PackagePlatform::Target, name, pkg))
        .chain(local_config.tools.iter().map(|(name, tool)| (PackagePlatform::Host, name, tool)))
    {
        if pkg.enable_build_cache {
            build_cache_enabled.insert((platform, name.clone()));
        }
    }

    let ctx = CoreContext {
        build_cache_enabled,
        parallelism: available_parallelism()?.get(),
        root_pkgset: None,
        bsdtar_pkgset: binary_to_pkgset.remove("bsdtar").unwrap(),
        git_pkgset: binary_to_pkgset.remove("git").unwrap(),
        patch_pkgset: binary_to_pkgset.remove("patch").unwrap(),
        sha256sum_pkgset: binary_to_pkgset.remove("sha256sum").unwrap(),
        wget_pkgset: binary_to_pkgset.remove("wget").unwrap(),
        store: store.clone(),
        ledger,
        workdir_parent,
        build_cache,
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

    let live_recipe_hashes = cached_hashes.iter().map(|(cat, hash)| (cat.as_str(), *hash)).collect::<HashSet<_>>();

    store.prune_store(resolve_effective_hashes(&ctx.ledger, live_recipe_hashes.iter().copied())?)?;
    ctx.ledger.prune(live_recipe_hashes)?;
    ctx.build_cache.prune(ctx.build_cache_enabled)?;

    Ok(())
}
