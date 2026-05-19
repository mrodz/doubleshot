use clap::{Args, Parser, Subcommand};
use nginx_config::ast::{Address, Directive, Item, LocationPattern, Main, ServerName};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BLUE: &str = "blue";
const GREEN: &str = "green";
const DEFAULT_CONFIG: &str = "doubleshot.toml";
const RESET: &str = "\x1b[0m";
const BLUE_FG: &str = "\x1b[34m";
const GREEN_FG: &str = "\x1b[32m";

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CommandKind::Serve(overrides) => {
            let config = AppConfig::load(cli.config.as_deref(), &overrides)?;
            serve(config)
        }
        CommandKind::Deploy {
            artifact,
            overrides,
        } => {
            let config = AppConfig::load(cli.config.as_deref(), &overrides)?;
            config.ensure_dirs()?;
            let mut slots = Slots::default();
            deploy_artifact(&config, &artifact, ArtifactMode::Copy, &mut slots)
        }
        CommandKind::Status(overrides) => {
            let config = AppConfig::load(cli.config.as_deref(), &overrides)?;
            print_status(&config)
        }
        CommandKind::InitConfig {
            output,
            from_nginx,
            nginx_conf,
            server_name,
        } => {
            let (sample, notes) = if from_nginx {
                let proposal = scan_nginx_config(&nginx_conf, server_name.as_deref())?;
                (
                    toml::to_string_pretty(&proposal.config)
                        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?,
                    proposal.notes,
                )
            } else {
                (
                    toml::to_string_pretty(&FileConfig::sample())
                        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?,
                    Vec::new(),
                )
            };

            match output {
                Some(path) => {
                    for note in notes {
                        eprintln!("{note}");
                    }
                    fs::write(path, sample)
                }
                None => {
                    for note in notes {
                        println!("# {note}");
                    }
                    if from_nginx {
                        println!();
                    }
                    print!("{sample}");
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(version, about = "Generic blue-green deployment helper")]
struct Cli {
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Watch the inbox and deploy artifacts as they arrive.
    Serve(CommonOverrides),
    /// Deploy one artifact immediately on this machine.
    Deploy {
        artifact: PathBuf,
        #[command(flatten)]
        overrides: CommonOverrides,
    },
    /// Print current deployment state.
    Status(CommonOverrides),
    /// Print or write a starter TOML config.
    InitConfig {
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        from_nginx: bool,
        #[arg(long, default_value = "/etc/nginx/nginx.conf")]
        nginx_conf: PathBuf,
        #[arg(long)]
        server_name: Option<String>,
    },
}

#[derive(Args, Clone, Debug, Default)]
struct CommonOverrides {
    #[arg(long)]
    home: Option<PathBuf>,
    #[arg(long)]
    inbox_dir: Option<PathBuf>,
    #[arg(long)]
    releases_dir: Option<PathBuf>,
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    #[arg(long)]
    poll_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
struct AppConfig {
    inbox_dir: PathBuf,
    releases_dir: PathBuf,
    runtime_dir: PathBuf,
    slots: Vec<SlotConfig>,
    launch: LaunchConfig,
    health: HealthConfig,
    switch: SwitchConfig,
    poll_interval: Duration,
    shutdown_timeout: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FileConfig {
    home: Option<PathBuf>,
    inbox_dir: Option<PathBuf>,
    releases_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    poll_seconds: Option<u64>,
    shutdown_timeout_seconds: Option<u64>,
    slots: Option<BTreeMap<String, SlotFileConfig>>,
    launch: Option<LaunchConfig>,
    health: Option<HealthConfig>,
    switch: Option<SwitchConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SlotFileConfig {
    port: u16,
}

#[derive(Clone, Debug)]
struct SlotConfig {
    name: String,
    port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LaunchConfig {
    command: String,
    #[serde(default)]
    env_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum HealthConfig {
    Tcp {
        host: String,
        timeout_seconds: u64,
        interval_millis: u64,
    },
    Http {
        url: String,
        #[serde(default = "default_http_method")]
        method: String,
        #[serde(default = "default_expected_status")]
        expected_status: u16,
        #[serde(default)]
        headers: HashMap<String, String>,
        timeout_seconds: u64,
        interval_millis: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum SwitchConfig {
    NginxProxyPassInclude {
        path: PathBuf,
        reload_command: String,
        host: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum ArtifactMode {
    Move,
    Copy,
}

#[derive(Clone, Debug)]
struct RenderContext<'a> {
    artifact: &'a Path,
    port: u16,
    slot: &'a str,
    release: &'a Path,
}

impl FileConfig {
    fn sample() -> Self {
        let mut slots = BTreeMap::new();
        slots.insert(BLUE.to_string(), SlotFileConfig { port: 8081 });
        slots.insert(GREEN.to_string(), SlotFileConfig { port: 8082 });

        Self {
            home: Some(PathBuf::from("/opt/doubleshot")),
            inbox_dir: None,
            releases_dir: None,
            runtime_dir: None,
            poll_seconds: Some(5),
            shutdown_timeout_seconds: Some(20),
            slots: Some(slots),
            launch: Some(LaunchConfig {
                command: "/usr/bin/java -Dserver.port={port} -jar {artifact}".to_string(),
                env_files: vec![PathBuf::from("/opt/bankerbee/prod.env")],
            }),
            health: Some(HealthConfig::Http {
                url: "http://127.0.0.1:{port}/actuator/health".to_string(),
                method: default_http_method(),
                expected_status: default_expected_status(),
                headers: HashMap::new(),
                timeout_seconds: 120,
                interval_millis: 1000,
            }),
            switch: Some(SwitchConfig::NginxProxyPassInclude {
                path: PathBuf::from("/etc/nginx/doubleshot/proxy-pass.inc"),
                reload_command: "sudo nginx -t && sudo systemctl reload nginx".to_string(),
                host: "127.0.0.1".to_string(),
            }),
        }
    }
}

impl AppConfig {
    fn load(config_path: Option<&Path>, overrides: &CommonOverrides) -> io::Result<Self> {
        let file = load_file_config(config_path)?;
        let env_default = Self::from_env_defaults()?;
        let home = overrides
            .home
            .clone()
            .or(file.home.clone())
            .unwrap_or(env_default.home);

        let mut config = Self {
            inbox_dir: overrides
                .inbox_dir
                .clone()
                .or(file.inbox_dir)
                .unwrap_or_else(|| home.join("inbox")),
            releases_dir: overrides
                .releases_dir
                .clone()
                .or(file.releases_dir)
                .unwrap_or_else(|| home.join("releases")),
            runtime_dir: overrides
                .runtime_dir
                .clone()
                .or(file.runtime_dir)
                .unwrap_or_else(|| home.join("runtime")),
            slots: file.slots.map(slots_from_file).unwrap_or(env_default.slots),
            launch: file.launch.unwrap_or(env_default.launch),
            health: file.health.unwrap_or(env_default.health),
            switch: file.switch.unwrap_or(env_default.switch),
            poll_interval: Duration::from_secs(
                overrides
                    .poll_seconds
                    .or(file.poll_seconds)
                    .unwrap_or(env_default.poll_seconds),
            ),
            shutdown_timeout: Duration::from_secs(
                file.shutdown_timeout_seconds
                    .unwrap_or(env_default.shutdown_timeout_seconds),
            ),
        };

        config
            .slots
            .sort_by(|left, right| left.name.cmp(&right.name));
        validate_config(&config)?;
        Ok(config)
    }

    fn from_env_defaults() -> io::Result<EnvDefaults> {
        let home = path_env("DOUBLESHOT_HOME", "/opt/doubleshot");
        let java = env::var("DOUBLESHOT_JAVA").unwrap_or_else(|_| "/usr/bin/java".to_string());
        let blue_port = parse_env("DOUBLESHOT_BLUE_PORT", 8081)?;
        let green_port = parse_env("DOUBLESHOT_GREEN_PORT", 8082)?;
        let host = env::var("DOUBLESHOT_APP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let env_files = optional_path_env("DOUBLESHOT_ENV_FILE")
            .into_iter()
            .collect::<Vec<_>>();

        Ok(EnvDefaults {
            home,
            slots: vec![
                SlotConfig {
                    name: BLUE.to_string(),
                    port: blue_port,
                },
                SlotConfig {
                    name: GREEN.to_string(),
                    port: green_port,
                },
            ],
            launch: LaunchConfig {
                command: format!("{java} -Dserver.port={{port}} -jar {{artifact}}"),
                env_files,
            },
            health: HealthConfig::Tcp {
                host: host.clone(),
                timeout_seconds: parse_env("DOUBLESHOT_HEALTH_TIMEOUT_SECONDS", 120)?,
                interval_millis: parse_env("DOUBLESHOT_HEALTH_INTERVAL_MILLIS", 1000)?,
            },
            switch: SwitchConfig::NginxProxyPassInclude {
                path: env::var_os("DOUBLESHOT_NGINX_PROXY_PASS")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/etc/nginx/doubleshot/proxy-pass.inc")),
                reload_command: env::var("DOUBLESHOT_NGINX_RELOAD")
                    .unwrap_or_else(|_| "nginx -s reload".to_string()),
                host,
            },
            poll_seconds: parse_env("DOUBLESHOT_POLL_SECONDS", 5)?,
            shutdown_timeout_seconds: parse_env("DOUBLESHOT_SHUTDOWN_TIMEOUT_SECONDS", 20)?,
        })
    }

    fn ensure_dirs(&self) -> io::Result<()> {
        fs::create_dir_all(&self.inbox_dir)?;
        fs::create_dir_all(&self.releases_dir)?;
        fs::create_dir_all(&self.runtime_dir)?;
        fs::create_dir_all(self.failed_dir())?;
        Ok(())
    }

    fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join("deploy.lock")
    }

    fn active_slot_file(&self) -> PathBuf {
        self.runtime_dir.join("active-slot")
    }

    fn active_release_file(&self) -> PathBuf {
        self.runtime_dir.join("active-release")
    }

    fn pid_file(&self, slot: &str) -> PathBuf {
        self.runtime_dir.join(format!("{slot}.pid"))
    }

    fn log_file(&self, slot: &str) -> PathBuf {
        self.runtime_dir.join(format!("{slot}.log"))
    }

    fn failed_dir(&self) -> PathBuf {
        self.inbox_dir.join("failed")
    }
}

#[derive(Clone, Debug)]
struct EnvDefaults {
    home: PathBuf,
    slots: Vec<SlotConfig>,
    launch: LaunchConfig,
    health: HealthConfig,
    switch: SwitchConfig,
    poll_seconds: u64,
    shutdown_timeout_seconds: u64,
}

#[derive(Clone, Debug)]
struct NginxScanProposal {
    config: FileConfig,
    notes: Vec<String>,
}

#[derive(Clone, Debug)]
struct NginxScanFile {
    path: PathBuf,
    source: String,
}

#[derive(Clone, Debug)]
struct NginxCandidate {
    file: PathBuf,
    server_names: Vec<String>,
    listens: Vec<String>,
    location: String,
    proxy_pass: String,
    target_host: String,
    target_port: u16,
    score: i32,
}

#[derive(Clone, Debug)]
struct ParsedProxyTarget {
    host: String,
    port: u16,
}

fn scan_nginx_config(root: &Path, server_name_hint: Option<&str>) -> io::Result<NginxScanProposal> {
    let files = collect_nginx_files(root)?;
    let upstreams = collect_upstreams(&files);
    let mut candidates = Vec::new();

    for file in &files {
        let sanitized = sanitize_nginx_source(&file.source);
        let parsed = nginx_config::parse_main(&sanitized).map_err(|err| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("failed to parse {}: {err}", file.path.display()),
            )
        })?;
        collect_proxy_candidates(&parsed, &file.path, &upstreams, &mut candidates);
    }

    if candidates.is_empty() {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!("no proxy_pass candidates found from {}", root.display()),
        ));
    }

    for candidate in &mut candidates {
        candidate.score = score_nginx_candidate(candidate, server_name_hint);
    }
    candidates.sort_by_key(|right| std::cmp::Reverse(right.score));

    let selected = candidates[0].clone();
    let config = FileConfig::from_nginx_candidate(&selected);
    let mut notes = vec![format!(
        "selected {} server_name={} location={} proxy_pass={}",
        selected.file.display(),
        if selected.server_names.is_empty() {
            "<none>".to_string()
        } else {
            selected.server_names.join(",")
        },
        selected.location,
        selected.proxy_pass
    )];

    notes.push(
        "replace the selected proxy_pass with: include /etc/nginx/doubleshot/proxy-pass.inc;"
            .to_string(),
    );
    notes.push(format!(
        "detected existing backend {}:{}; generated first deploy slot avoids that occupied port",
        selected.target_host, selected.target_port
    ));

    if candidates.len() > 1 {
        notes.push("ranked nginx candidates:".to_string());
        for candidate in candidates.iter().take(5) {
            notes.push(format!(
                "score={} file={} server_name={} location={} proxy_pass={}",
                candidate.score,
                candidate.file.display(),
                if candidate.server_names.is_empty() {
                    "<none>".to_string()
                } else {
                    candidate.server_names.join(",")
                },
                candidate.location,
                candidate.proxy_pass
            ));
        }
    }

    Ok(NginxScanProposal { config, notes })
}

impl FileConfig {
    fn from_nginx_candidate(candidate: &NginxCandidate) -> Self {
        let first_port = next_available_port(candidate.target_port);
        let second_port = next_available_port(first_port);
        let mut slots = BTreeMap::new();
        slots.insert(BLUE.to_string(), SlotFileConfig { port: first_port });
        slots.insert(GREEN.to_string(), SlotFileConfig { port: second_port });

        Self {
            home: Some(PathBuf::from("/opt/doubleshot")),
            inbox_dir: None,
            releases_dir: None,
            runtime_dir: None,
            poll_seconds: Some(5),
            shutdown_timeout_seconds: Some(20),
            slots: Some(slots),
            launch: Some(LaunchConfig {
                command: "/usr/bin/java -Dserver.port={port} -jar {artifact}".to_string(),
                env_files: Vec::new(),
            }),
            health: Some(HealthConfig::Tcp {
                host: candidate.target_host.clone(),
                timeout_seconds: 120,
                interval_millis: 1000,
            }),
            switch: Some(SwitchConfig::NginxProxyPassInclude {
                path: PathBuf::from("/etc/nginx/doubleshot/proxy-pass.inc"),
                reload_command: "sudo nginx -t && sudo systemctl reload nginx".to_string(),
                host: candidate.target_host.clone(),
            }),
        }
    }
}

#[derive(Default)]
struct Slots {
    children: HashMap<String, Child>,
}

impl Slots {
    fn take(&mut self, slot: &str) -> Option<Child> {
        self.children.remove(slot)
    }

    fn put(&mut self, slot: &str, child: Child) {
        self.children.insert(slot.to_string(), child);
    }

    fn reap_finished(&mut self) {
        self.children.retain(|slot, child| match child.try_wait() {
            Ok(Some(status)) => {
                eprintln!("{slot} exited with {status}");
                false
            }
            Ok(None) => true,
            Err(err) => {
                eprintln!("failed to poll {slot}: {err}");
                false
            }
        });
    }
}

#[derive(Debug)]
struct DeployLock {
    path: PathBuf,
}

impl DeployLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "pid={}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path)? {
                        match fs::remove_file(&path) {
                            Ok(()) => continue,
                            Err(err) if err.kind() == ErrorKind::NotFound => continue,
                            Err(err) => return Err(err),
                        }
                    }

                    return Err(io::Error::new(
                        ErrorKind::WouldBlock,
                        format!("deployment semaphore is held at {}", path.display()),
                    ));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

fn lock_is_stale(path: &Path) -> io::Result<bool> {
    let contents = fs::read_to_string(path)?;
    let Some(pid) = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid="))
        .map(str::trim)
    else {
        return Ok(false);
    };

    Ok(!pid.is_empty() && !process_exists(pid))
}

impl Drop for DeployLock {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() != ErrorKind::NotFound
        {
            eprintln!("failed to remove lock {}: {err}", self.path.display());
        }
    }
}

fn serve(config: AppConfig) -> io::Result<()> {
    config.ensure_dirs()?;
    let mut slots = Slots::default();

    println!("doubleshot watching {}", config.inbox_dir.display());

    loop {
        slots.reap_finished();

        match next_artifact(&config.inbox_dir)? {
            Some(artifact) => {
                if let Err(err) =
                    deploy_artifact(&config, &artifact, ArtifactMode::Move, &mut slots)
                {
                    if err.kind() == ErrorKind::WouldBlock {
                        eprintln!("{err}");
                        thread::sleep(config.poll_interval);
                        continue;
                    }

                    eprintln!("deployment failed for {}: {err}", artifact.display());
                    quarantine_failed_artifact(&config, &artifact)?;
                }
            }
            None => thread::sleep(config.poll_interval),
        }
    }
}

fn collect_nginx_files(root: &Path) -> io::Result<Vec<NginxScanFile>> {
    let mut files = Vec::new();
    let mut seen = BTreeMap::new();
    collect_nginx_files_inner(root, &mut files, &mut seen)?;
    Ok(files)
}

fn collect_nginx_files_inner(
    path: &Path,
    files: &mut Vec<NginxScanFile>,
    seen: &mut BTreeMap<PathBuf, ()>,
) -> io::Result<()> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if seen.insert(canonical, ()).is_some() {
        return Ok(());
    }

    let source = fs::read_to_string(path)?;
    files.push(NginxScanFile {
        path: path.to_path_buf(),
        source: source.clone(),
    });

    let base = path.parent().unwrap_or_else(|| Path::new("/"));
    for include in scan_include_paths(&source, base)? {
        collect_nginx_files_inner(&include, files, seen)?;
    }

    Ok(())
}

fn scan_include_paths(source: &str, base: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in source.lines() {
        let uncommented = strip_comment(line);
        let trimmed = uncommented.trim();
        if !trimmed.starts_with("include ") {
            continue;
        }

        let include = trimmed
            .trim_start_matches("include")
            .trim()
            .trim_end_matches(';')
            .trim_matches('"')
            .trim_matches('\'');
        let include_path = if Path::new(include).is_absolute() {
            PathBuf::from(include)
        } else {
            base.join(include)
        };

        paths.extend(expand_simple_glob(&include_path)?);
    }

    Ok(paths)
}

fn expand_simple_glob(pattern: &Path) -> io::Result<Vec<PathBuf>> {
    let pattern_string = pattern.display().to_string();
    if !pattern_string.contains('*') {
        return Ok(if pattern.exists() {
            vec![pattern.to_path_buf()]
        } else {
            Vec::new()
        });
    }

    let Some(parent) = pattern.parent() else {
        return Ok(Vec::new());
    };
    let Some(name_pattern) = pattern.file_name().and_then(OsStr::to_str) else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();

    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if wildcard_match(name_pattern, name) {
            paths.push(path);
        }
    }

    paths.sort();
    Ok(paths)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }

    let mut remainder = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        if index == 0 {
            let Some(stripped) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = stripped;
            continue;
        }

        let Some(position) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[position + part.len()..];
    }

    pattern.ends_with('*') || remainder.is_empty()
}

