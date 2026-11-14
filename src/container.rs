use std::collections::HashMap;
use std::default::Default;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::models::HostConfig;
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use bollard::Docker;
use futures_util::StreamExt;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::config::{AgentConfig, Settings, WorkspaceMode};

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    async fn ensure_network(&self) -> Result<()>;
    async fn ensure_agent(&self, agent: &AgentConfig) -> Result<String>;
    async fn exec(
        &self,
        agent_name: &str,
        cmd: Vec<String>,
        timeout_sec: u64,
    ) -> Result<ExecResult>;
    async fn list_agent_containers(&self) -> Result<Vec<(String, String)>>;
    async fn shutdown_all(&self) -> Result<()>;
}

pub struct DockerRuntime {
    docker: Docker,
    settings: Settings,
}

impl DockerRuntime {
    pub fn connect(settings: Settings) -> Result<Self> {
        let docker = connect_docker().context(
            "connecting to Docker/OrbStack (is the daemon running? try: docker context use orbstack)",
        )?;
        Ok(Self { docker, settings })
    }

    fn container_name(&self, agent: &str) -> String {
        self.settings.container_name(agent)
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        let exists = self.docker.inspect_image(image).await.is_ok();
        if exists {
            return Ok(());
        }
        warn!(%image, "image missing locally; attempting pull");
        let options = Some(CreateImageOptions {
            from_image: image.split(':').next().unwrap_or(image),
            tag: image.split(':').nth(1).unwrap_or("latest"),
            ..Default::default()
        });
        let mut stream = self.docker.create_image(options, None, None);
        while let Some(item) = stream.next().await {
            item.context("pulling image")?;
        }
        Ok(())
    }

    async fn remove_if_exists(&self, name: &str) -> Result<()> {
        let _ = self
            .docker
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        Ok(())
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn ensure_network(&self) -> Result<()> {
        let networks = self
            .docker
            .list_networks(Some(ListNetworksOptions::<String> {
                filters: HashMap::from([(
                    "name".to_string(),
                    vec![self.settings.docker_network.clone()],
                )]),
                ..Default::default()
            }))
            .await?;
        if networks.is_empty() {
            info!(network = %self.settings.docker_network, "creating docker network");
            self.docker
                .create_network(CreateNetworkOptions {
                    name: self.settings.docker_network.clone(),
                    driver: "bridge".into(),
                    check_duplicate: true,
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    }

    async fn ensure_agent(&self, agent: &AgentConfig) -> Result<String> {
        let name = self.container_name(&agent.name);
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                filters: HashMap::from([("name".into(), vec![format!("/{name}")]) ]),
                ..Default::default()
            }))
            .await?;

        if let Some(existing) = list.first() {
            if let Some(id) = &existing.id {
                if existing.state.as_deref() != Some("running") {
                    self.docker
                        .start_container(id, None::<StartContainerOptions<String>>)
                        .await?;
                }
                return Ok(name);
            }
        }

        self.ensure_image(&agent.image).await?;

        let workspace = self.settings.workspace_dir.display().to_string();
        let mode = agent.workspace_mode.as_bind_mode();
        let binds = vec![
            format!("{workspace}:/workspace:{mode}"),
            format!("stubot-scratch-{}:/tmp/agent_scratch:rw", agent.name),
        ];

        let env = vec![
            format!("AGENT_NAME={}", agent.name),
            "HOME=/tmp/agent_scratch".into(),
            format!("AGENT_NETWORK={}", agent.network),
        ];

        let host_config = HostConfig {
            binds: Some(binds),
            network_mode: Some(self.settings.docker_network.clone()),
            // readonly rootfs would break many tools; rely on volume mode
            ..Default::default()
        };

        // If image missing and pull failed, create may fail — surface clearly
        self.remove_if_exists(&name).await?;

        let config = Config {
            image: Some(agent.image.clone()),
            hostname: Some(format!("agent-{}", agent.name)),
            working_dir: Some("/workspace".into()),
            env: Some(env),
            cmd: Some(vec!["sleep".into(), "infinity".into()]),
            host_config: Some(host_config),
            tty: Some(false),
            open_stdin: Some(false),
            labels: Some(HashMap::from([
                ("stubot".into(), "1".into()),
                ("stubot.agent".into(), agent.name.clone()),
            ])),
            ..Default::default()
        };

        info!(%name, image = %agent.image, "creating agent container");
        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.as_str(),
                    platform: None,
                }),
                config,
            )
            .await
            .with_context(|| format!("creating container {name} (build images via scripts/build-images.sh)"))?;

        self.docker
            .start_container(&name, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("starting container {name}"))?;

        Ok(name)
    }

    async fn exec(
        &self,
        agent_name: &str,
        cmd: Vec<String>,
        timeout_sec: u64,
    ) -> Result<ExecResult> {
        let name = self.container_name(agent_name);
        let exec = self
            .docker
            .create_exec(
                &name,
                CreateExecOptions {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(cmd),
                    working_dir: Some("/workspace".into()),
                    ..Default::default()
                },
            )
            .await?;

        let start = self.docker.start_exec(&exec.id, None);
        let result = timeout(Duration::from_secs(timeout_sec), async {
            match start.await? {
                StartExecResults::Attached { mut output, .. } => {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    while let Some(Ok(chunk)) = output.next().await {
                        match chunk {
                            bollard::container::LogOutput::StdOut { message } => {
                                stdout.push_str(&String::from_utf8_lossy(&message));
                            }
                            bollard::container::LogOutput::StdErr { message } => {
                                stderr.push_str(&String::from_utf8_lossy(&message));
                            }
                            _ => {}
                        }
                    }
                    let inspect = self.docker.inspect_exec(&exec.id).await?;
                    let exit_code = inspect.exit_code.unwrap_or(-1);
                    Ok::<_, anyhow::Error>(ExecResult {
                        exit_code,
                        stdout,
                        stderr,
                    })
                }
                StartExecResults::Detached => Err(anyhow!("exec detached unexpectedly")),
            }
        })
        .await
        .map_err(|_| anyhow!("exec timed out after {timeout_sec}s"))??;

        Ok(result)
    }

    async fn list_agent_containers(&self) -> Result<Vec<(String, String)>> {
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                filters: HashMap::from([("label".into(), vec!["stubot=1".into()])]),
                ..Default::default()
            }))
            .await?;
        Ok(list
            .into_iter()
            .map(|c| {
                let name = c
                    .names
                    .and_then(|n| n.first().cloned())
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string();
                let state = c.state.unwrap_or_else(|| "unknown".into());
                (name, state)
            })
            .collect())
    }

    async fn shutdown_all(&self) -> Result<()> {
        let list = self.list_agent_containers().await?;
        for (name, _) in list {
            let _ = self
                .docker
                .remove_container(
                    &name,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            info!(%name, "removed agent container");
        }
        Ok(())
    }
}

