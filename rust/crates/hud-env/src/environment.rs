//! The environment declaration: identity, capabilities, and task templates.

use crate::task::Template;
use futures::future::BoxFuture;
use hud_types::Capability;
use indexmap::IndexMap;
use std::sync::Arc;

pub type HookError = Box<dyn std::error::Error + Send + Sync>;

type StartHook =
    Box<dyn FnOnce() -> BoxFuture<'static, Result<Vec<Capability>, HookError>> + Send + Sync>;
type StopHook = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("unknown capability: '{0}'")]
    UnknownCapability(String),
    #[error("environment start hook failed: {0}")]
    StartHook(#[source] HookError),
}

/// Capabilities + tasks dispatched over the HUD wire protocol.
///
/// A pure declaration between `new` and [`Environment::start`]: it holds no
/// runtime state beyond registered hooks. Serving (`hud_env::serve`) runs the
/// start hooks — so hook-published capabilities (a workspace's SSH address) are
/// concrete by the time any client sends `hello` — and the stop hooks on the
/// way down.
pub struct Environment {
    pub name: String,
    pub version: String,
    capabilities: Vec<Capability>,
    templates: IndexMap<String, Arc<dyn Template>>,
    on_start: Vec<StartHook>,
    // Interior-mutable so a served (Arc-shared) environment can still run its
    // stop hooks on teardown.
    on_stop: std::sync::Mutex<Vec<StopHook>>,
    started: bool,
}

impl Environment {
    pub fn new(name: impl Into<String>) -> Environment {
        Environment {
            name: name.into(),
            version: "0.0.1".to_string(),
            capabilities: Vec::new(),
            templates: IndexMap::new(),
            on_start: Vec::new(),
            on_stop: std::sync::Mutex::new(Vec::new()),
            started: false,
        }
    }

    pub fn version(mut self, version: impl Into<String>) -> Environment {
        self.version = version.into();
        self
    }

    /// Publish a capability (concrete wire data a client can dial).
    pub fn add_capability(&mut self, capability: Capability) {
        self.capabilities.push(capability);
    }

    /// Builder form of [`Environment::add_capability`].
    pub fn capability(mut self, capability: Capability) -> Environment {
        self.add_capability(capability);
        self
    }

    /// Register a task template.
    pub fn add_template(&mut self, template: impl Template + 'static) {
        self.templates
            .insert(template.id().to_string(), Arc::new(template));
    }

    /// Builder form of [`Environment::add_template`].
    pub fn template(mut self, template: impl Template + 'static) -> Environment {
        self.add_template(template);
        self
    }

    /// Register a start hook: runs before serving, and may publish
    /// capabilities for daemons it brings up (the workspace pattern).
    pub fn on_start<F, Fut>(&mut self, hook: F)
    where
        F: FnOnce() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Vec<Capability>, HookError>> + Send + 'static,
    {
        self.on_start.push(Box::new(move || Box::pin(hook())));
    }

    /// Register a stop hook: runs (in reverse order) when serving ends.
    pub fn on_stop<F, Fut>(&mut self, hook: F)
    where
        F: FnOnce() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.on_stop
            .lock()
            .expect("stop hooks lock")
            .push(Box::new(move || Box::pin(hook())));
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// Look up a published capability by name.
    pub fn find_capability(&self, name: &str) -> Result<&Capability, EnvironmentError> {
        self.capabilities
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| EnvironmentError::UnknownCapability(name.to_string()))
    }

    pub fn templates(&self) -> impl Iterator<Item = &Arc<dyn Template>> {
        self.templates.values()
    }

    pub fn find_template(&self, id: &str) -> Option<&Arc<dyn Template>> {
        self.templates.get(id)
    }

    /// Run start hooks (idempotent); hook-published capabilities are appended.
    pub async fn start(&mut self) -> Result<(), EnvironmentError> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        for hook in self.on_start.drain(..) {
            let published = hook().await.map_err(EnvironmentError::StartHook)?;
            self.capabilities.extend(published);
        }
        Ok(())
    }

    /// Run stop hooks in reverse registration order (idempotent, best-effort).
    pub async fn stop(&self) {
        let hooks: Vec<StopHook> = {
            let mut guard = self.on_stop.lock().expect("stop hooks lock");
            guard.drain(..).rev().collect()
        };
        for hook in hooks {
            hook().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hud_types::Capability;

    #[tokio::test]
    async fn start_hooks_publish_capabilities() {
        let mut env = Environment::new("test");
        env.on_start(|| async {
            Ok(vec![
                Capability::tcp("svc", "127.0.0.1:1234", "raw/1").unwrap()
            ])
        });
        assert!(env.capabilities().is_empty());
        env.start().await.unwrap();
        assert_eq!(env.capabilities().len(), 1);
        assert_eq!(env.find_capability("svc").unwrap().protocol, "raw/1");
        assert!(env.find_capability("nope").is_err());
        // Idempotent.
        env.start().await.unwrap();
        assert_eq!(env.capabilities().len(), 1);
    }
}