fn sanitize_nginx_source(source: &str) -> String {
    let mut sanitized = String::new();
    let mut skip_depth: i32 = 0;

    for line in source.lines() {
        let line_without_comment = strip_comment(line);
        let trimmed = line_without_comment.trim();
        if trimmed.is_empty() {
            sanitized.push('\n');
            continue;
        }

        if skip_depth > 0 {
            skip_depth += count_char(trimmed, '{') as i32;
            skip_depth -= count_char(trimmed, '}') as i32;
            sanitized.push('\n');
            continue;
        }

        let name = first_directive_name(trimmed);
        if trimmed.contains('{') && !is_supported_nginx_block(name) {
            skip_depth += count_char(trimmed, '{') as i32;
            skip_depth -= count_char(trimmed, '}') as i32;
            sanitized.push('\n');
            continue;
        }

        if trimmed.ends_with(';') && !is_supported_nginx_directive(name) {
            sanitized.push('\n');
            continue;
        }

        sanitized.push_str(&line_without_comment);
        sanitized.push('\n');
    }

    sanitized
}

fn strip_comment(line: &str) -> String {
    line.split_once('#')
        .map(|(before, _)| before.to_string())
        .unwrap_or_else(|| line.to_string())
}

fn first_directive_name(line: &str) -> &str {
    line.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('}')
}

