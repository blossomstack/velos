//! Explicit local backend selection. Existing workers remain container-only.
use crate::native::{NativeConfig, NativeProcess};
use crate::{
    AppleContainer, ContainerRuntime, Instance, InstanceId, RunSpec, RuntimeError, Workload,
    WorkloadHealth,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "config", deny_unknown_fields)]
pub enum RuntimeConfig {
    #[default]
    AppleContainer,
    NativeProcess(NativeConfig),
    Mixed(NativeConfig),
}

impl RuntimeConfig {
    pub fn is_container(&self) -> bool {
        matches!(self, Self::AppleContainer)
    }
    pub fn build(&self) -> Result<Arc<dyn ContainerRuntime>, RuntimeError> {
        match self {
            Self::AppleContainer => Ok(Arc::new(AppleContainer::new())),
            Self::NativeProcess(config) => Ok(Arc::new(NativeProcess::new(config.clone())?)),
            Self::Mixed(config) => Ok(Arc::new(MixedRuntime::new(
                Arc::new(AppleContainer::new()),
                NativeProcess::new(config.clone())?,
            ))),
        }
    }
}

pub struct MixedRuntime {
    container: Arc<dyn ContainerRuntime>,
    native: NativeProcess,
}
impl MixedRuntime {
    pub fn new(container: Arc<dyn ContainerRuntime>, native: NativeProcess) -> Self {
        Self { container, native }
    }
}
#[async_trait]
impl ContainerRuntime for MixedRuntime {
    async fn run(&self, spec: &RunSpec) -> Result<InstanceId, RuntimeError> {
        match &spec.workload {
            Workload::Container { .. } => {
                if self.native.contains(&spec.uid).await {
                    return Err(RuntimeError::Command(
                        "uid already owned by native runtime".into(),
                    ));
                }
                self.container.run(spec).await
            }
            Workload::NativeProcess { .. } => {
                if self
                    .container
                    .list()
                    .await?
                    .iter()
                    .any(|i| i.uid == spec.uid)
                {
                    return Err(RuntimeError::Command(
                        "uid already owned by container runtime".into(),
                    ));
                }
                self.native.run(spec).await
            }
        }
    }
    async fn stop(&self, uid: &str) -> Result<(), RuntimeError> {
        if self.native.contains(uid).await {
            self.native.stop(uid).await
        } else {
            self.container.stop(uid).await
        }
    }
    async fn start(&self, uid: &str) -> Result<InstanceId, RuntimeError> {
        if self.native.contains(uid).await {
            self.native.start(uid).await
        } else {
            self.container.start(uid).await
        }
    }
    async fn remove(&self, uid: &str) -> Result<(), RuntimeError> {
        if self.native.contains(uid).await {
            self.native.remove(uid).await
        } else {
            self.container.remove(uid).await
        }
    }
    async fn list(&self) -> Result<Vec<Instance>, RuntimeError> {
        let mut instances = self.container.list().await?;
        for native in self.native.list().await? {
            if instances.iter().any(|i| i.uid == native.uid) {
                return Err(RuntimeError::Command(
                    "uid appears in both runtimes; refusing ambiguous ownership".into(),
                ));
            }
            instances.push(native);
        }
        Ok(instances)
    }
    async fn health(&self) -> Result<Vec<WorkloadHealth>, RuntimeError> {
        self.native.health().await
    }
    async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.native.shutdown().await
    }
    async fn version(&self) -> Result<String, RuntimeError> {
        Ok(format!(
            "{} + {}",
            self.container.version().await?,
            self.native.version().await?
        ))
    }
    async fn ensure_ready(&self) -> Result<(), RuntimeError> {
        self.container.ensure_ready().await?;
        self.native.ensure_ready().await
    }
}
