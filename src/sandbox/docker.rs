//! Docker implementation of the [`SandboxBackend`] trait.
//!
//! Wraps the existing [`ContainerRunner`] for ephemeral execution and provides
//! persistent instance lifecycle management via the bollard Docker API.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use bollard::container::{
    CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::models::HostConfig;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::sandbox::backend::{
    InstanceBind, STOP_GRACE_PERIOD, SandboxBackend, SandboxBackendKind,
};
use crate::sandbox::config::{ResourceLimits, SandboxPolicy};
use crate::sandbox::container::{ContainerOutput, ContainerRunner, connect_docker};
use crate::sandbox::error::{Result, SandboxError};

/// Docker-based sandbox backend.
///
/// Delegates ephemeral execution to [`ContainerRunner`] and manages persistent
/// container instances via the bollard Docker API.
pub struct DockerBackend {
    image: String,
    proxy_port: u16,
    docker: RwLock<Option<bollard::Docker>>,
}

impl DockerBackend {
    /// Create a new Docker backend.
    pub fn new(image: String, proxy_port: u16) -> Self {
        Self {
            image,
            proxy_port,
            docker: RwLock::new(None),
        }
    }

    /// Get or create a cached Docker connection.
    async fn docker(&self) -> Result<bollard::Docker> {
        {
            let guard = self.docker.read().await;
            if let Some(ref d) = *guard {
                return Ok(d.clone());
            }
        }
        let docker = connect_docker().await?;
        *self.docker.write().await = Some(docker.clone());
        Ok(docker)
    }

    /// Create a `ContainerRunner` from the cached Docker connection.
    async fn runner(&self) -> Result<ContainerRunner> {
        let docker = self.docker().await?;
        Ok(ContainerRunner::new(
            docker,
            self.image.clone(),
            self.proxy_port,
        ))
    }

    /// Detect the host address containers should use to reach the orchestrator.
    fn detect_orchestrator_host() -> &'static str {
        if cfg!(target_os = "linux") {
            "172.17.0.1"
        } else {
            "host.docker.internal"
        }
    }
}

#[async_trait]
impl SandboxBackend for DockerBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::Docker
    }

    async fn is_available(&self) -> bool {
        match self.docker().await {
            Ok(d) => d.ping().await.is_ok(),
            Err(_) => false,
        }
    }

    async fn image_exists(&self) -> bool {
        match self.runner().await {
            Ok(r) => r.image_exists().await,
            Err(_) => false,
        }
    }

    async fn pull_image(&self) -> Result<()> {
        self.runner().await?.pull_image().await
    }

    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        policy: SandboxPolicy,
        limits: &ResourceLimits,
        env: HashMap<String, String>,
    ) -> Result<ContainerOutput> {
        self.runner()
            .await?
            .execute(command, working_dir, policy, limits, env)
            .await
    }

    async fn create_instance(
        &self,
        job_id: Uuid,
        entrypoint: Vec<String>,
        env: Vec<(String, String)>,
        binds: Vec<InstanceBind>,
        limits: &ResourceLimits,
    ) -> Result<String> {
        let docker = self.docker().await?;

        let container_name = format!("ironclaw-job-{job_id}");

        // Build environment variables list
        let env_list: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();

        // Build bind mounts
        let bind_list: Vec<String> = binds
            .iter()
            .map(|b| {
                let mode = if b.writable { "rw" } else { "ro" };
                format!("{}:{}:{}", b.host_path, b.guest_path, mode)
            })
            .collect();

        let host_config = HostConfig {
            binds: if bind_list.is_empty() {
                None
            } else {
                Some(bind_list)
            },
            memory: Some((limits.memory_bytes) as i64),
            cpu_shares: Some(limits.cpu_shares as i64),
            network_mode: Some("bridge".to_string()),
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
            cap_drop: Some(vec!["ALL".to_string()]),
            cap_add: Some(vec!["CHOWN".to_string()]),
            security_opt: Some(vec!["no-new-privileges:true".to_string()]),
            tmpfs: Some(
                [("/tmp".to_string(), "size=512M".to_string())]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        };

        let config = bollard::container::Config {
            image: Some(self.image.clone()),
            cmd: Some(entrypoint),
            env: Some(env_list),
            user: Some("1000:1000".to_string()),
            working_dir: Some("/workspace".to_string()),
            host_config: Some(host_config),
            ..Default::default()
        };

        let response = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name,
                    platform: None,
                }),
                config,
            )
            .await
            .map_err(|e| SandboxError::ContainerCreationFailed {
                reason: e.to_string(),
            })?;

        Ok(response.id)
    }

    async fn start_instance(&self, instance_id: &str) -> Result<()> {
        let docker = self.docker().await?;
        docker
            .start_container(instance_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| SandboxError::ContainerStartFailed {
                reason: e.to_string(),
            })
    }

    async fn stop_instance(&self, instance_id: &str) -> Result<()> {
        let docker = self.docker().await?;

        // Graceful stop with timeout
        let _ = docker
            .stop_container(
                instance_id,
                Some(StopContainerOptions {
                    t: STOP_GRACE_PERIOD.as_secs() as i64,
                }),
            )
            .await;

        // Force remove
        let _ = docker
            .remove_container(
                instance_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;

        Ok(())
    }

    fn orchestrator_host(&self) -> &str {
        Self::detect_orchestrator_host()
    }
}
