#![allow(
    clippy::missing_errors_doc,
    reason = "RLM operations return typed validation and persistence errors"
)]

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    atomic,
    error::{MimirError, Result},
};

#[derive(Debug, Clone, Copy)]
pub struct RlmLimits {
    pub max_value_bytes: usize,
    pub max_namespace_bytes: usize,
    pub max_keys: usize,
}

#[derive(Debug)]
pub struct RlmStore {
    state_root: PathBuf,
    extension_name: String,
    workspace_scope: String,
    limits: RlmLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamespaceState {
    schema_version: u16,
    #[serde(default)]
    values: BTreeMap<String, Value>,
}

impl Default for NamespaceState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            values: BTreeMap::new(),
        }
    }
}

impl RlmStore {
    pub fn new(
        state_root: &Path,
        workspace_root: &Path,
        extension_name: &str,
        limits: RlmLimits,
    ) -> Result<Self> {
        validate_name("extension", extension_name)?;
        Ok(Self {
            state_root: atomic::canonical_state_root(state_root),
            extension_name: extension_name.to_owned(),
            workspace_scope: workspace_scope(workspace_root),
            limits,
        })
    }

    pub async fn put(&self, namespace: &str, key: &str, value: Value) -> Result<()> {
        validate_name("namespace", namespace)?;
        validate_name("key", key)?;
        if serde_json::to_vec(&value)?.len() > self.limits.max_value_bytes {
            return Err(MimirError::Configuration(
                "RLM value exceeds the configured value limit".into(),
            ));
        }
        let path = self.namespace_path(namespace);
        let lock = atomic::path_lock(&path);
        let _guard = lock.lock().await;
        let mut state = self.load_unlocked(namespace).await?;
        state.values.insert(key.to_owned(), value);
        if state.values.len() > self.limits.max_keys {
            return Err(MimirError::Configuration(
                "RLM namespace exceeds the configured key limit".into(),
            ));
        }
        self.write_unlocked(namespace, &state).await
    }

    pub async fn get(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
        validate_name("namespace", namespace)?;
        validate_name("key", key)?;
        Ok(self
            .load_unlocked(namespace)
            .await?
            .values
            .get(key)
            .cloned())
    }

    pub async fn list(&self, namespace: &str) -> Result<Value> {
        validate_name("namespace", namespace)?;
        let values = self.load_unlocked(namespace).await?.values;
        let object = values.into_iter().collect::<Map<String, Value>>();
        Ok(Value::Object(object))
    }

    pub async fn delete(&self, namespace: &str, key: &str) -> Result<()> {
        validate_name("namespace", namespace)?;
        validate_name("key", key)?;
        let path = self.namespace_path(namespace);
        let lock = atomic::path_lock(&path);
        let _guard = lock.lock().await;
        let mut state = self.load_unlocked(namespace).await?;
        state.values.remove(key);
        self.write_unlocked(namespace, &state).await
    }

    async fn load_unlocked(&self, namespace: &str) -> Result<NamespaceState> {
        let path = self.namespace_path(namespace);
        atomic::prepare_state_path(&self.state_root, &path).await?;
        let state: NamespaceState = atomic::read_json(&path).await?.unwrap_or_default();
        if state.schema_version != 1 {
            return Err(MimirError::Configuration(format!(
                "unsupported RLM namespace schema version {}",
                state.schema_version
            )));
        }
        Ok(state)
    }

    async fn write_unlocked(&self, namespace: &str, state: &NamespaceState) -> Result<()> {
        let path = self.namespace_path(namespace);
        atomic::prepare_state_path(&self.state_root, &path).await?;
        if serde_json::to_vec_pretty(state)?.len() > self.limits.max_namespace_bytes {
            return Err(MimirError::Configuration(
                "RLM namespace exceeds the configured namespace size limit".into(),
            ));
        }
        atomic::write_json(&path, state).await
    }

    fn namespace_path(&self, namespace: &str) -> PathBuf {
        self.state_root
            .join("extensions/rlm")
            .join(&self.extension_name)
            .join(&self.workspace_scope)
            .join(format!("{namespace}.json"))
    }
}

fn validate_name(label: &str, value: &str) -> Result<()> {
    let pattern = Regex::new(r"^[A-Za-z0-9._-]{1,64}$").expect("RLM name regex is valid");
    if pattern.is_match(value) {
        Ok(())
    } else {
        Err(MimirError::Configuration(format!(
            "RLM {label} '{value}' is invalid"
        )))
    }
}

fn workspace_scope(workspace_root: &Path) -> String {
    let workspace = std::fs::canonicalize(workspace_root)
        .unwrap_or_else(|_| workspace_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let digest = Sha256::digest(workspace.as_bytes());
    let mut key = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        write!(key, "{byte:02x}").expect("writing to a string cannot fail");
    }
    key
}
