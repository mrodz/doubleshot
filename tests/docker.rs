#![cfg(feature = "docker-tests")]

use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
    time::{SystemTime, UNIX_EPOCH},
};

use testcontainers::{
    GenericImage, ImageExt,
    core::{Host, IntoContainerPort, Mount, WaitFor},
    runners::SyncRunner,
};

const BIN: &str = env!("CARGO_BIN_EXE_doubleshot");

#[derive(Clone, Debug)]
enum DockerAvailability {
    Available,
    Unavailable(String),
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

            match GenericImage::new("alpine", "3.20")
            .with_wait_for(WaitFor::seconds(1))
            .with_cmd(["sh", "-c", "true"])
            .start()
            {
                Ok(_) => DockerAvailability::Available,
                Err(err) => DockerAvailability::Unavailable(format!(
                    "Docker daemon is reachable, but testcontainers could not start a probe container: {err}"
                )),
            }
        })
        .clone()
}

fn skip_without_docker() -> bool {
    match docker_availability() {
        DockerAvailability::Available => false,
        DockerAvailability::Unavailable(reason) => {
            eprintln!("skipping docker-backed assertion: {reason}");
            true
        }
    }
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

fn tcp_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
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
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
fn init_config_from_nginx_output_is_valid_toml() {
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
fn http_health_success_accepts_expected_status() {
    if skip_without_docker() {
        return;
    }

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
    if skip_without_docker() {
        return;
    }

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
    let Some(backend) = build_springboot_backend() else {
        return;
    };

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
    let Some(backend) = build_node_backend() else {
        return;
    };

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
    let Some(backend) = build_axum_backend() else {
        return;
    };

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
    let Some(backend) = build_django_backend() else {
        return;
    };

    let dir = temp_dir("django-deploy");
    let config = dir.join("doubleshot.toml");
    let blue_port = available_port();
    let green_port = available_port();
    write_django_deploy_config(&config, &dir, &backend.python, blue_port, green_port);

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
        "deployment promoted before the slow Django startup completed; elapsed={elapsed:?}"
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

struct DjangoBackend {
    artifact: PathBuf,
    python: PathBuf,
}

fn build_django_backend() -> Option<DjangoBackend> {
    let app_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends/django");
    let artifact = app_dir.join("app.py");
    if !artifact.is_file() {
        eprintln!(
            "skipping Django backend test because {} does not exist",
            artifact.display()
        );
        return None;
    }

    let python = python3_path()?;
    let output = Command::new(&python)
        .args(["-c", "import django, uvicorn"])
        .output();

    let Ok(output) = output else {
        eprintln!("skipping Django backend test because python3 was not runnable");
        return None;
    };

    if !output.status.success() {
        eprintln!(
            "skipping Django backend test because django or uvicorn is not installed for {}\nstderr:\n{}",
            python.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    Some(DjangoBackend { artifact, python })
}

fn python3_path() -> Option<PathBuf> {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output();

    let Ok(output) = output else {
        eprintln!("skipping Django backend test because python3 was not found on PATH");
        return None;
    };

    if !output.status.success() {
        eprintln!(
            "skipping Django backend test because python3 failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    stdout(&output)
        .lines()
        .last()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn build_axum_backend() -> Option<AxumBackend> {
    let app_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends/axum");
    let output = Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(&app_dir)
        .output();

    let Ok(output) = output else {
        eprintln!("skipping Axum backend test because cargo was not found on PATH");
        return None;
    };

    if !output.status.success() {
        eprintln!(
            "skipping Axum backend test because cargo build failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let artifact = app_dir.join("target/debug/axum");
    if !artifact.is_file() {
        eprintln!(
            "skipping Axum backend test because cargo did not produce {}",
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
        eprintln!("skipping Node backend test because node was not found on PATH");
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
            eprintln!("skipping Node backend test because npm was not found on PATH");
            return None;
        };

        if !output.status.success() {
            eprintln!(
                "skipping Node backend test because npm ci failed\nstdout:\n{}\nstderr:\n{}",
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
            "skipping Spring Boot backend test because bootJar failed\nstdout:\n{}\nstderr:\n{}",
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
            "skipping Spring Boot backend test because Gradle did not report a Java 25 launcher\nstdout:\n{}\nstderr:\n{}",
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

fn write_django_deploy_config(
    path: &Path,
    dir: &Path,
    python: &Path,
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
command = "env PORT={{port}} APP_VERSION={{slot}} STARTUP_DELAY=8 {python} {{artifact}}"
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
            python = shell_quote(python),
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