fn is_supported_nginx_block(name: &str) -> bool {
    matches!(name, "http" | "server" | "location" | "if" | "limit_except")
}

fn is_supported_nginx_directive(name: &str) -> bool {
    matches!(
        name,
        "include"
            | "listen"
            | "server_name"
            | "proxy_pass"
            | "proxy_set_header"
            | "proxy_http_version"
            | "proxy_connect_timeout"
            | "proxy_read_timeout"
            | "proxy_send_timeout"
            | "return"
            | "rewrite"
            | "set"
            | "root"
            | "alias"
            | "error_page"
            | "try_files"
            | "ssl_certificate"
            | "ssl_certificate_key"
            | "index"
    )
}

fn count_char(value: &str, needle: char) -> usize {
    value
        .chars()
        .filter(|candidate| *candidate == needle)
        .count()
}

fn collect_upstreams(files: &[NginxScanFile]) -> HashMap<String, ParsedProxyTarget> {
    let mut upstreams = HashMap::new();
    for file in files {
        for (name, target) in scan_upstreams(&file.source) {
            upstreams.insert(name, target);
        }
    }
    upstreams
}

fn scan_upstreams(source: &str) -> Vec<(String, ParsedProxyTarget)> {
    let mut upstreams = Vec::new();
    let lines = source.lines().collect::<Vec<_>>();
    let mut index = 0;

    while index < lines.len() {
        let trimmed = strip_comment(lines[index]).trim().to_string();
        if !trimmed.starts_with("upstream ") || !trimmed.contains('{') {
            index += 1;
            continue;
        }

        let name = trimmed
            .trim_start_matches("upstream")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('{')
            .to_string();
        let mut depth = count_char(&trimmed, '{') as i32 - count_char(&trimmed, '}') as i32;
        index += 1;

        while index < lines.len() && depth > 0 {
            let line = strip_comment(lines[index]);
            let inner = line.trim();
            if inner.starts_with("server ") {
                let server = inner
                    .trim_start_matches("server")
                    .trim()
                    .trim_end_matches(';')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if let Some(target) = parse_host_port(server) {
                    upstreams.push((name.clone(), target));
                    break;
                }
            }
            depth += count_char(inner, '{') as i32;
            depth -= count_char(inner, '}') as i32;
            index += 1;
        }
    }

    upstreams
}

