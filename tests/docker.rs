#![cfg(feature = "docker-tests")]

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
    time::{SystemTime, UNIX_EPOCH},
};

use testcontainers::{
    Container, GenericBuildableImage, GenericImage, ImageExt,
    core::{BuildImageOptions, CmdWaitFor, ExecCommand, Host, IntoContainerPort, Mount, WaitFor},
    runners::{SyncBuilder, SyncRunner},
};

const BIN: &str = env!("CARGO_BIN_EXE_doubleshot");
const SSH_USER: &str = "deployer";
const SSH_PASSWORD: &str = "doubleshot";
const SSH_HOST: &str = "host.testcontainers.internal";
const REMOTE_HOME: &str = "/opt/doubleshot";
const REMOTE_CONFIG: &str = "/opt/doubleshot/doubleshot.toml";
const SSH_BLUE_PORT: u16 = 18081;
const SSH_GREEN_PORT: u16 = 18082;

#[derive(Clone, Debug)]
enum DockerAvailability {
    Available,
    Unavailable(String),
    CannotBuildImages(String),
}

fn docker_availability() -> DockerAvailability {
    static AVAILABLE: OnceLock<DockerAvailability> = OnceLock::new();
    AVAILABLE
        .get_or_init(|| {
            if Command::new("docker")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                return DockerAvailability::Unavailable(
                    "Docker CLI was not found on PATH.".to_string(),
                );
            }

            if !docker_daemon_running() {
                eprintln!("Docker CLI exists, but the Docker daemon is not running or is unreachable.");
                if prompt_yes_no("Start Docker before running docker-tests?") {
                    start_docker_daemon();
                    wait_for_docker_daemon();
                } else {
                    return DockerAvailability::Unavailable(
                        "Docker daemon is not running; user did not approve starting it.".to_string(),
                    );
                }
            }

            if !docker_daemon_running() {
                return DockerAvailability::Unavailable(
                    "Docker daemon is still not reachable after the start attempt.".to_string(),
                );
            }

            if let Err(err) = GenericImage::new("alpine", "3.20")
                .with_wait_for(WaitFor::seconds(1))
                .with_cmd(["sh", "-c", "true"])
                .start()
            {
                return DockerAvailability::Unavailable(format!(
                    "Docker daemon is reachable, but testcontainers could not start a probe container: {err}"
                ));
            }

            match build_probe_image() {
                Ok(()) => DockerAvailability::Available,
                Err(err) => DockerAvailability::CannotBuildImages(format!(
                    "Docker daemon is reachable, but docker-tests cannot build Docker images: {err}"
                )),
            }
        })
        .clone()
}

fn require_docker() {
    match docker_availability() {
        DockerAvailability::Available => (),
        DockerAvailability::Unavailable(reason) => {
            panic!("docker-tests require Docker, but it is unavailable: {reason}");
        }
        DockerAvailability::CannotBuildImages(reason) => {
            panic!("{reason}");
        }
    }
}

fn build_probe_image() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir("image-build-probe");
    write_file(&dir.join("Dockerfile"), "FROM alpine:3.20\nRUN true\n");

    let tag = format!(
        "probe-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );

    GenericBuildableImage::new("doubleshot-image-build-probe", tag)
        .with_dockerfile(dir.join("Dockerfile"))
        .build_image_with(BuildImageOptions::new())
        .map(|_| ())
        .map_err(Into::into)
}

fn docker_daemon_running() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn prompt_yes_no(question: &str) -> bool {
    if !io::stdin().is_terminal() {
        eprintln!("{question} Not prompting because stdin is not interactive.");
        return false;
    }

    eprint!("{question} [y/N] ");
    let _ = io::stderr().flush();

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }

    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn start_docker_daemon() {
    for command in docker_start_commands() {
        eprintln!("trying to start Docker with: {}", command.join(" "));
        if let Some((program, args)) = command.split_first() {
            let _ = Command::new(program)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        if docker_daemon_running() {
            return;
        }
    }
}

fn docker_start_commands() -> Vec<Vec<&'static str>> {
    if cfg!(target_os = "macos") {
        vec![
            vec!["open", "-a", "Docker"],
            vec!["open", "-a", "OrbStack"],
            vec!["colima", "start"],
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            vec![
                "powershell",
                "-NoProfile",
                "-Command",
                "Start-Service docker",
            ],
            vec![
                "powershell",
                "-NoProfile",
                "-Command",
                "Start-Process 'Docker Desktop'",
            ],
        ]
    } else {
        vec![
            vec!["systemctl", "--user", "start", "docker"],
            vec!["systemctl", "start", "docker"],
            vec!["service", "docker", "start"],
        ]
    }
}

