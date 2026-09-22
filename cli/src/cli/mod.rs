use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{OpenOptions, write},
    io::{self, Read, Seek, SeekFrom, Write, stderr, stdout},
    path::{Path, PathBuf},
    sync::Arc,
    thread::available_parallelism,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chariot_config::{DEFAULT_BASE_CONFIG_PATH, DEFAULT_LUA_CONFIG_PATH, base::read_base_config, lua::eval_lua_config};
use chariot_core::{
    CoreContext, DEFAULT_TARGET_PREFIX,
    buildcache::{BuildCache, BuildDirectory},
    collect_all_hashes,
    config::{
        Config, GlobalEnvironment,
        package::PackagePlatform,
        script::{Script, ScriptLanguage},
    },
    execenv::ExecEnv,
    ledger::Ledger,
    package::resolve_package_runtime_dependencies,
    resolve_effective_hashes,
    store::Store,
    workdir::WorkDirectoryParent,
    xbps::package_install,
};
use chariot_rootfs::{CachedPkgSet, DEFAULT_MANIFESTS_URL, ManifestFetchSpec, PkgSetState, RootFS, StderrTarget};
use chariot_runtime::{Mount, MountKind};
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

use crate::{
    cli::support::setup_lua_lsp,
    config::{CliConfig, parse_cli_config},
    util::ProgressBarWriter,
};

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
const ARG_ROOTFS_HELP: &str = "path to chariot rootfs";
const ARG_ROOTFS_ENV: &str = "CHARIOT_ROOTFS_PATH";
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

    #[command(about = "cache support commands")]
    Cache(CacheOptions),

    #[command(about = "rootfs support commands")]
    Rootfs(RootFSOptions),

    #[command(about = "execute a command inside provided environment")]
    Exec(ExecOptions),

    #[command(about = "install package")]
    Install(InstallOptions),

    #[command(about = "garbage collect")]
    Gc(GarbageCollectionOptions),
}

#[derive(Args)]
struct CacheOptions {
    #[arg(long, env = ARG_CACHE_ENV, help = ARG_CACHE_HELP,  default_value = DEFAULT_CACHE_PATH)]
    cache: PathBuf,

    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Subcommand)]
enum CacheCommand {
    #[command(about = "deletes all store and ledger entries")]
    Purge,

    #[command(about = "lists all entries in the ledger")]
    ListLedger,
}

#[derive(Args)]
struct RootFSOptions {
    #[arg(long, env = ARG_ROOTFS_ENV, help = ARG_ROOTFS_HELP, default_value = DEFAULT_ROOTFS_PATH)]
    rootfs: PathBuf,

    #[command(subcommand)]
    command: RootFSCommand,
}

#[derive(Subcommand)]
enum RootFSCommand {
    Init {
        #[arg(long, default_value = DEFAULT_MANIFESTS_URL)]
        url: String,
        version: String,
        hash: String,
    },
    Status,
    #[command(subcommand)]
    Pkgset(PkgsetCommand),
    Purge,
}

#[derive(Subcommand)]
enum PkgsetCommand {
    List,
    Cache { packages: Vec<String> },
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
struct CommonBuildOptions {
    #[arg(long, env = ARG_CACHE_ENV, help = ARG_CACHE_HELP, default_value = DEFAULT_CACHE_PATH)]
    cache: PathBuf,

    #[arg(long, env = ARG_ROOTFS_ENV, help = ARG_ROOTFS_HELP, default_value = DEFAULT_ROOTFS_PATH)]
    rootfs: PathBuf,

    #[arg(long, env = ARG_BASECONFIG_ENV, help = ARG_BASECONFIG_HELP, default_value = DEFAULT_BASE_CONFIG_PATH)]
    base_config: PathBuf,

    #[arg(long, env = "CHARIOT_ARCH", help = "target architecture")]
    arch: String,

    #[arg(long, env = "CHARIOT_OPTIONS", help = "user defined options", value_parser = parse_kv, value_delimiter = ',')]
    options: Vec<(String, String)>,

