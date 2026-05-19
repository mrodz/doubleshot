use clap::{Args, Parser, Subcommand};
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
        CommandKind::InitConfig { output } => {
            let sample = toml::to_string_pretty(&FileConfig::sample())
                .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
            match output {
                Some(path) => fs::write(path, sample),
                None => {
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

struct DeployLock {
    path: PathBuf,
}

impl DeployLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id())?;
                Ok(Self { path })
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => Err(io::Error::new(
                ErrorKind::WouldBlock,
                format!("deployment semaphore is held at {}", path.display()),
            )),
            Err(err) => Err(err),
        }
    }
}

impl Drop for DeployLock {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.path) {
            if err.kind() != ErrorKind::NotFound {
                eprintln!("failed to remove lock {}: {err}", self.path.display());
            }
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

    println!(
        "deploying {} to {} on port {}",
        release.display(),
        target.name,
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

    if let Err(err) = wait_until_ready(config, &context) {
        let _ = stop_child(&mut child, config.shutdown_timeout);
        let _ = quarantine_failed_artifact(config, &release);
        return Err(err);
    }

    promote(config, &context)?;
    fs::write(config.active_slot_file(), &target.name)?;
    fs::write(config.active_release_file(), release.display().to_string())?;
    slots.put(&target.name, child);

    if let Some(old_slot) = active_slot {
        if old_slot != target.name {
            stop_slot_if_running(config, slots, &old_slot)?;
        }
    }

    println!(
        "deployment promoted {} on port {}",
        target.name, target.port
    );
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

fn wait_until_ready(config: &AppConfig, context: &RenderContext<'_>) -> io::Result<()> {
    match &config.health {
        HealthConfig::Tcp {
            host,
            timeout_seconds,
            interval_millis,
        } => wait_for_tcp(
            &format!("{}:{}", render_template(host, context), context.port),
            Duration::from_secs(*timeout_seconds),
            Duration::from_millis(*interval_millis),
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

fn wait_for_tcp(address: &str, timeout: Duration, interval: Duration) -> io::Result<()> {
    let address: SocketAddr = address
        .parse()
        .map_err(|err| io::Error::new(ErrorKind::InvalidInput, err))?;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
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
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
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
}