fn collect_proxy_candidates(
    main: &Main,
    file: &Path,
    upstreams: &HashMap<String, ParsedProxyTarget>,
    candidates: &mut Vec<NginxCandidate>,
) {
    collect_proxy_candidates_from_directives(&main.directives, file, upstreams, candidates);
}

fn collect_proxy_candidates_from_directives(
    directives: &[Directive],
    file: &Path,
    upstreams: &HashMap<String, ParsedProxyTarget>,
    candidates: &mut Vec<NginxCandidate>,
) {
    for directive in directives {
        match &directive.item {
            Item::Http(http) => collect_proxy_candidates_from_directives(
                &http.directives,
                file,
                upstreams,
                candidates,
            ),
            Item::Server(server) => collect_server_candidates(server, file, upstreams, candidates),
            _ => {}
        }
    }
}

fn collect_server_candidates(
    server: &nginx_config::ast::Server,
    file: &Path,
    upstreams: &HashMap<String, ParsedProxyTarget>,
    candidates: &mut Vec<NginxCandidate>,
) {
    let server_names = server
        .directives
        .iter()
        .flat_map(|directive| match &directive.item {
            Item::ServerName(names) => names.iter().map(server_name_to_string).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let listens = server
        .directives
        .iter()
        .filter_map(|directive| match &directive.item {
            Item::Listen(listen) => Some(address_to_string(&listen.address)),
            _ => None,
        })
        .collect::<Vec<_>>();

    for directive in &server.directives {
        if let Item::Location(location) = &directive.item {
            let location_name = location_pattern_to_string(&location.pattern);
            for location_directive in &location.directives {
                if let Item::ProxyPass(value) = &location_directive.item {
                    let proxy_pass = value.to_string();
                    if let Some(target) = parse_proxy_pass_target(&proxy_pass, upstreams) {
                        candidates.push(NginxCandidate {
                            file: file.to_path_buf(),
                            server_names: server_names.clone(),
                            listens: listens.clone(),
                            location: location_name.clone(),
                            proxy_pass,
                            target_host: target.host,
                            target_port: target.port,
                            score: 0,
                        });
                    }
                }
            }
        }
    }
}

fn parse_proxy_pass_target(
    proxy_pass: &str,
    upstreams: &HashMap<String, ParsedProxyTarget>,
) -> Option<ParsedProxyTarget> {
    let target = proxy_pass.strip_prefix("http://")?;
    let authority = target.split('/').next().unwrap_or(target);
    if let Some(parsed) = parse_host_port(authority) {
        return Some(parsed);
    }
    upstreams.get(authority).cloned()
}

fn parse_host_port(authority: &str) -> Option<ParsedProxyTarget> {
    let (host, port) = authority.rsplit_once(':')?;
    Some(ParsedProxyTarget {
        host: host.trim_matches(['[', ']']).to_string(),
        port: port.parse().ok()?,
    })
}

fn score_nginx_candidate(candidate: &NginxCandidate, server_name_hint: Option<&str>) -> i32 {
    let mut score = 0;
    if candidate.location == "/" {
        score += 50;
    }
    if is_loopback_host(&candidate.target_host) {
        score += 25;
    }
    if candidate
        .listens
        .iter()
        .any(|listen| listen.contains("443"))
    {
        score += 10;
    }
    if let Some(hint) = server_name_hint
        && candidate
            .server_names
            .iter()
            .any(|server_name| server_name == hint)
    {
        score += 100;
    }
    score
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn next_available_port(port: u16) -> u16 {
    match port {
        0..=65533 => port + 1,
        65534 => 65533,
        65535 => 65534,
    }
}

fn server_name_to_string(name: &ServerName) -> String {
    match name {
        ServerName::Exact(value) => value.clone(),
        ServerName::Suffix(value) => format!(".{value}"),
        ServerName::StarSuffix(value) => format!("*.{value}"),
        ServerName::StarPrefix(value) => format!("{value}.*"),
        ServerName::Regex(value) => format!("~{value}"),
    }
}

fn address_to_string(address: &Address) -> String {
    match address {
        Address::Ip(address) => address.to_string(),
        Address::StarPort(port) => format!("*:{port}"),
        Address::Port(port) => port.to_string(),
        Address::Unix(path) => format!("unix:{}", path.display()),
    }
}

fn location_pattern_to_string(pattern: &LocationPattern) -> String {
    match pattern {
        LocationPattern::Prefix(value) => value.clone(),
        LocationPattern::Exact(value) => format!("= {value}"),
        LocationPattern::FinalPrefix(value) => format!("^~ {value}"),
        LocationPattern::Regex(value) => format!("~ {value}"),
        LocationPattern::RegexInsensitive(value) => format!("~* {value}"),
        LocationPattern::Named(value) => format!("@{value}"),
    }
}

fn deploy_artifact(
    config: &AppConfig,
    artifact: &Path,
    mode: ArtifactMode,
    slots: &mut Slots,
) -> io::Result<()> {
    let _lock = DeployLock::acquire(config.lock_file())?;

    let release = import_release(config, artifact, mode)?;
    let active_slot = active_slot(config)?;
    let target = target_slot(config, active_slot.as_deref())?;

    println!("deploying {}", release.display());
    println!(
        "traffic: {} -> {} on port {}",
        active_slot_label(active_slot.as_deref()),
        slot_label(&target.name),
        target.port
    );

    stop_slot_if_running(config, slots, &target.name)?;
    let context = RenderContext {
        artifact: &release,
        port: target.port,
        slot: &target.name,
        release: &release,
    };
    let mut child = launch_slot(config, &context)?;

    if let Err(err) = wait_until_ready(config, &context, &mut child) {
        let _ = stop_launched_slot(config, &mut child, &target.name);
        let _ = quarantine_failed_artifact(config, &release);
        return Err(err);
    }

    let previous_switch = capture_switch_state(config)?;
    if let Err(err) = promote(config, &context) {
        let _ = restore_switch_state(config, previous_switch);
        let _ = stop_launched_slot(config, &mut child, &target.name);
        let _ = quarantine_failed_artifact(config, &release);
        return Err(err);
    }

    if let Err(err) = write_active_state(config, &target.name, &release) {
        let _ = restore_switch_state(config, previous_switch);
        let _ = stop_launched_slot(config, &mut child, &target.name);
        let _ = quarantine_failed_artifact(config, &release);
        return Err(err);
    }

    slots.put(&target.name, child);

    if let Some(old_slot) = active_slot
        && old_slot != target.name
    {
        stop_slot_if_running(config, slots, &old_slot)?;
    }

    println!(
        "traffic switched: {} active on port {}",
        slot_label(&target.name),
        target.port
    );
    Ok(())
}

fn stop_launched_slot(config: &AppConfig, child: &mut Child, slot: &str) -> io::Result<()> {
    let result = stop_child(child, config.shutdown_timeout);
    let _ = fs::remove_file(config.pid_file(slot));
    result
}

enum SwitchState {
    NginxProxyPassInclude { contents: Option<Vec<u8>> },
}

fn capture_switch_state(config: &AppConfig) -> io::Result<SwitchState> {
    match &config.switch {
        SwitchConfig::NginxProxyPassInclude { path, .. } => match fs::read(path) {
            Ok(contents) => Ok(SwitchState::NginxProxyPassInclude {
                contents: Some(contents),
            }),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                Ok(SwitchState::NginxProxyPassInclude { contents: None })
            }
            Err(err) => Err(err),
        },
    }
}

fn restore_switch_state(config: &AppConfig, previous: SwitchState) -> io::Result<()> {
    match (&config.switch, previous) {
        (
            SwitchConfig::NginxProxyPassInclude {
                path,
                reload_command,
                ..
            },
            SwitchState::NginxProxyPassInclude { contents },
        ) => {
            match contents {
                Some(contents) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let tmp = path.with_extension("tmp");
                    fs::write(&tmp, contents)?;
                    fs::rename(tmp, path)?;
                }
                None => match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                },
            }
            run_shell(reload_command).map(|_| ())
        }
    }
}