fn wait_for_docker_daemon() {
    for _ in 0..30 {
        if docker_daemon_running() {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "doubleshot-docker-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn run_doubleshot(args: &[&str]) -> std::process::Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[derive(Debug)]
struct ContainerCommandOutput {
    code: i64,
    stdout: String,
    stderr: String,
}

impl ContainerCommandOutput {
    fn success(&self) -> bool {
        self.code == 0
    }
}

struct SshTestRig {
    _server: Container<GenericImage>,
    deployer: Container<GenericImage>,
    ssh_port: u16,
}

impl SshTestRig {
    fn start(expected_status: u16) -> Self {
        let rig = Self::start_empty();
        rig.write_remote_config(&ssh_deploy_config(expected_status));
        rig.start_remote_serve();
        rig
    }

    fn start_empty() -> Self {
        let server_image = build_ssh_server_image();
        let deployer_image = build_ssh_deployer_image();

        let server = server_image
            .with_exposed_port(22.tcp())
            .with_wait_for(WaitFor::message_on_stderr("Server listening"))
            .start()
            .unwrap();
        let ssh_port = server.get_host_port_ipv4(22.tcp()).unwrap();

        let deployer = deployer_image
            .with_wait_for(WaitFor::seconds(1))
            .with_host(SSH_HOST, Host::HostGateway)
            .start()
            .unwrap();

        let rig = Self {
            _server: server,
            deployer,
            ssh_port,
        };
        rig
    }

    fn ssh(&self, remote_command: &str) -> ContainerCommandOutput {
        self.exec_deployer(&format!(
            "{} {}",
            self.ssh_prefix(),
            shell_quote_str(remote_command)
        ))
    }

    fn exec_deployer(&self, script: &str) -> ContainerCommandOutput {
        exec_container_shell(&self.deployer, script)
    }

    fn upload_artifact(&self, artifact: &str, contents: &str) {
        let output = self.exec_deployer(&format!(
            "printf %s {} > /work/{artifact} && {} /work/{artifact} {}@{}:{REMOTE_HOME}/inbox/{artifact}",
            shell_quote_str(contents),
            self.scp_prefix(),
            SSH_USER,
            SSH_HOST,
        ));
        assert!(
            output.success(),
            "scp failed\nstdout:\n{}\nstderr:\n{}",
            output.stdout,
            output.stderr
        );
    }

    fn start_follow(&self, label: &str, artifact: &str, timeout_seconds: u64) {
        let remote = format!(
            "doubleshot --config {REMOTE_CONFIG} follow {artifact} --timeout-seconds {timeout_seconds}",
        );
        let script = format!(
            "rm -f /work/{label}.out /work/{label}.err /work/{label}.code; \
             nohup sh -c {} >/work/{label}.nohup 2>&1 &",
            shell_quote_str(&format!(
                "{} {} > /work/{label}.out 2> /work/{label}.err; echo $? > /work/{label}.code",
                self.ssh_prefix(),
                shell_quote_str(&remote),
            )),
        );

        let output = self.exec_deployer(&script);
        assert!(
            output.success(),
            "failed to start follow\nstdout:\n{}\nstderr:\n{}",
            output.stdout,
            output.stderr
        );
    }

    fn wait_follow(&self, label: &str, timeout: Duration) -> ContainerCommandOutput {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let code = self.exec_deployer(&format!("cat /work/{label}.code 2>/dev/null || true"));
            if !code.stdout.trim().is_empty() {
                let stdout =
                    self.exec_deployer(&format!("cat /work/{label}.out 2>/dev/null || true"));
                let stderr =
                    self.exec_deployer(&format!("cat /work/{label}.err 2>/dev/null || true"));
                return ContainerCommandOutput {
                    code: code.stdout.trim().parse().unwrap_or(-1),
                    stdout: stdout.stdout,
                    stderr: stderr.stdout,
                };
            }
            thread::sleep(Duration::from_millis(100));
        }

        let stdout = self.exec_deployer(&format!("cat /work/{label}.out 2>/dev/null || true"));
        let stderr = self.exec_deployer(&format!("cat /work/{label}.err 2>/dev/null || true"));
        panic!(
            "follow did not finish within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
            stdout.stdout, stderr.stdout
        );
    }

    fn follow_artifact(
        &self,
        label: &str,
        artifact: &str,
        timeout_seconds: u64,
    ) -> ContainerCommandOutput {
        self.start_follow(label, artifact, timeout_seconds);
        self.wait_follow(label, Duration::from_secs(timeout_seconds + 10))
    }

    fn write_remote_config(&self, config: &str) {
        let output = self.exec_deployer(&format!(
            "cat > /work/doubleshot.toml <<'EOF'\n{config}\nEOF\n\
             {} 'mkdir -p {REMOTE_HOME}/inbox {REMOTE_HOME}/www {REMOTE_HOME}/runtime {REMOTE_HOME}/releases && printf ok > {REMOTE_HOME}/www/index.html' && \
             {} /work/doubleshot.toml {}@{}:{REMOTE_CONFIG}",
            self.ssh_prefix(),
            self.scp_prefix(),
            SSH_USER,
            SSH_HOST,
        ));
        assert!(
            output.success(),
            "remote config setup failed\nstdout:\n{}\nstderr:\n{}",
            output.stdout,
            output.stderr
        );
    }

    fn start_remote_serve(&self) {
        let output = self.ssh(&format!(
            "nohup doubleshot --config {REMOTE_CONFIG} serve > {REMOTE_HOME}/serve.log 2>&1 & echo $! > {REMOTE_HOME}/serve.pid"
        ));
        assert!(
            output.success(),
            "failed to start remote serve\nstdout:\n{}\nstderr:\n{}",
            output.stdout,
            output.stderr
        );

        let wait = self.exec_deployer(&format!(
            "for i in $(seq 1 80); do \
                 {} 'test -f {REMOTE_HOME}/serve.log && grep -q \"doubleshot watching\" {REMOTE_HOME}/serve.log' && exit 0; \
                 sleep 0.1; \
             done; \
             {} 'cat {REMOTE_HOME}/serve.log 2>/dev/null || true'; \
             exit 1",
            self.ssh_prefix(),
            self.ssh_prefix(),
        ));
        assert!(
            wait.success(),
            "serve did not become ready\nstdout:\n{}\nstderr:\n{}",
            wait.stdout,
            wait.stderr
        );
    }

    fn ssh_prefix(&self) -> String {
        format!(
            "sshpass -p {} ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p {} {}@{}",
            shell_quote_str(SSH_PASSWORD),
            self.ssh_port,
            SSH_USER,
            SSH_HOST,
        )
    }

    fn scp_prefix(&self) -> String {
        format!(
            "sshpass -p {} scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P {}",
            shell_quote_str(SSH_PASSWORD),
            self.ssh_port,
        )
    }
}

fn exec_container_shell(
    container: &Container<GenericImage>,
    script: &str,
) -> ContainerCommandOutput {
    let mut result = container
        .exec(ExecCommand::new(["sh", "-lc", script]).with_cmd_ready_condition(CmdWaitFor::exit()))
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout_to_vec().unwrap()).to_string();
    let stderr = String::from_utf8_lossy(&result.stderr_to_vec().unwrap()).to_string();
    let code = result.exit_code().unwrap().unwrap_or(-1);

    ContainerCommandOutput {
        code,
        stdout,
        stderr,
    }
}

fn build_ssh_server_image() -> GenericImage {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    GenericBuildableImage::new("doubleshot-ssh-server", ssh_image_tag())
        .with_dockerfile(manifest.join("tests/ssh/Dockerfile.server"))
        .with_file(manifest.join("Cargo.toml"), "Cargo.toml")
        .with_file(manifest.join("Cargo.lock"), "Cargo.lock")
        .with_file(manifest.join("src"), "src")
        .build_image_with(BuildImageOptions::new().with_skip_if_exists(true))
        .unwrap()
}

fn build_ssh_deployer_image() -> GenericImage {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    GenericBuildableImage::new("doubleshot-ssh-deployer", ssh_image_tag())
        .with_dockerfile(manifest.join("tests/ssh/Dockerfile.deployer"))
        .build_image_with(BuildImageOptions::new().with_skip_if_exists(true))
        .unwrap()
}

fn ssh_image_tag() -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut hasher = DefaultHasher::new();
    for path in [
        manifest.join("Cargo.toml"),
        manifest.join("Cargo.lock"),
        manifest.join("src/main.rs"),
        manifest.join("tests/ssh/Dockerfile.server"),
        manifest.join("tests/ssh/Dockerfile.deployer"),
    ] {
        path.display().to_string().hash(&mut hasher);
        fs::read(path).unwrap().hash(&mut hasher);
    }
    format!("test-{:x}", hasher.finish())
}