    #[arg(long, help = "allow creation of new profiles without user input")]
    allow_new_profiles: bool,
}

#[derive(Args)]
struct ExecOptions {
    #[command(flatten)]
    common_build_opts: CommonBuildOptions,

    #[arg(long, help = "make execution environment reflect the build environment of a package")]
    build_env: Option<String>,

    #[arg(short, long, help = "mount package build directory into execution environment", value_name = "DEST_PATH=PACKAGE_NAME", value_parser = parse_kv)]
    build_dir: Vec<(String, String)>,

    #[arg(
        short = 'p',
        long,
        help = "native packages to install into the execution environment",
        value_delimiter = ','
    )]
    native_pkg: Vec<String>,

    #[arg(long, help = "host packages to install into the execution environment", value_delimiter = ',')]
    tool: Vec<String>,

    #[arg(long, help = "target packages to install into the sysroot", value_delimiter = ',')]
    pkg: Vec<String>,

    #[arg(long, help = "current working directory", default_value = "/")]
    cwd: String,

    #[arg(short, long, help = "environment variable(s) to pass into the execution environment", value_parser = parse_kv, value_delimiter = ',')]
    env_var: Vec<(String, String)>,

    #[arg(short, long, help = "bind mount(s) into the execution environment", value_parser = parse_mount)]
    mount: Vec<(String, String, bool, bool)>,

    #[arg(long, help = "script language", default_value = "bash", value_parser = parse_language)]
    language: ScriptLanguage,

    #[arg(long, help = "forward stdin into execution environment")]
    stdin: bool,

    #[arg(long, help = "forward stdout from execution environment")]
    no_stdout: bool,

    #[arg(long, help = "forward stderr from execution environment")]
    no_stderr: bool,

    #[arg(help = "script to execute")]
    command: String,
}

#[derive(Args)]
struct InstallOptions {
    #[command(flatten)]
    common_build_opts: CommonBuildOptions,

    #[arg(long, help = "install a host package (tool)")]
    tool: bool,

    #[arg(long, help = "force installation, even if the package is already installed")]
    force: bool,

    #[arg(required = true, help = "packages to build and install")]
    packages: Vec<String>,

    #[arg(required = true, help = "package install destination")]
    dest: String,
}

#[derive(Args)]
struct GarbageCollectionOptions {
    #[arg(long, env = ARG_CACHE_ENV, help = ARG_CACHE_HELP,  default_value = DEFAULT_CACHE_PATH)]
    cache: PathBuf,