fn write_active_state(config: &AppConfig, slot: &str, release: &Path) -> io::Result<()> {
    let active_slot = config.active_slot_file();
    let active_release = config.active_release_file();
    let active_slot_tmp = active_slot.with_extension("tmp");
    let active_release_tmp = active_release.with_extension("tmp");

    fs::write(&active_slot_tmp, slot)?;
    fs::write(&active_release_tmp, release.display().to_string())?;
    fs::rename(&active_slot_tmp, active_slot)?;
    fs::rename(&active_release_tmp, active_release)?;
    Ok(())
}

fn next_artifact(inbox_dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut artifacts = Vec::new();

    for entry in fs::read_dir(inbox_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension() != Some(OsStr::new("tmp")) {
            artifacts.push(path);
        }
    }

    artifacts.sort();
    Ok(artifacts.into_iter().next())
}

fn import_release(config: &AppConfig, artifact: &Path, mode: ArtifactMode) -> io::Result<PathBuf> {
    let name = artifact
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("artifact");
    let stamp = unix_timestamp()?;
    let release = config.releases_dir.join(format!("{stamp}-{name}"));

    match mode {
        ArtifactMode::Move => fs::rename(artifact, &release)?,
        ArtifactMode::Copy => {
            fs::copy(artifact, &release)?;
        }
    }

    Ok(release)
}

fn quarantine_failed_artifact(config: &AppConfig, artifact: &Path) -> io::Result<()> {
    if !artifact.exists() {
        return Ok(());
    }

    let name = artifact
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("failed-artifact");
    let failed = config
        .failed_dir()
        .join(format!("{}-{name}", unix_timestamp()?));
    fs::rename(artifact, failed)?;
    Ok(())
}

fn active_slot(config: &AppConfig) -> io::Result<Option<String>> {
    match fs::read_to_string(config.active_slot_file()) {
        Ok(slot) => {
            let slot = slot.trim();
            if config.slots.iter().any(|candidate| candidate.name == slot) {
                Ok(Some(slot.to_string()))
            } else {
                Ok(None)
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn target_slot(config: &AppConfig, active_slot: Option<&str>) -> io::Result<SlotConfig> {
    let Some(active_slot) = active_slot else {
        return config.slots.first().cloned().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "at least one slot is required")
        });
    };

    let index = config
        .slots
        .iter()
        .position(|slot| slot.name == active_slot)
        .unwrap_or(0);
    Ok(config.slots[(index + 1) % config.slots.len()].clone())
}

fn launch_slot(config: &AppConfig, context: &RenderContext<'_>) -> io::Result<Child> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.log_file(context.slot))?;
    let err_log = log.try_clone()?;
    let command = render_template(&config.launch.command, context);
    let mut process = Command::new("sh");

    process
        .arg("-c")
        .arg(format!("exec {command}"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));

    for env_file in &config.launch.env_files {
        for (key, value) in read_env_file(env_file)? {
            process.env(key, value);
        }
    }

    let child = process.spawn()?;
    fs::write(config.pid_file(context.slot), child.id().to_string())?;
    Ok(child)
}

fn wait_until_ready(
    config: &AppConfig,
    context: &RenderContext<'_>,
    child: &mut Child,
) -> io::Result<()> {
    match &config.health {
        HealthConfig::Tcp {
            host,
            timeout_seconds,
            interval_millis,
        } => wait_for_tcp(
            &format!("{}:{}", render_template(host, context), context.port),
            Duration::from_secs(*timeout_seconds),
            Duration::from_millis(*interval_millis),
            child,
        ),
        HealthConfig::Http {
            url,
            method,
            expected_status,
            headers,
            timeout_seconds,
            interval_millis,
        } => wait_for_http(
            &render_template(url, context),
            method,
            *expected_status,
            headers,
            Duration::from_secs(*timeout_seconds),
            Duration::from_millis(*interval_millis),
            child,
        ),
    }
}

fn promote(config: &AppConfig, context: &RenderContext<'_>) -> io::Result<()> {
    match &config.switch {
        SwitchConfig::NginxProxyPassInclude {
            path,
            reload_command,
            host,
        } => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }

            let tmp = path.with_extension("tmp");
            let host = render_template(host, context);
            fs::write(
                &tmp,
                format!("proxy_pass http://{host}:{};\n", context.port),
            )?;
            fs::rename(tmp, path)?;

            run_shell(reload_command).map(|_| ())
        }
    }
}

fn wait_for_tcp(
    address: &str,
    timeout: Duration,
    interval: Duration,
    child: &mut Child,
) -> io::Result<()> {
    let address: SocketAddr = address
        .parse()
        .map_err(|err| io::Error::new(ErrorKind::InvalidInput, err))?;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        ensure_child_running(child)?;
        match TcpStream::connect_timeout(&address, interval) {
            Ok(_) => return Ok(()),
            Err(_) => thread::sleep(interval),
        }
    }

    Err(io::Error::new(
        ErrorKind::TimedOut,
        format!("application did not accept TCP connections on {address}"),
    ))
}

fn wait_for_http(
    url: &str,
    method: &str,
    expected_status: u16,
    headers: &HashMap<String, String>,
    timeout: Duration,
    interval: Duration,
    child: &mut Child,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        ensure_child_running(child)?;
        match http_probe(url, method, headers, interval) {
            Ok(status) if status == expected_status => return Ok(()),
            Ok(_) | Err(_) => thread::sleep(interval),
        }
    }

    Err(io::Error::new(
        ErrorKind::TimedOut,
        format!("HTTP health check did not return {expected_status} for {url}"),
    ))
}

fn ensure_child_running(child: &mut Child) -> io::Result<()> {
    if let Some(status) = child.try_wait()? {
        return Err(io::Error::other(format!(
            "application exited before becoming ready with status {status}"
        )));
    }

    Ok(())
}