fn ssh_deploy_config(expected_status: u16) -> String {
    format!(
        r#"
home = "{REMOTE_HOME}"
poll_seconds = 1
shutdown_timeout_seconds = 1

[slots.blue]
port = {SSH_BLUE_PORT}

[slots.green]
port = {SSH_GREEN_PORT}

[launch]
command = "python3 -m http.server {{port}} --bind 127.0.0.1 --directory {REMOTE_HOME}/www # {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/"
method = "GET"
expected_status = {expected_status}
timeout_seconds = 2
interval_millis = 100

[switch]
kind = "nginx-proxy-pass-include"
path = "{REMOTE_HOME}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#
    )
}

fn ssh_django_deploy_config() -> String {
    format!(
        r#"
home = "{REMOTE_HOME}"
poll_seconds = 1
shutdown_timeout_seconds = 5

[slots.blue]
port = {SSH_BLUE_PORT}

[slots.green]
port = {SSH_GREEN_PORT}

[launch]
command = "env PORT={{port}} APP_VERSION={{slot}} STARTUP_DELAY=8 python3 {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/health"
method = "GET"
expected_status = 200
timeout_seconds = 30
interval_millis = 250

[switch]
kind = "nginx-proxy-pass-include"
path = "{REMOTE_HOME}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#
    )
}

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn strip_note_comments(output: &str) -> String {
    output
        .lines()
        .filter(|line| !line.starts_with("# "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn start_http_echo(text: &str, status: u16) -> testcontainers::Container<GenericImage> {
    GenericImage::new("hashicorp/http-echo", "1.0")
        .with_exposed_port(5678.tcp())
        .with_wait_for(WaitFor::message_on_stderr("listening"))
        .with_cmd([
            "-listen=:5678",
            &format!("-text={text}"),
            &format!("-status-code={status}"),
        ])
        .start()
        .unwrap()
}

#[test]
fn ssh_follow_streams_successful_inbox_deployment() {
    require_docker();

    let rig = SshTestRig::start(200);
    rig.start_follow("success", "app-success.jar", 20);
    rig.upload_artifact("app-success.jar", "payload");

    let output = rig.wait_follow("success", Duration::from_secs(30));

    assert!(
        output.success(),
        "follow failed\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
    for phase in [
        "[started]",
        "[imported]",
        "[launching]",
        "[waiting]",
        "[ready]",
        "[switching]",
        "[succeeded]",
    ] {
        assert!(
            output.stdout.contains(phase),
            "missing {phase} in follow output:\n{}",
            output.stdout
        );
    }
}

#[test]
fn ssh_follow_reports_failed_health_and_serve_quarantines_artifact() {
    require_docker();

    let rig = SshTestRig::start(418);
    rig.start_follow("failure", "app-fail.jar", 15);
    rig.upload_artifact("app-fail.jar", "payload");

    let output = rig.wait_follow("failure", Duration::from_secs(25));
    let state = rig.ssh(
        "test ! -f /opt/doubleshot/runtime/active-slot && find /opt/doubleshot/inbox/failed -name '*app-fail.jar' -type f",
    );

    assert!(
        !output.success(),
        "follow unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
    assert!(
        output
            .stdout
            .contains("[failed] HTTP health check did not return 418"),
        "unexpected follow output:\n{}",
        output.stdout
    );
    assert!(
        state.success(),
        "failed artifact/state assertion failed\nstdout:\n{}\nstderr:\n{}",
        state.stdout,
        state.stderr
    );
    assert!(state.stdout.contains("app-fail.jar"));
}

#[test]
fn ssh_status_reflects_remote_deployment_state() {
    require_docker();

    let rig = SshTestRig::start(200);
    rig.upload_artifact("app-status.jar", "payload");
    let follow = rig.follow_artifact("status", "app-status.jar", 20);
    assert!(
        follow.success(),
        "follow failed\nstdout:\n{}\nstderr:\n{}",
        follow.stdout,
        follow.stderr
    );

    let status = rig.ssh(&format!("doubleshot --config {REMOTE_CONFIG} status"));

    assert!(
        status.success(),
        "status failed\nstdout:\n{}\nstderr:\n{}",
        status.stdout,
        status.stderr
    );
    assert!(status.stdout.contains("active slot: blue"));
    assert!(
        status
            .stdout
            .contains("active release: /opt/doubleshot/releases/")
    );
    assert!(status.stdout.contains("app-status.jar"));
    assert!(status.stdout.contains("runtime: /opt/doubleshot/runtime"));
    assert!(status.stdout.contains("blue port=18081 pid="));
    assert!(status.stdout.contains("green port=18082 pid=not running"));
}

#[test]
fn ssh_serve_deploys_two_artifacts_and_flips_slots() {
    require_docker();

    let rig = SshTestRig::start(200);
    rig.upload_artifact("app-blue.jar", "blue");
    let first = rig.follow_artifact("blue", "app-blue.jar", 20);
    rig.upload_artifact("app-green.jar", "green");
    let second = rig.follow_artifact("green", "app-green.jar", 20);
    let state =
        rig.ssh("cat /opt/doubleshot/runtime/active-slot && cat /opt/doubleshot/proxy-pass.inc");

    assert!(
        first.success(),
        "first follow failed\nstdout:\n{}\nstderr:\n{}",
        first.stdout,
        first.stderr
    );
    assert!(
        second.success(),
        "second follow failed\nstdout:\n{}\nstderr:\n{}",
        second.stdout,
        second.stderr
    );
    assert!(
        first
            .stdout
            .contains("[succeeded] blue active on port 18081")
    );
    assert!(
        second
            .stdout
            .contains("[succeeded] green active on port 18082")
    );
    assert!(
        state.success(),
        "state check failed\nstdout:\n{}\nstderr:\n{}",
        state.stdout,
        state.stderr
    );
    assert!(state.stdout.contains("green"));
    assert!(state.stdout.contains("proxy_pass http://127.0.0.1:18082;"));
}

#[test]
fn ssh_follow_times_out_for_missing_artifact() {
    require_docker();

    let rig = SshTestRig::start(200);
    let output = rig.follow_artifact("missing", "missing.jar", 2);

    assert!(
        !output.success(),
        "missing follow unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains("no deployment events arrived within timeout for missing.jar"),
        "unexpected stderr:\n{}\nstdout:\n{}",
        output.stderr,
        output.stdout
    );
}

fn tcp_get(port: u16, path: &str) -> String {
    tcp_get_result(port, path).unwrap()
}

fn tcp_get_result(port: u16, path: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn available_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn nginx_fixture(dir: &Path, proxy_pass: &str) -> PathBuf {
    let root = dir.join("nginx.conf");
    write_file(
        &root,
        &format!(
            r#"
http {{
    server {{
        listen 443 ssl;
        server_name api.example.com;
        location / {{
            limit_req zone=api_limit burst=30 nodelay;
            proxy_pass {proxy_pass};
            proxy_http_version 1.1;
            proxy_set_header Host $host;
        }}
    }}
}}
"#
        ),
    );
    root
}

fn direct_proxy_config_from_port(port: u16) -> (PathBuf, PathBuf) {
    let dir = temp_dir("nginx-direct");
    let root = nginx_fixture(&dir, &format!("http://127.0.0.1:{port}"));
    (dir, root)
}

#[test]
fn nginx_scan_detects_direct_proxy_pass() {
    require_docker();

    let backend = start_http_echo("direct", 200);
    let port = backend.get_host_port_ipv4(5678.tcp()).unwrap();
    let (_dir, root) = direct_proxy_config_from_port(port);

    let output = run_doubleshot(&[
        "init-config",
        "--from-nginx",
        "--nginx-conf",
        root.to_str().unwrap(),
    ]);

    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains(&format!("detected existing backend 127.0.0.1:{port}")));
    assert!(text.contains(&format!("port = {}", port + 1)));
}

#[test]
fn nginx_scan_detects_named_upstream() {
    require_docker();

    let backend = start_http_echo("upstream", 200);
    let port = backend.get_host_port_ipv4(5678.tcp()).unwrap();
    let dir = temp_dir("nginx-upstream");
    let root = dir.join("nginx.conf");
    write_file(
        &root,
        &format!(
            r#"
http {{
    upstream app_backend {{
        server 127.0.0.1:{port};
    }}
    server {{
        server_name app.example.com;
        location / {{
            proxy_pass http://app_backend;
        }}
    }}
}}
"#
        ),
    );

    let output = run_doubleshot(&[
        "init-config",
        "--from-nginx",
        "--nginx-conf",
        root.to_str().unwrap(),
        "--server-name",
        "app.example.com",
    ]);

    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("proxy_pass=http://app_backend"));
    assert!(text.contains(&format!("port = {}", port + 1)));
}

#[test]
fn nginx_scan_follows_conf_d_include() {
    require_docker();

    let backend = start_http_echo("include", 200);
    let port = backend.get_host_port_ipv4(5678.tcp()).unwrap();
    let dir = temp_dir("nginx-include");
    let root = dir.join("nginx.conf");
    write_file(&root, "http {\n    include conf.d/*.conf;\n}\n");
    write_file(
        &dir.join("conf.d/app.conf"),
        &format!(
            r#"
server {{
    server_name include.example.com;
    location / {{
        proxy_pass http://127.0.0.1:{port};
    }}
}}
"#
        ),
    );

    let output = run_doubleshot(&[
        "init-config",
        "--from-nginx",
        "--nginx-conf",
        root.to_str().unwrap(),
    ]);

    assert_success(&output);
    assert!(stdout(&output).contains("conf.d/app.conf"));
}

#[test]
fn nginx_scan_server_name_hint_ranks_target() {
    require_docker();

    let first = start_http_echo("first", 200);
    let wanted = start_http_echo("wanted", 200);
    let first_port = first.get_host_port_ipv4(5678.tcp()).unwrap();
    let wanted_port = wanted.get_host_port_ipv4(5678.tcp()).unwrap();
    let dir = temp_dir("nginx-rank");
    let root = dir.join("nginx.conf");
    write_file(
        &root,
        &format!(
            r#"
http {{
    server {{
        server_name first.example.com;
        location / {{
            proxy_pass http://127.0.0.1:{first_port};
        }}
    }}
    server {{
        server_name wanted.example.com;
        location /api {{
            proxy_pass http://127.0.0.1:{wanted_port};
        }}
    }}
}}
"#
        ),
    );

    let output = run_doubleshot(&[
        "init-config",
        "--from-nginx",
        "--nginx-conf",
        root.to_str().unwrap(),
        "--server-name",
        "wanted.example.com",
    ]);

    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("wanted.example.com"));
    assert!(text.contains(&format!("port = {}", wanted_port + 1)));
}

#[test]
fn nginx_container_validates_generated_proxy_include() {
    require_docker();

    let dir = temp_dir("nginx-validate");
    write_file(
        &dir.join("nginx.conf"),
        r#"
events {}
http {
    server {
        listen 80;
        location / {
            include /etc/nginx/doubleshot/proxy-pass.inc;
        }
    }
}
"#,
    );
    write_file(
        &dir.join("proxy-pass.inc"),
        "proxy_pass http://127.0.0.1:8080;\n",
    );

    let container = GenericImage::new("nginx", "1.27-alpine")
        .with_wait_for(WaitFor::seconds(1))
        .with_mount(Mount::bind_mount(
            dir.to_str().unwrap(),
            "/tmp/doubleshot-nginx",
        ))
        .with_cmd([
            "sh",
            "-c",
            "mkdir -p /etc/nginx/doubleshot && cp /tmp/doubleshot-nginx/proxy-pass.inc /etc/nginx/doubleshot/proxy-pass.inc && nginx -t -c /tmp/doubleshot-nginx/nginx.conf",
        ])
        .start()
        .unwrap();

    drop(container);
}

#[test]
fn nginx_container_proxies_to_http_backend() {
    require_docker();

    let backend = start_http_echo("proxied", 200);
    let backend_port = backend.get_host_port_ipv4(5678.tcp()).unwrap();
    let dir = temp_dir("nginx-proxy");
    write_file(
        &dir.join("nginx.conf"),
        r#"
events {}
http {
    server {
        listen 80;
        location / {
            proxy_pass http://host.testcontainers.internal:BACKEND_PORT;
        }
    }
}
"#
        .replace("BACKEND_PORT", &backend_port.to_string())
        .as_str(),
    );

    let nginx = GenericImage::new("nginx", "1.27-alpine")
        .with_exposed_port(80.tcp())
        .with_wait_for(WaitFor::seconds(2))
        .with_host("host.testcontainers.internal", Host::HostGateway)
        .with_mount(Mount::bind_mount(
            dir.to_str().unwrap(),
            "/tmp/doubleshot-nginx",
        ))
        .with_cmd([
            "nginx",
            "-c",
            "/tmp/doubleshot-nginx/nginx.conf",
            "-g",
            "daemon off;",
        ])
        .start()
        .unwrap();
    let nginx_port = nginx.get_host_port_ipv4(80.tcp()).unwrap();

    let response = tcp_get(nginx_port, "/");

    assert!(response.contains("200 OK"));
    assert!(response.contains("proxied"));
}

#[test]
#[ignore = "stress-style Docker test; run explicitly when validating reload behavior under load"]
fn nginx_reload_under_load_has_no_bad_gateway_responses() {
    require_docker();

    let blue = start_http_echo("blue", 200);
    let green = start_http_echo("green", 200);
    let blue_port = blue.get_host_port_ipv4(5678.tcp()).unwrap();
    let green_port = green.get_host_port_ipv4(5678.tcp()).unwrap();
    let dir = temp_dir("nginx-reload-load");
    write_file(
        &dir.join("nginx.conf"),
        r#"
events {}
http {
    server {
        listen 80;
        location / {
            include /tmp/doubleshot-nginx/proxy-pass.inc;
        }
    }
}
"#,
    );
    write_file(
        &dir.join("proxy-pass.inc"),
        &format!("proxy_pass http://host.testcontainers.internal:{blue_port};\n"),
    );

    let nginx = GenericImage::new("nginx", "1.27-alpine")
        .with_exposed_port(80.tcp())
        .with_wait_for(WaitFor::seconds(2))
        .with_host("host.testcontainers.internal", Host::HostGateway)
        .with_mount(Mount::bind_mount(
            dir.to_str().unwrap(),
            "/tmp/doubleshot-nginx",
        ))
        .with_cmd([
            "nginx",
            "-c",
            "/tmp/doubleshot-nginx/nginx.conf",
            "-g",
            "daemon off;",
        ])
        .start()
        .unwrap();
    let nginx_port = nginx.get_host_port_ipv4(80.tcp()).unwrap();
    assert!(tcp_get(nginx_port, "/").contains("blue"));

    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..16 {
        let stop = Arc::clone(&stop);
        let total = Arc::clone(&total);
        let bad = Arc::clone(&bad);
        workers.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match tcp_get_result(nginx_port, "/") {
                    Ok(response)
                        if response.contains("200 OK")
                            && (response.contains("blue") || response.contains("green")) => {}
                    _ => {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
                total.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    thread::sleep(Duration::from_millis(500));
    write_file(
        &dir.join("proxy-pass.inc"),
        &format!("proxy_pass http://host.testcontainers.internal:{green_port};\n"),
    );
    let reload = Command::new("docker")
        .args(["kill", "--signal=HUP", nginx.id()])
        .output()
        .unwrap();
    assert_success(&reload);
    thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().unwrap();
    }

    assert!(
        total.load(Ordering::Relaxed) > 100,
        "load generator did not issue enough requests"
    );
    assert_eq!(bad.load(Ordering::Relaxed), 0);
    assert!(tcp_get(nginx_port, "/").contains("green"));
}

#[test]
fn init_config_from_nginx_output_is_valid_toml() {
    require_docker();

    let backend = start_http_echo("toml", 200);
    let port = backend.get_host_port_ipv4(5678.tcp()).unwrap();
    let (_dir, root) = direct_proxy_config_from_port(port);

    let output = run_doubleshot(&[
        "init-config",
        "--from-nginx",
        "--nginx-conf",
        root.to_str().unwrap(),
    ]);

    assert_success(&output);
    let parsed: toml::Value = toml::from_str(&strip_note_comments(&stdout(&output))).unwrap();
    assert!(parsed.get("slots").is_some());
    assert!(parsed.get("launch").is_some());
    assert!(parsed.get("health").is_some());
    assert!(parsed.get("switch").is_some());
}

#[test]
fn direct_deploy_promotes_first_slot_against_http_container() {
    require_docker();

    let dir = temp_dir("deploy-success");
    let artifact = dir.join("artifact.txt");
    write_file(&artifact, "payload");
    let config = dir.join("doubleshot.toml");
    write_deploy_config(&config, &dir, 200, 8991, 8992);

    let output = run_doubleshot(&[
        "deploy",
        artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(dir.join("runtime/active-slot")).unwrap(),
        "blue"
    );
    assert!(
        fs::read_to_string(dir.join("runtime/active-release"))
            .unwrap()
            .contains("artifact.txt")
    );
    assert_eq!(
        fs::read_to_string(dir.join("proxy-pass.inc")).unwrap(),
        "proxy_pass http://127.0.0.1:8991;\n"
    );
}

#[test]
fn concurrent_deploy_rejects_second_runner_without_state_drift() {
    let dir = temp_dir("deploy-concurrent");
    let python_available = Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(
        python_available,
        "python3 is required for concurrent deploy tests"
    );

    let first_artifact = dir.join("first.txt");
    let second_artifact = dir.join("second.txt");
    write_file(&first_artifact, "first");
    write_file(&second_artifact, "second");
    let config = dir.join("doubleshot.toml");
    let blue_port = available_port();
    let green_port = available_port();
    write_slow_deploy_config(&config, &dir, blue_port, green_port);

    let first = Command::new(BIN)
        .args([
            "deploy",
            first_artifact.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock_file = dir.join("runtime/deploy.lock");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !lock_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(lock_file.exists(), "first deploy did not acquire the lock");
    let second = run_doubleshot(&[
        "deploy",
        second_artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let first = first.wait_with_output().unwrap();

    assert_success(&first);
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("deployment semaphore is held"),
        "unexpected second deploy stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.join("runtime/active-slot")).unwrap(),
        "blue"
    );
    assert!(
        fs::read_to_string(dir.join("runtime/active-release"))
            .unwrap()
            .contains("first.txt")
    );
    assert_eq!(
        fs::read_to_string(dir.join("proxy-pass.inc")).unwrap(),
        format!("proxy_pass http://127.0.0.1:{blue_port};\n")
    );
    cleanup_runtime_pids(&dir);
}

#[test]
fn http_health_success_accepts_expected_status() {
    require_docker();

    let dir = temp_dir("health-success");
    let artifact = dir.join("artifact.txt");
    write_file(&artifact, "payload");
    let config = dir.join("doubleshot.toml");
    write_deploy_config(&config, &dir, 200, 8993, 8994);

    let output = run_doubleshot(&[
        "deploy",
        artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(dir.join("runtime/active-slot")).unwrap(),
        "blue"
    );
}

#[test]
fn http_health_failure_blocks_promotion() {
    require_docker();

    let dir = temp_dir("health-failure");
    let artifact = dir.join("artifact.txt");
    write_file(&artifact, "payload");
    let config = dir.join("doubleshot.toml");
    write_deploy_config_with_expected(&config, &dir, 200, 201, 8995, 8996);

    let output = run_doubleshot(&[
        "deploy",
        artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);

    assert!(!output.status.success());
    assert!(!dir.join("runtime/active-slot").exists());
    assert!(!dir.join("proxy-pass.inc").exists());
}

#[test]
fn springboot_backend_waits_for_readiness_before_promotion() {
    let backend =
        build_springboot_backend().expect("Spring Boot backend test prerequisites were not met");

    let dir = temp_dir("springboot-deploy");
    let config = dir.join("doubleshot.toml");
    let blue_port = available_port();
    let green_port = available_port();
    write_springboot_deploy_config(&config, &dir, &backend.java, blue_port, green_port);

    let started = Instant::now();
    let output = run_doubleshot(&[
        "deploy",
        backend.artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let elapsed = started.elapsed();

    cleanup_runtime_pids(&dir);
    assert_success(&output);
    assert!(
        elapsed >= Duration::from_secs(9),
        "deployment promoted before the slow Spring Boot startup completed; elapsed={elapsed:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("runtime/active-slot")).unwrap(),
        "blue"
    );
    assert_eq!(
        fs::read_to_string(dir.join("proxy-pass.inc")).unwrap(),
        format!("proxy_pass http://127.0.0.1:{blue_port};\n")
    );

    let response = tcp_get(blue_port, "/");
    assert!(response.contains("200"));
    assert!(response.contains("Hello from blue"));
}

#[test]
fn node_backend_waits_for_readiness_before_promotion() {
    let backend = build_node_backend().expect("Node backend test prerequisites were not met");

    let dir = temp_dir("node-deploy");
    let config = dir.join("doubleshot.toml");
    let blue_port = available_port();
    let green_port = available_port();
    write_node_deploy_config(&config, &dir, &backend.node_modules, blue_port, green_port);

    let started = Instant::now();
    let output = run_doubleshot(&[
        "deploy",
        backend.artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let elapsed = started.elapsed();

    assert_success(&output);
    assert!(
        elapsed >= Duration::from_secs(7),
        "deployment promoted before the slow Node startup completed; elapsed={elapsed:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("runtime/active-slot")).unwrap(),
        "blue"
    );
    assert_eq!(
        fs::read_to_string(dir.join("proxy-pass.inc")).unwrap(),
        format!("proxy_pass http://127.0.0.1:{blue_port};\n")
    );

    let response = tcp_get(blue_port, "/");
    assert!(response.contains("200"));
    assert!(response.contains("Hello from DoubleShot!"));
    assert!(response.contains("blue"));

    cleanup_runtime_pids(&dir);
}

#[test]
fn axum_backend_waits_for_readiness_before_promotion() {
    let backend = build_axum_backend().expect("Axum backend test prerequisites were not met");

    let dir = temp_dir("axum-deploy");
    let config = dir.join("doubleshot.toml");
    let blue_port = available_port();
    let green_port = available_port();
    write_axum_deploy_config(&config, &dir, blue_port, green_port);

    let started = Instant::now();
    let output = run_doubleshot(&[
        "deploy",
        backend.artifact.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let elapsed = started.elapsed();

    assert_success(&output);
    assert!(
        elapsed >= Duration::from_secs(7),
        "deployment promoted before the slow Axum startup completed; elapsed={elapsed:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("runtime/active-slot")).unwrap(),
        "blue"
    );
    assert_eq!(
        fs::read_to_string(dir.join("proxy-pass.inc")).unwrap(),
        format!("proxy_pass http://127.0.0.1:{blue_port};\n")
    );

    let response = tcp_get(blue_port, "/");
    assert!(response.contains("200"));
    assert!(response.contains("Hello from version: blue"));

    cleanup_runtime_pids(&dir);
}

#[test]
fn django_backend_waits_for_readiness_before_promotion() {
    require_docker();

    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends/django/app.py");
    let rig = SshTestRig::start_empty();
    rig.write_remote_config(&ssh_django_deploy_config());
    rig.upload_artifact("django-app.py", &fs::read_to_string(app).unwrap());

    let deploy_command =
        format!("doubleshot --config {REMOTE_CONFIG} deploy {REMOTE_HOME}/inbox/django-app.py");
    let started = Instant::now();
    let output = rig.ssh(&deploy_command);
    let elapsed = started.elapsed();

    assert!(
        output.success(),
        "Django deploy failed\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
    assert!(
        elapsed >= Duration::from_secs(7),
        "deployment promoted before the slow Django startup completed; elapsed={elapsed:?}"
    );
    assert_eq!(
        rig.ssh(&format!("cat {REMOTE_HOME}/runtime/active-slot"))
            .stdout
            .trim(),
        "blue"
    );
    assert_eq!(
        rig.ssh(&format!("cat {REMOTE_HOME}/proxy-pass.inc")).stdout,
        format!("proxy_pass http://127.0.0.1:{SSH_BLUE_PORT};\n")
    );

    let response = rig.ssh(&format!(
        "python3 -c 'import urllib.request; print(urllib.request.urlopen(\"http://127.0.0.1:{SSH_BLUE_PORT}/\").read().decode(), end=\"\")'"
    ));
    assert!(
        response.success(),
        "Django response check failed\nstdout:\n{}\nstderr:\n{}",
        response.stdout,
        response.stderr
    );
    assert!(response.stdout.contains("Hello from version: blue"));
}

struct SpringbootBackend {
    artifact: PathBuf,
    java: PathBuf,
}

struct NodeBackend {
    artifact: PathBuf,
    node_modules: PathBuf,
}

struct AxumBackend {
    artifact: PathBuf,
}

fn build_axum_backend() -> Option<AxumBackend> {
    let app_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends/axum");
    let output = Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(&app_dir)
        .output();

    let Ok(output) = output else {
        eprintln!("cannot run Axum backend test because cargo was not found on PATH");
        return None;
    };

    if !output.status.success() {
        eprintln!(
            "cannot run Axum backend test because cargo build failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let artifact = app_dir.join("target/debug/axum");
    if !artifact.is_file() {
        eprintln!(
            "cannot run Axum backend test because cargo did not produce {}",
            artifact.display()
        );
        return None;
    }

    Some(AxumBackend { artifact })
}

fn build_node_backend() -> Option<NodeBackend> {
    if Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("cannot run Node backend test because node was not found on PATH");
        return None;
    }

    let app_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends/node");
    let node_modules = app_dir.join("node_modules");
    if !node_modules.join("express").is_dir() {
        let output = Command::new("npm")
            .arg("ci")
            .arg("--ignore-scripts")
            .current_dir(&app_dir)
            .output();

        let Ok(output) = output else {
            eprintln!("cannot run Node backend test because npm was not found on PATH");
            return None;
        };

        if !output.status.success() {
            eprintln!(
                "cannot run Node backend test because npm ci failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
    }

    Some(NodeBackend {
        artifact: app_dir.join("app.js"),
        node_modules,
    })
}

fn build_springboot_backend() -> Option<SpringbootBackend> {
    let demo = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends/springboot-jre25/demo");

    let output = Command::new("./gradlew")
        .args(["bootJar", "printJavaLauncher"])
        .arg("--quiet")
        .current_dir(&demo)
        .output()
        .unwrap_or_else(|err| panic!("failed to run Spring Boot Gradle wrapper: {err}"));

    if !output.status.success() {
        eprintln!(
            "cannot run Spring Boot backend test because bootJar failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let java = stdout(&output)
        .lines()
        .last()
        .map(PathBuf::from)
        .filter(|path| path.is_file());

    let Some(java) = java else {
        eprintln!(
            "cannot run Spring Boot backend test because Gradle did not report a Java 25 launcher\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    };

    Some(SpringbootBackend {
        artifact: demo.join("build/libs/demo-0.0.1-SNAPSHOT.jar"),
        java,
    })
}

fn write_deploy_config(path: &Path, dir: &Path, status: u16, blue_port: u16, green_port: u16) {
    write_deploy_config_with_expected(path, dir, status, status, blue_port, green_port);
}

fn write_slow_deploy_config(path: &Path, dir: &Path, blue_port: u16, green_port: u16) {
    write_file(
        path,
        &format!(
            r#"
home = "{home}"
poll_seconds = 1
shutdown_timeout_seconds = 1

[slots.blue]
port = {blue_port}

[slots.green]
port = {green_port}

[launch]
command = "sh -c 'sleep 2; exec python3 -m http.server {{port}} --bind 127.0.0.1 --directory {home}' # {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/"
method = "GET"
expected_status = 200
timeout_seconds = 8
interval_millis = 100

[switch]
kind = "nginx-proxy-pass-include"
path = "{home}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#,
            home = dir.display(),
        ),
    );
}

fn write_deploy_config_with_expected(
    path: &Path,
    dir: &Path,
    status: u16,
    expected_status: u16,
    blue_port: u16,
    green_port: u16,
) {
    write_file(
        path,
        &format!(
            r#"
home = "{home}"
poll_seconds = 1
shutdown_timeout_seconds = 1

[slots.blue]
port = {blue_port}

[slots.green]
port = {green_port}

[launch]
command = "python3 -m http.server {{port}} --bind 127.0.0.1 --directory {home} # {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/"
method = "GET"
expected_status = {expected_status}
timeout_seconds = 5
interval_millis = 100

[switch]
kind = "nginx-proxy-pass-include"
path = "{home}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#,
            home = dir.display(),
        ),
    );

    if status != 200 {
        write_file(
            &dir.join("index.html"),
            &format!("<html><body>status {status}</body></html>\n"),
        );
    }
}

fn write_springboot_deploy_config(
    path: &Path,
    dir: &Path,
    java: &Path,
    blue_port: u16,
    green_port: u16,
) {
    write_file(
        path,
        &format!(
            r#"
home = "{home}"
poll_seconds = 1
shutdown_timeout_seconds = 5

[slots.blue]
port = {blue_port}

[slots.green]
port = {green_port}

[launch]
command = "env APP_VERSION={{slot}} {java} -Dserver.port={{port}} -jar {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/actuator/health/readiness"
method = "GET"
expected_status = 200
timeout_seconds = 45
interval_millis = 250

[switch]
kind = "nginx-proxy-pass-include"
path = "{home}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#,
            home = dir.display(),
            java = shell_quote(java),
        ),
    );
}

fn write_node_deploy_config(
    path: &Path,
    dir: &Path,
    node_modules: &Path,
    blue_port: u16,
    green_port: u16,
) {
    write_file(
        path,
        &format!(
            r#"
home = "{home}"
poll_seconds = 1
shutdown_timeout_seconds = 5

[slots.blue]
port = {blue_port}

[slots.green]
port = {green_port}

[launch]
command = "env PORT={{port}} VERSION={{slot}} STARTUP_DELAY_MS=8000 NODE_PATH={node_modules} node {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/health"
method = "GET"
expected_status = 200
timeout_seconds = 30
interval_millis = 250

[switch]
kind = "nginx-proxy-pass-include"
path = "{home}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#,
            home = dir.display(),
            node_modules = shell_quote(node_modules),
        ),
    );
}

fn write_axum_deploy_config(path: &Path, dir: &Path, blue_port: u16, green_port: u16) {
    write_file(
        path,
        &format!(
            r#"
home = "{home}"
poll_seconds = 1
shutdown_timeout_seconds = 5

[slots.blue]
port = {blue_port}

[slots.green]
port = {green_port}

[launch]
command = "env PORT={{port}} APP_VERSION={{slot}} STARTUP_DELAY=8 {{artifact}}"
env_files = []

[health]
kind = "http"
url = "http://127.0.0.1:{{port}}/health"
method = "GET"
expected_status = 200
timeout_seconds = 30
interval_millis = 250

[switch]
kind = "nginx-proxy-pass-include"
path = "{home}/proxy-pass.inc"
reload_command = "true"
host = "127.0.0.1"
"#,
            home = dir.display(),
        ),
    );
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn cleanup_runtime_pids(dir: &Path) {
    for slot in ["blue", "green"] {
        let pid_file = dir.join(format!("runtime/{slot}.pid"));
        let Ok(pid) = fs::read_to_string(&pid_file) else {
            continue;
        };
        let pid = pid.trim();
        if !pid.is_empty() {
            let _ = Command::new("kill").arg("-TERM").arg(pid).status();
        }
        let _ = fs::remove_file(pid_file);
    }
}