    #[arg(long, env = ARG_BASECONFIG_ENV, help = ARG_BASECONFIG_HELP,  default_value = DEFAULT_BASE_CONFIG_PATH)]
    base_config: PathBuf,
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

fn parse_language(str: &str) -> Result<ScriptLanguage, String> {
    match str {
        "bash" | "sh" => Ok(ScriptLanguage::Bash),
        "python" | "py" => Ok(ScriptLanguage::Python),
        str => Err(format!("unknown script language `{}`", str)),
    }
}

fn parse_mount(str: &str) -> Result<(String, String, bool, bool), String> {
    let (mount, opts) = match str.split_once(":") {
        Some((mount, opts)) => (mount, opts.split(":").collect::<Vec<_>>()),
        None => (str, Vec::new()),
    };

    let mut is_read_only = false;
    let mut is_file = false;
    for opt in opts {
        match opt {
            "ro" => is_read_only = true,
            "file" => is_file = true,
            _ => continue,
        }
    }

    match mount.split_once("=") {
        None => Err(format!("`{}` is not a valid mount", str)),
        Some((from, to)) => Ok((from.to_string(), to.to_string(), is_read_only, is_file)),
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for &u in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = u;
    }
    if unit == "B" { format!("{bytes}B") } else { format!("{value:.1}{unit}") }
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

fn progress_bar() -> ProgressBar {
    let pb = ProgressBar::no_length().with_style(ProgressStyle::with_template("{elapsed:.yellow.light} | {prefix:.bold} {wide_msg:.dim}").unwrap());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn build_prepare(build_opts: CommonBuildOptions, local_config: &CliConfig) -> Result<(CoreContext, Config, HashSet<(String, u128)>)> {
    let options = HashMap::from_iter(build_opts.options);

    let cache_path = PathBuf::from(build_opts.cache);
    make_path(&cache_path).context("Failed to create cache directory")?;

    write(cache_path.join(CACHE_FILENAME_GITIGNORE), "# Generated by Chariot\n*").context("Failed to write gitignore")?;

    let local_sources_path = cache_path.join(CACHE_SUBDIR_LOCAL_SOURCES);
    force_rm(&local_sources_path)?;

    let (base_config, config, cached_hashes) = with_state(&cache_path.join(CACHE_FILENAME_STATE), |state| {
        let input_state = InputState {
            arch: build_opts.arch.clone(),
            options: options.clone(),
        };

        let input_state_index = match state.known_input_states.iter().position(|state| state == &input_state) {
            Some(index) => index,
            None => {
                let ok = build_opts.allow_new_profiles
                    || Confirm::new()
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

        let base_config_path = build_opts
            .base_config
            .canonicalize()
            .context("Failed to canonicalize (find absolute path of) base config")?;

        let config_dir = base_config_path.parent().context("Failed to resolve directory of base config")?;

        let base_config = read_base_config(&base_config_path).context("Failed to get base config")?;
        let target_prefix = base_config.target_prefix.clone().unwrap_or(String::from(DEFAULT_TARGET_PREFIX));

        let global_environment = Arc::new(GlobalEnvironment {
            global_environment_variables: BTreeMap::new(),
            rootfs_manifest_hash: base_config.rootfs.hash.clone(),
            target_prefix: target_prefix.clone(),
            target_arch: build_opts.arch,
        });

        let lua_config_path = config_dir.join(base_config.lua_root.clone().unwrap_or(PathBuf::from(DEFAULT_LUA_CONFIG_PATH)));
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

    let rootfs = Arc::new(match RootFS::get(&&build_opts.rootfs).context("Failed to get rootfs")? {
        None => {
            info!("No rootfs found");

            let pb = progress_bar()
                .with_message("Downloading...")
                .with_prefix(format!("Initializing rootfs `{}`", base_config.rootfs.version));

            let mut pb_writer = ProgressBarWriter::init(&pb);

            let rootfs = RootFS::init(
                &&build_opts.rootfs,
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

        let pb = progress_bar().with_prefix(format!("Fetching {} package set", pkg));

        let mut pb_writer = ProgressBarWriter::init(&pb);
        binary_to_pkgset.insert(binary, CachedPkgSet::get(&rootfs, &None, &BTreeSet::from([pkg]), &mut pb_writer)?);

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

    Ok((
        ctx,
        config,
        cached_hashes.into_iter().map(|(cat, hash)| (cat, hash)).collect::<HashSet<_>>(),
    ))
}

pub fn run_cli() -> Result<()> {
    let opts = ChariotOptions::parse();

    let local_config = parse_cli_config(&opts.local_config).context("Failed to parse local config")?;

    match opts.command {
        MainCommand::Install(install_opts) => {
            let (ctx, config, cached_hashes) = build_prepare(install_opts.common_build_opts, &local_config)?;

            let mut selected_packages = Vec::new();
            for name in install_opts.packages {
                let platform = match install_opts.tool {
                    true => PackagePlatform::Host,
                    false => PackagePlatform::Target,
                };

                let selected_package = config.packages.iter().find(|pkg| pkg.name == name && pkg.platform == platform);

                selected_packages.push(match selected_package {
                    None => bail!("Could not find a {} package with the name `{}`", platform.to_string(), name),
                    Some(pkg) => pkg,
                });
            }

            make_path(&install_opts.dest)?;

            for selected_package in selected_packages {
                let entries = resolve_package_runtime_dependencies(&ctx, &mut stdout(), selected_package)?;

                package_install(
                    &ctx,
                    None,
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

            ctx.store.prune_store(resolve_effective_hashes(
                &ctx.ledger,
                cached_hashes.iter().map(|(cat, hash)| (cat.as_str(), *hash)),
            )?)?;
            ctx.ledger
                .prune(cached_hashes.iter().map(|(cat, hash)| (cat.as_str(), *hash)).collect())?;
            ctx.build_cache.prune(ctx.build_cache_enabled)?;
        }
        MainCommand::Exec(exec_options) => {
            let (ctx, config, _) = build_prepare(exec_options.common_build_opts, &local_config)?;

            let mut packages = exec_options
                .pkg
                .iter()
                .map(|name| {
                    let pkg = match config
                        .packages
                        .iter()
                        .find(|pkg| pkg.platform == PackagePlatform::Target && &pkg.name == name)
                    {
                        Some(pkg) => pkg,
                        None => bail!("Could not find package `{}`", name),
                    };

                    Ok(pkg)
                })
                .collect::<Result<Vec<_>, _>>()?;

            let mut tools = exec_options
                .pkg
                .iter()
                .map(|name| {
                    let tool = match config
                        .packages
                        .iter()
                        .find(|pkg| pkg.platform == PackagePlatform::Host && &pkg.name == name)
                    {
                        Some(tool) => tool,
                        None => bail!("Could not find tool `{}`", name),
                    };

                    Ok(tool)
                })
                .collect::<Result<Vec<_>, _>>()?;

            let (pkgset, pkg) = if let Some(build_env_pkg) = exec_options.build_env {
                let pkg = config
                    .packages
                    .iter()
                    .find(|pkg| pkg.platform == PackagePlatform::Target && pkg.name == build_env_pkg);

                let pkg = match pkg {
                    None => bail!("No build_env package `{}` found", build_env_pkg),
                    Some(pkg) => pkg,
                };

                let pkgset = CachedPkgSet::get(
                    &ctx.rootfs,
                    &None,
                    &BTreeSet::from_iter(exec_options.native_pkg.iter().chain(&exec_options.native_pkg)),
                    &mut stderr(),
                )?;

                for pkg in &pkg.dependencies.packages {
                    packages.push(pkg);
                }

                for tool in &pkg.dependencies.tools {
                    tools.push(tool);
                }

                (pkgset, Some(pkg))
            } else {
                let pkgset = CachedPkgSet::get(&ctx.rootfs, &None, &BTreeSet::from_iter(exec_options.native_pkg.iter()), &mut stderr())?;

                (pkgset, None)
            };

            let sources = match pkg {
                Some(pkg) => &pkg.dependencies.sources,
                None => &BTreeMap::new(),
            };

            let exec_env = ExecEnv::create(
                &ctx,
                &mut stderr(),
                pkgset,
                sources,
                &packages.into_iter().cloned().collect(),
                &tools.into_iter().cloned().collect(),
            )?;

            let mut mounts = exec_options
                .mount
                .into_iter()
                .map(|(from, to, read_only, is_file)| Mount {
                    dest: PathBuf::from(to),
                    kind: MountKind::Bind {
                        from: PathBuf::from(from),
                        read_only,
                        is_file,
                    },
                })
                .collect::<Vec<_>>();

            let mut _build_directories = Vec::new();
            for (dest, pkg_name) in exec_options.build_dir {
                let build_dir = BuildDirectory::get_read_only(&ctx.build_cache, PackagePlatform::Target, &config.global_env.target_arch, &pkg_name)
                    .with_context(|| format!("Failed to get build directory for package `{}`", pkg_name))?;

                mounts.push(Mount {
                    dest: PathBuf::from(dest),
                    kind: MountKind::Bind {
                        from: build_dir.path(),
                        read_only: true,
                        is_file: false,
                    },
                });

                _build_directories.push(build_dir);
            }

            let script = Script::new(exec_options.language, exec_options.command);

            let mut stderr_handle = stderr();
            let mut stdout_handle = stdout();
            exec_env.exec(
                exec_options.cwd,
                mounts.iter().collect(),
                &exec_options.env_var.iter().map(|(var, value)| (var.as_str(), value.as_str())).collect(),
                exec_options.stdin,
                match exec_options.no_stdout {
                    true => None,
                    false => Some(&mut stdout_handle),
                },
                match exec_options.no_stderr {
                    true => StderrTarget::Discard,
                    false => StderrTarget::Capture(&mut stderr_handle),
                },
                script.command(),
            )?;
        }
        MainCommand::Gc(GarbageCollectionOptions {
            cache: cache_path,
            base_config: base_config_path,
        }) => {
            let store = Store::get(cache_path.join(CACHE_SUBDIR_STORE)).context("Failed to get store")?;
            let ledger = Ledger::get(cache_path.join(CACHE_FILENAME_LEDGER)).context("Failed to get ledger")?;

            let local_sources_path = cache_path.join(CACHE_SUBDIR_LOCAL_SOURCES);
            force_rm(&local_sources_path)?;

            let base_config_path = base_config_path
                .canonicalize()
                .context("Failed to canonicalize (find absolute path of) base config")?;

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
        MainCommand::Cache(CacheOptions { cache: cache_path, command }) => match command {
            CacheCommand::Purge => {
                let store = Store::get(cache_path.join(CACHE_SUBDIR_STORE)).context("Failed to get store")?;
                let ledger = Ledger::get(cache_path.join(CACHE_FILENAME_LEDGER)).context("Failed to get ledger")?;

                store.prune_store(HashSet::new()).context("Failed to purge store")?;
                ledger.prune(HashSet::new()).context("Failed to purge ledger")?;
            }
            CacheCommand::ListLedger => {
                let ledger = Ledger::get(cache_path.join(CACHE_FILENAME_LEDGER)).context("Failed to get ledger")?;
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
        },
        MainCommand::Rootfs(RootFSOptions {
            rootfs: rootfs_path,
            command,
        }) => {
            if let RootFSCommand::Init { url, version, hash } = command {
                let spec = ManifestFetchSpec { url, version, hash };
                RootFS::init(&rootfs_path, &spec, &mut stdout())?;
                info!("rootfs initialized at {}", rootfs_path.display());
                return Ok(());
            }

            let rootfs = match RootFS::get(&rootfs_path)? {
                Some(r) => r,
                None => bail!("no intact rootfs found at {}", rootfs_path.display()),
            };

            let rootfs = Arc::new(rootfs);

            match command {
                RootFSCommand::Init { .. } => {}
                RootFSCommand::Status => {
                    let spec = rootfs.get_manifest_spec();
                    info!("path:    {}", rootfs_path.display());
                    info!("url:     {}", spec.url);
                    info!("version: {}", spec.version);
                    info!("hash:    {}", spec.hash);
                }
                RootFSCommand::Pkgset(PkgsetCommand::List) => {
                    let pkgsets = rootfs.list_pkgsets()?;
                    if pkgsets.is_empty() {
                        info!("no cached package sets");
                        return Ok(());
                    }
                    info!("{:<6} {:<12} {:<6} {:<6} {:<12} packages", "id", "state", "base", "depth", "size");
                    info!("{}", "-".repeat(60));
                    for ps in pkgsets {
                        let state = match ps.state {
                            PkgSetState::Unknown => "unknown",
                            PkgSetState::Cached => "cached",
                            PkgSetState::Deduplicated => "deduped",
                        };
                        info!(
                            "{:<6} {:<12} {:<6} {:<6} {:<12} {}",
                            ps.id,
                            state,
                            ps.base.map(|id| id.to_string()).unwrap_or(String::new()),
                            ps.base_depth,
                            format_size(ps.size),
                            ps.packages.iter().map(|str| str.as_ref()).collect::<Vec<_>>().join(", "),
                        );
                    }
                }
                RootFSCommand::Pkgset(PkgsetCommand::Cache { packages }) => {
                    CachedPkgSet::get(&rootfs, &None, &BTreeSet::from_iter(packages.iter()), &mut stdout())
                        .context("failed to get cached package set")?;
                }
                RootFSCommand::Purge => {
                    let (total, removed, in_use) = rootfs.prune_pkgsets(|_| true)?;
                    info!("purged {removed}/{total} package sets ({in_use} in use, skipped)");
                }
            }
        }
        MainCommand::Support { command } => match command {
            SupportCommand::SetupLSP => setup_lua_lsp()?,
            SupportCommand::Completions { shell } => generate(shell, &mut ChariotOptions::command(), "chariot".to_string(), &mut io::stdout()),
        },
    };

    Ok(())
}