fn http_probe(
    url: &str,
    method: &str,
    headers: &HashMap<String, String>,
    timeout: Duration,
) -> io::Result<u16> {
    let parsed = parse_http_url(url)?;
    let address = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::AddrNotAvailable, "no address resolved"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    write!(
        stream,
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        method, parsed.path, parsed.host
    )?;
    for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    response
        .lines()
        .next()
        .and_then(|status| status.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid HTTP response"))
}

#[derive(Debug, PartialEq, Eq)]
struct HttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> io::Result<HttpUrl> {
    let without_scheme = url.strip_prefix("http://").ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("only http:// health URLs are supported: {url}"),
        )
    })?;
    let (authority, path) = without_scheme
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((without_scheme, "/".to_string()));
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "HTTP health URL needs a port"))?;

    Ok(HttpUrl {
        host: host.to_string(),
        port: port
            .parse()
            .map_err(|err| io::Error::new(ErrorKind::InvalidInput, err))?,
        path,
    })
}

fn stop_slot_if_running(config: &AppConfig, slots: &mut Slots, slot: &str) -> io::Result<()> {
    if let Some(mut child) = slots.take(slot) {
        stop_child(&mut child, config.shutdown_timeout)?;
    }

    let pid_file = config.pid_file(slot);
    let pid = match fs::read_to_string(&pid_file) {
        Ok(pid) => pid.trim().to_string(),
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if pid.is_empty() {
        let _ = fs::remove_file(pid_file);
        return Ok(());
    }

    let _ = Command::new("kill").arg("-TERM").arg(&pid).status();
    let deadline = Instant::now() + config.shutdown_timeout;

    while Instant::now() < deadline {
        if !process_exists(&pid) {
            let _ = fs::remove_file(pid_file);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }

    let _ = Command::new("kill").arg("-KILL").arg(&pid).status();
    let _ = fs::remove_file(pid_file);
    Ok(())
}

fn stop_child(child: &mut Child, timeout: Duration) -> io::Result<()> {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn print_status(config: &AppConfig) -> io::Result<()> {
    println!("inbox: {}", config.inbox_dir.display());
    println!("releases: {}", config.releases_dir.display());
    println!("runtime: {}", config.runtime_dir.display());
    println!(
        "active slot: {}",
        active_slot(config)?.unwrap_or_else(|| "none".to_string())
    );
    println!(
        "active release: {}",
        read_optional_string(&config.active_release_file())?.unwrap_or_else(|| "none".to_string())
    );
    println!("slots:");
    for slot in &config.slots {
        let pid = read_optional_string(&config.pid_file(&slot.name))?
            .unwrap_or_else(|| "not running".to_string());
        println!("  {} port={} pid={}", slot.name, slot.port, pid.trim());
    }
    Ok(())
}

fn load_file_config(config_path: Option<&Path>) -> io::Result<FileConfig> {
    let path = config_path.unwrap_or_else(|| Path::new(DEFAULT_CONFIG));
    match fs::read_to_string(path) {
        Ok(contents) => {
            toml::from_str(&contents).map_err(|err| io::Error::new(ErrorKind::InvalidData, err))
        }
        Err(err) if err.kind() == ErrorKind::NotFound && config_path.is_none() => Ok(FileConfig {
            home: None,
            inbox_dir: None,
            releases_dir: None,
            runtime_dir: None,
            poll_seconds: None,
            shutdown_timeout_seconds: None,
            slots: None,
            launch: None,
            health: None,
            switch: None,
        }),
        Err(err) => Err(err),
    }
}

fn slots_from_file(slots: BTreeMap<String, SlotFileConfig>) -> Vec<SlotConfig> {
    slots
        .into_iter()
        .map(|(name, slot)| SlotConfig {
            name,
            port: slot.port,
        })
        .collect()
}

fn validate_config(config: &AppConfig) -> io::Result<()> {
    if config.slots.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "at least one deployment slot is required",
        ));
    }

    if !config.launch.command.contains("{artifact}") || !config.launch.command.contains("{port}") {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "launch.command must include {artifact} and {port}",
        ));
    }

    Ok(())
}

fn read_optional_string(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value.trim().to_string())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn read_env_file(path: &Path) -> io::Result<Vec<(String, String)>> {
    let contents = fs::read_to_string(path)?;
    let mut values = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid env line in {}: {line}", path.display()),
            ));
        };

        let key = key.trim();
        if key.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("empty env key in {}", path.display()),
            ));
        }

        values.push((key.to_string(), unquote_env_value(value.trim()).to_string()));
    }

    Ok(values)
}

fn unquote_env_value(value: &str) -> &str {
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }

    value
}

fn render_template(template: &str, context: &RenderContext<'_>) -> String {
    template
        .replace(
            "{artifact}",
            &shell_quote(&context.artifact.display().to_string()),
        )
        .replace(
            "{release}",
            &shell_quote(&context.release.display().to_string()),
        )
        .replace("{port}", &context.port.to_string())
        .replace("{slot}", context.slot)
}

fn active_slot_label(slot: Option<&str>) -> String {
    slot.map(slot_label).unwrap_or_else(|| "none".to_string())
}

fn slot_label(slot: &str) -> String {
    match slot {
        BLUE => format!("{BLUE_FG}⏹{RESET} {BLUE}"),
        GREEN => format!("{GREEN_FG}⏹{RESET} {GREEN}"),
        other => format!("⏹ {other}"),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_shell(command: &str) -> io::Result<()> {
    let status = Command::new("sh").arg("-c").arg(command).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "command exited with {status}: {command}"
        )))
    }
}

fn path_env(name: &str, default: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn optional_path_env(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

fn parse_env<T>(name: &str, default: T) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => value.parse().map_err(|err| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid value for {name}: {err}"),
            )
        }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(io::Error::new(ErrorKind::InvalidInput, err)),
    }
}

fn unix_timestamp() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(io::Error::other)
}

fn default_http_method() -> String {
    "GET".to_string()
}