/// No-op runtime for `--skip-containers` / unit tests.
pub struct NullRuntime;

#[async_trait]
impl ContainerRuntime for NullRuntime {
    async fn ensure_network(&self) -> Result<()> {
        Ok(())
    }
    async fn ensure_agent(&self, agent: &AgentConfig) -> Result<String> {
        Ok(format!("null-{}", agent.name))
    }
    async fn exec(
        &self,
        agent_name: &str,
        cmd: Vec<String>,
        _timeout_sec: u64,
    ) -> Result<ExecResult> {
        Ok(ExecResult {
            exit_code: 0,
            stdout: format!("[null-runtime] {agent_name}: {}", cmd.join(" ")),
            stderr: String::new(),
        })
    }
    async fn list_agent_containers(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![])
    }
    async fn shutdown_all(&self) -> Result<()> {
        Ok(())
    }
}

pub type SharedRuntime = Arc<dyn ContainerRuntime>;

pub fn build_runtime(settings: &Settings) -> Result<SharedRuntime> {
    if settings.skip_containers {
        Ok(Arc::new(NullRuntime))
    } else {
        Ok(Arc::new(DockerRuntime::connect(settings.clone())?))
    }
}

fn connect_docker() -> Result<Docker> {
    // Prefer explicit DOCKER_HOST, then OrbStack, then Docker Desktop, then defaults.
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if !host.is_empty() {
            return Docker::connect_with_local_defaults()
                .or_else(|_| Docker::connect_with_http_defaults())
                .context(format!("DOCKER_HOST={host}"));
        }
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let candidates = [
        format!("{home}/.orbstack/run/docker.sock"),
        format!("{home}/.docker/run/docker.sock"),
        "/var/run/docker.sock".to_string(),
    ];

    for sock in candidates {
        let path = Path::new(&sock);
        if path.exists() {
            let url = format!("unix://{sock}");
            match Docker::connect_with_unix(&sock, 120, bollard::API_DEFAULT_VERSION) {
                Ok(d) => {
                    info!(%url, "connected to docker");
                    return Ok(d);
                }
                Err(e) => warn!(%url, error = %e, "docker socket failed"),
            }
        }
    }

    Docker::connect_with_local_defaults().context("no usable docker socket found")
}

#[allow(dead_code)]
pub fn workspace_bind(workspace: &Path, mode: WorkspaceMode) -> String {
    format!(
        "{}:/workspace:{}",
        workspace.display(),
        mode.as_bind_mode()
    )
}