fn default_expected_status() -> u16 {
    200
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(artifact: &'a Path) -> RenderContext<'a> {
        RenderContext {
            artifact,
            port: 8081,
            slot: BLUE,
            release: artifact,
        }
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "doubleshot-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fixture(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn test_config(dir: &Path, blue_port: u16, green_port: u16) -> AppConfig {
        AppConfig {
            inbox_dir: dir.join("inbox"),
            releases_dir: dir.join("releases"),
            runtime_dir: dir.join("runtime"),
            slots: vec![
                SlotConfig {
                    name: BLUE.to_string(),
                    port: blue_port,
                },
                SlotConfig {
                    name: GREEN.to_string(),
                    port: green_port,
                },
            ],
            launch: LaunchConfig {
                command: "sleep 30 # {artifact} {port}".to_string(),
                env_files: vec![],
            },
            health: HealthConfig::Http {
                url: "http://127.0.0.1:{port}/health".to_string(),
                method: "GET".to_string(),
                expected_status: 200,
                headers: HashMap::new(),
                timeout_seconds: 2,
                interval_millis: 50,
            },
            switch: SwitchConfig::NginxProxyPassInclude {
                path: dir.join("proxy-pass.inc"),
                reload_command: "true".to_string(),
                host: "127.0.0.1".to_string(),
            },
            poll_interval: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        }
    }

    fn scan_nginx_fixture(
        name: &str,
        files: &[(&str, &str)],
        server_name_hint: Option<&str>,
    ) -> NginxScanProposal {
        let dir = temp_test_dir(name);
        for (relative_path, contents) in files {
            write_fixture(&dir.join(relative_path), contents);
        }
        scan_nginx_config(&dir.join("nginx.conf"), server_name_hint).unwrap()
    }

    fn assert_generated_config(
        proposal: &NginxScanProposal,
        blue_port: u16,
        green_port: u16,
        host: &str,
    ) {
        let slots = proposal.config.slots.as_ref().unwrap();
        assert_eq!(slots.get(BLUE).unwrap().port, blue_port);
        assert_eq!(slots.get(GREEN).unwrap().port, green_port);

        let launch = proposal.config.launch.as_ref().unwrap();
        assert_eq!(
            launch.command,
            "/usr/bin/java -Dserver.port={port} -jar {artifact}"
        );

        match proposal.config.health.as_ref().unwrap() {
            HealthConfig::Tcp { host: actual, .. } => assert_eq!(actual, host),
            other => panic!("expected tcp health config, got {other:?}"),
        }

        match proposal.config.switch.as_ref().unwrap() {
            SwitchConfig::NginxProxyPassInclude { host: actual, .. } => assert_eq!(actual, host),
        }
    }

    #[test]
    fn renders_command_templates() {
        let artifact = Path::new("/tmp/my app.jar");
        let rendered = render_template(
            "java -jar {artifact} --port {port} --slot {slot}",
            &context(artifact),
        );

        assert_eq!(
            rendered,
            "java -jar '/tmp/my app.jar' --port 8081 --slot blue"
        );
    }

    #[test]
    fn renders_colored_slot_labels() {
        assert_eq!(slot_label(BLUE), "\x1b[34m⏹\x1b[0m blue");
        assert_eq!(slot_label(GREEN), "\x1b[32m⏹\x1b[0m green");
    }

    #[test]
    fn parses_http_health_urls() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:8081/actuator/health").unwrap(),
            HttpUrl {
                host: "127.0.0.1".to_string(),
                port: 8081,
                path: "/actuator/health".to_string()
            }
        );
    }

    #[test]
    fn chooses_next_slot_after_active_slot() {
        let config = AppConfig {
            inbox_dir: PathBuf::from("/tmp/inbox"),
            releases_dir: PathBuf::from("/tmp/releases"),
            runtime_dir: PathBuf::from("/tmp/runtime"),
            slots: vec![
                SlotConfig {
                    name: BLUE.to_string(),
                    port: 8081,
                },
                SlotConfig {
                    name: GREEN.to_string(),
                    port: 8082,
                },
            ],
            launch: LaunchConfig {
                command: "run {artifact} {port}".to_string(),
                env_files: vec![],
            },
            health: HealthConfig::Tcp {
                host: "127.0.0.1".to_string(),
                timeout_seconds: 1,
                interval_millis: 1,
            },
            switch: SwitchConfig::NginxProxyPassInclude {
                path: PathBuf::from("/tmp/proxy-pass.inc"),
                reload_command: "true".to_string(),
                host: "127.0.0.1".to_string(),
            },
            poll_interval: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        };

        assert_eq!(target_slot(&config, None).unwrap().name, BLUE);
        assert_eq!(target_slot(&config, Some(BLUE)).unwrap().name, GREEN);
        assert_eq!(target_slot(&config, Some(GREEN)).unwrap().name, BLUE);
    }

    #[test]
    fn ignores_unknown_active_slot_state() {
        let dir = temp_test_dir("unknown-active-slot");
        let config = test_config(&dir, 8081, 8082);
        fs::create_dir_all(&config.runtime_dir).unwrap();
        fs::write(config.active_slot_file(), "purple").unwrap();

        assert_eq!(active_slot(&config).unwrap(), None);
    }

    #[test]
    fn deploy_lock_rejects_live_owner() {
        let dir = temp_test_dir("live-lock");
        let path = dir.join("deploy.lock");
        fs::write(&path, format!("pid={}\n", std::process::id())).unwrap();

        let err = DeployLock::acquire(path).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn deploy_lock_recovers_dead_owner() {
        let dir = temp_test_dir("stale-lock");
        let path = dir.join("deploy.lock");
        fs::write(&path, "pid=999999999\n").unwrap();

        let lock = DeployLock::acquire(path.clone()).unwrap();

        assert!(fs::read_to_string(&path).unwrap().contains("pid="));
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn parses_env_files_with_quotes_and_rejects_invalid_lines() {
        let dir = temp_test_dir("env-files");
        let valid = dir.join("valid.env");
        write_fixture(
            &valid,
            r#"
# comment
PLAIN=value
DOUBLE="quoted value"
SINGLE='single quoted'
"#,
        );

        assert_eq!(
            read_env_file(&valid).unwrap(),
            vec![
                ("PLAIN".to_string(), "value".to_string()),
                ("DOUBLE".to_string(), "quoted value".to_string()),
                ("SINGLE".to_string(), "single quoted".to_string()),
            ]
        );

        let invalid = dir.join("invalid.env");
        write_fixture(&invalid, "NOT_A_PAIR\n");
        assert_eq!(
            read_env_file(&invalid).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn failed_promotion_restores_switch_and_stops_new_slot() {
        let dir = temp_test_dir("failed-promotion-cleanup");
        let mut config = test_config(&dir, 18081, 18082);
        config.switch = SwitchConfig::NginxProxyPassInclude {
            path: dir.join("proxy-pass.inc"),
            reload_command: "false".to_string(),
            host: "127.0.0.1".to_string(),
        };
        config.ensure_dirs().unwrap();
        write_fixture(&config.active_slot_file(), BLUE);
        write_fixture(&config.active_release_file(), "/old/release.jar");
        write_fixture(
            &dir.join("proxy-pass.inc"),
            "proxy_pass http://127.0.0.1:18081;\n",
        );
        let release = dir.join("releases/artifact.txt");
        write_fixture(&release, "payload");
        let context = RenderContext {
            artifact: &release,
            port: 18082,
            slot: GREEN,
            release: &release,
        };
        let mut child = launch_slot(&config, &context).unwrap();

        let previous_switch = capture_switch_state(&config).unwrap();
        let err = promote(&config, &context).unwrap_err();
        assert!(restore_switch_state(&config, previous_switch).is_err());
        stop_launched_slot(&config, &mut child, GREEN).unwrap();

        assert_eq!(err.kind(), ErrorKind::Other);
        assert_eq!(fs::read_to_string(config.active_slot_file()).unwrap(), BLUE);
        assert_eq!(
            fs::read_to_string(config.active_release_file()).unwrap(),
            "/old/release.jar"
        );
        assert_eq!(
            fs::read_to_string(dir.join("proxy-pass.inc")).unwrap(),
            "proxy_pass http://127.0.0.1:18081;\n"
        );
        assert!(!config.pid_file(GREEN).exists());
    }

    #[test]
    fn scans_nginx_direct_proxy_and_avoids_active_port() {
        let dir = temp_test_dir("direct-proxy");
        let root = dir.join("nginx.conf");
        write_fixture(
            &root,
            r#"
http {
    server {
        listen 443 ssl;
        server_name api.example.com;
        location / {
            limit_req zone=api_limit burst=30 nodelay;
            proxy_pass http://127.0.0.1:8080;
        }
    }
}
"#,
        );

        let proposal = scan_nginx_config(&root, None).unwrap();
        let slots = proposal.config.slots.unwrap();

        assert_eq!(slots.get(BLUE).unwrap().port, 8081);
        assert_eq!(slots.get(GREEN).unwrap().port, 8082);
        assert!(proposal.notes[0].contains("api.example.com"));
    }

    #[test]
    fn scans_nginx_includes_and_named_upstream() {
        let dir = temp_test_dir("named-upstream");
        let root = dir.join("nginx.conf");
        let app = dir.join("conf.d/app.conf");
        write_fixture(
            &root,
            r#"
http {
    include conf.d/*.conf;
}
"#,
        );
        write_fixture(
            &app,
            r#"
upstream app_backend {
    server 127.0.0.1:9000;
}

server {
    server_name app.example.com;
    location / {
        proxy_pass http://app_backend;
    }
}
"#,
        );

        let proposal = scan_nginx_config(&root, Some("app.example.com")).unwrap();
        let slots = proposal.config.slots.unwrap();

        assert_eq!(slots.get(BLUE).unwrap().port, 9001);
        assert_eq!(slots.get(GREEN).unwrap().port, 9002);
        assert!(proposal.notes[0].contains("app_backend"));
    }

    #[test]
    fn ranks_server_name_hint_first() {
        let dir = temp_test_dir("ranking");
        let root = dir.join("nginx.conf");
        write_fixture(
            &root,
            r#"
http {
    server {
        server_name first.example.com;
        location / {
            proxy_pass http://127.0.0.1:8080;
        }
    }
    server {
        server_name wanted.example.com;
        location /api {
            proxy_pass http://127.0.0.1:7000;
        }
    }
}
"#,
        );

        let proposal = scan_nginx_config(&root, Some("wanted.example.com")).unwrap();
        let slots = proposal.config.slots.unwrap();

        assert_eq!(slots.get(BLUE).unwrap().port, 7001);
        assert!(proposal.notes[0].contains("wanted.example.com"));
    }

    #[test]
    fn nginx_scan_generates_config_for_plain_http_server() {
        let proposal = scan_nginx_fixture(
            "plain-http",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        listen 80;
        server_name app.example.com;
        location / {
            proxy_pass http://127.0.0.1:3000;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 3001, 3002, "127.0.0.1");
        assert!(proposal.notes[0].contains("app.example.com"));
    }

    #[test]
    fn nginx_scan_generates_config_when_proxy_pass_has_uri_path() {
        let proposal = scan_nginx_fixture(
            "proxy-pass-path",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        server_name app.example.com;
        location /api/ {
            proxy_pass http://127.0.0.1:4100/internal/;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 4101, 4102, "127.0.0.1");
        assert!(proposal.notes[0].contains("proxy_pass=http://127.0.0.1:4100/internal/"));
    }

    #[test]
    fn nginx_scan_generates_config_for_localhost_backend() {
        let proposal = scan_nginx_fixture(
            "localhost-backend",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        server_name local.example.com;
        location / {
            proxy_pass http://localhost:5000;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 5001, 5002, "localhost");
    }

    #[test]
    fn nginx_scan_generates_config_for_private_network_backend() {
        let proposal = scan_nginx_fixture(
            "private-network-backend",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        server_name internal.example.com;
        location / {
            proxy_pass http://10.20.30.40:7000;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 7001, 7002, "10.20.30.40");
    }

    #[test]
    fn nginx_scan_prefers_https_listener_when_no_hint_is_given() {
        let proposal = scan_nginx_fixture(
            "https-listener-rank",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        listen 80;
        server_name web.example.com;
        location / {
            proxy_pass http://127.0.0.1:8100;
        }
    }
    server {
        listen 443 ssl;
        server_name secure.example.com;
        location / {
            proxy_pass http://127.0.0.1:8200;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 8201, 8202, "127.0.0.1");
        assert!(proposal.notes[0].contains("secure.example.com"));
    }

    #[test]
    fn nginx_scan_prefers_root_location_over_nested_location() {
        let proposal = scan_nginx_fixture(
            "root-location-rank",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        server_name app.example.com;
        location /api {
            proxy_pass http://127.0.0.1:9100;
        }
        location / {
            proxy_pass http://127.0.0.1:9200;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 9201, 9202, "127.0.0.1");
        assert!(proposal.notes[0].contains("location=/ "));
    }

    #[test]
    fn nginx_scan_uses_server_name_hint_over_default_ranking() {
        let proposal = scan_nginx_fixture(
            "server-name-hint-over-rank",
            &[(
                "nginx.conf",
                r#"
http {
    server {
        listen 443 ssl;
        server_name default.example.com;
        location / {
            proxy_pass http://127.0.0.1:9300;
        }
    }
    server {
        listen 80;
        server_name hinted.example.com;
        location /api {
            proxy_pass http://127.0.0.1:9400;
        }
    }
}
"#,
            )],
            Some("hinted.example.com"),
        );

        assert_generated_config(&proposal, 9401, 9402, "127.0.0.1");
        assert!(proposal.notes[0].contains("hinted.example.com"));
    }

    #[test]
    fn nginx_scan_generates_config_from_quoted_include() {
        let proposal = scan_nginx_fixture(
            "quoted-include",
            &[
                (
                    "nginx.conf",
                    r#"
http {
    include "conf.d/app.conf";
}
"#,
                ),
                (
                    "conf.d/app.conf",
                    r#"
server {
    server_name include.example.com;
    location / {
        proxy_pass http://127.0.0.1:9500;
    }
}
"#,
                ),
            ],
            None,
        );

        assert_generated_config(&proposal, 9501, 9502, "127.0.0.1");
        assert!(proposal.notes[0].contains("conf.d/app.conf"));
    }

    #[test]
    fn nginx_scan_generates_config_from_upstream_with_path_suffix() {
        let proposal = scan_nginx_fixture(
            "upstream-path-suffix",
            &[(
                "nginx.conf",
                r#"
http {
    upstream java_app {
        server 127.0.0.1:9600;
    }

    server {
        server_name upstream.example.com;
        location / {
            proxy_pass http://java_app/service/;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 9601, 9602, "127.0.0.1");
        assert!(proposal.notes[0].contains("proxy_pass=http://java_app/service/"));
    }

    #[test]
    fn nginx_scan_generates_config_when_unknown_directives_are_present() {
        let proposal = scan_nginx_fixture(
            "unknown-directives",
            &[(
                "nginx.conf",
                r#"
http {
    map $http_upgrade $connection_upgrade {
        default upgrade;
        '' close;
    }

    server {
        listen 443 ssl http2;
        server_name noisy.example.com;
        gzip on;

        location / {
            proxy_http_version 1.1;
            proxy_set_header Host $host;
            proxy_pass http://127.0.0.1:9700;
        }
    }
}
"#,
            )],
            None,
        );

        assert_generated_config(&proposal, 9701, 9702, "127.0.0.1");
        assert!(proposal.notes[0].contains("noisy.example.com"));
    }

    #[test]
    fn matches_simple_wildcards() {
        assert!(wildcard_match("*.conf", "app.conf"));
        assert!(wildcard_match("api-*.conf", "api-prod.conf"));
        assert!(!wildcard_match("api-*.conf", "web-prod.conf"));
    }
}
