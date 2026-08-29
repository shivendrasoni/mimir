use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock, Weak},
};

use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock};

use crate::{
    auth::{AuthCredential, AuthStore, OAuthCredential, OAuthProvider, refresh_oauth},
    error::Result as MimirResult,
    model::{ModelRequest, ModelResponse},
    provider::{
        AnthropicCredentialKind, AnthropicProvider, AuthenticationRefreshStatus, Provider,
        ProviderError, ProviderEvent, ProviderEventSink,
    },
};

type ProviderBuilder = Arc<
    dyn Fn(&OAuthCredential) -> std::result::Result<Arc<dyn Provider>, ProviderError> + Send + Sync,
>;
type RefreshLocks = StdMutex<HashMap<(PathBuf, String), Weak<Mutex<()>>>>;

#[async_trait]
trait OAuthRefresher: Send + Sync {
    async fn refresh(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> MimirResult<OAuthCredential>;
}

struct NativeOAuthRefresher;

#[async_trait]
impl OAuthRefresher for NativeOAuthRefresher {
    async fn refresh(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> MimirResult<OAuthCredential> {
        refresh_oauth(provider, credential).await
    }
}

struct ProviderState {
    credential: OAuthCredential,
    provider: Arc<dyn Provider>,
}

/// Anthropic OAuth provider wrapper that refreshes one rejected stored access
/// token and retries the same provider request exactly once.
pub(crate) struct RefreshingOAuthProvider {
    provider_id: String,
    oauth_provider: OAuthProvider,
    store: AuthStore,
    state: RwLock<ProviderState>,
    refresh_lock: Arc<Mutex<()>>,
    refresher: Arc<dyn OAuthRefresher>,
    builder: ProviderBuilder,
}

impl RefreshingOAuthProvider {
    pub(crate) fn anthropic(
        store: AuthStore,
        provider_id: &str,
        credential: OAuthCredential,
        base_url: String,
        headers: BTreeMap<String, String>,
    ) -> std::result::Result<Self, ProviderError> {
        let builder: ProviderBuilder = Arc::new(move |credential| {
            AnthropicProvider::with_credential_kind_and_headers(
                Some(&base_url),
                &credential.access,
                AnthropicCredentialKind::OAuthToken,
                &headers,
            )
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        });
        Self::with_parts(
            store,
            provider_id,
            OAuthProvider::Anthropic,
            credential,
            Arc::new(NativeOAuthRefresher),
            builder,
        )
    }

    fn with_parts(
        store: AuthStore,
        provider_id: &str,
        oauth_provider: OAuthProvider,
        credential: OAuthCredential,
        refresher: Arc<dyn OAuthRefresher>,
        builder: ProviderBuilder,
    ) -> std::result::Result<Self, ProviderError> {
        let provider = builder(&credential)?;
        Ok(Self {
            provider_id: provider_id.into(),
            oauth_provider,
            refresh_lock: refresh_lock(store.path(), provider_id),
            store,
            state: RwLock::new(ProviderState {
                credential,
                provider,
            }),
            refresher,
            builder,
        })
    }

    async fn snapshot(&self) -> (OAuthCredential, Arc<dyn Provider>) {
        let state = self.state.read().await;
        (state.credential.clone(), Arc::clone(&state.provider))
    }

    async fn refresh_after_rejection(
        &self,
        rejected: &OAuthCredential,
        sink: Option<&dyn ProviderEventSink>,
    ) -> std::result::Result<Arc<dyn Provider>, ProviderError> {
        let _guard = self.refresh_lock.lock().await;
        let current = self
            .store
            .get(&self.provider_id)
            .await
            .map_err(|_| ProviderError::Authentication)?;
        if let Some(AuthCredential::OAuth(current)) = current
            && current.access != rejected.access
            && !current.is_expired(now_ms())
        {
            let provider = (self.builder)(&current)?;
            let mut state = self.state.write().await;
            state.credential = current;
            state.provider = Arc::clone(&provider);
            return Ok(provider);
        }

        emit_refresh(
            sink,
            &self.provider_id,
            AuthenticationRefreshStatus::Started,
        )
        .await;
        let Ok(refreshed) = self.refresher.refresh(self.oauth_provider, rejected).await else {
            emit_refresh(sink, &self.provider_id, AuthenticationRefreshStatus::Failed).await;
            return Err(ProviderError::Authentication);
        };
        let Ok(provider) = (self.builder)(&refreshed) else {
            emit_refresh(sink, &self.provider_id, AuthenticationRefreshStatus::Failed).await;
            return Err(ProviderError::Authentication);
        };
        if self
            .store
            .set_oauth(&self.provider_id, refreshed.clone())
            .await
            .is_err()
        {
            emit_refresh(sink, &self.provider_id, AuthenticationRefreshStatus::Failed).await;
            return Err(ProviderError::Authentication);
        }
        let mut state = self.state.write().await;
        state.credential = refreshed;
        state.provider = Arc::clone(&provider);
        emit_refresh(
            sink,
            &self.provider_id,
            AuthenticationRefreshStatus::Succeeded,
        )
        .await;
        Ok(provider)
    }
}

#[async_trait]
impl Provider for RefreshingOAuthProvider {
    async fn complete(
        &self,
        request: ModelRequest,
    ) -> std::result::Result<ModelResponse, ProviderError> {
        let (credential, provider) = self.snapshot().await;
        match provider.complete(request.clone()).await {
            Err(ProviderError::AuthenticationRejected) => {
                self.refresh_after_rejection(&credential, None)
                    .await?
                    .complete(request)
                    .await
            }
            result => result,
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &dyn ProviderEventSink,
    ) -> std::result::Result<ModelResponse, ProviderError> {
        let (credential, provider) = self.snapshot().await;
        match provider.stream(request.clone(), sink).await {
            Err(ProviderError::AuthenticationRejected) => {
                self.refresh_after_rejection(&credential, Some(sink))
                    .await?
                    .stream(request, sink)
                    .await
            }
            result => result,
        }
    }
}

pub(crate) async fn refresh_stored_oauth_if_expired(
    store: &AuthStore,
    provider_id: &str,
    oauth_provider: OAuthProvider,
    credential: OAuthCredential,
) -> MimirResult<OAuthCredential> {
    refresh_stored_oauth_if_expired_with(
        store,
        provider_id,
        oauth_provider,
        credential,
        now_ms(),
        &NativeOAuthRefresher,
    )
    .await
}

async fn refresh_stored_oauth_if_expired_with(
    store: &AuthStore,
    provider_id: &str,
    oauth_provider: OAuthProvider,
    credential: OAuthCredential,
    now: u64,
    refresher: &dyn OAuthRefresher,
) -> MimirResult<OAuthCredential> {
    if !credential.is_expired(now) {
        return Ok(credential);
    }
    let refresh_lock = refresh_lock(store.path(), provider_id);
    let _guard = refresh_lock.lock().await;
    let current = match store.get(provider_id).await? {
        Some(AuthCredential::OAuth(current)) => current,
        Some(AuthCredential::ApiKey { .. }) | None => credential,
    };
    if !current.is_expired(now) {
        return Ok(current);
    }
    let rotated = refresher.refresh(oauth_provider, &current).await?;
    store.set_oauth(provider_id, rotated.clone()).await?;
    Ok(rotated)
}

async fn emit_refresh(
    sink: Option<&dyn ProviderEventSink>,
    provider: &str,
    status: AuthenticationRefreshStatus,
) {
    if let Some(sink) = sink {
        sink.emit(ProviderEvent::AuthenticationRefresh {
            provider: provider.into(),
            status,
        })
        .await;
    }
}

fn refresh_lock(path: &std::path::Path, provider: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<RefreshLocks> = OnceLock::new();
    let key = (
        path.canonicalize().unwrap_or_else(|_| path.to_owned()),
        provider.into(),
    );
    let locks = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Semaphore;

    use super::*;
    use crate::{
        error::MimirError,
        model::{Content, Message, StopReason, ThinkingLevel},
    };

    fn credential(access: &str, refresh: &str, expires_at_ms: u64) -> OAuthCredential {
        OAuthCredential {
            access: access.into(),
            refresh: refresh.into(),
            expires_at_ms,
            account_id: None,
            enterprise_url: None,
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "claude-test".into(),
            thinking_level: ThinkingLevel::Off,
            thinking_effort: None,
            system_prompt: String::new(),
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            max_output_tokens: 128,
        }
    }

    fn response() -> ModelResponse {
        ModelResponse {
            message: Message::assistant(
                vec![Content::Text {
                    text: "done".into(),
                }],
                StopReason::Stop,
            ),
            response_id: Some("response-redacted".into()),
        }
    }

    struct CredentialProvider {
        access: String,
        forbidden: bool,
    }

    #[async_trait]
    impl Provider for CredentialProvider {
        async fn complete(
            &self,
            _request: ModelRequest,
        ) -> std::result::Result<ModelResponse, ProviderError> {
            if self.forbidden {
                return Err(ProviderError::Authentication);
            }
            if self.access == "old-access" {
                return Err(ProviderError::AuthenticationRejected);
            }
            Ok(response())
        }
    }

    fn provider_builder(forbidden: bool) -> ProviderBuilder {
        Arc::new(move |credential| {
            Ok(Arc::new(CredentialProvider {
                access: credential.access.clone(),
                forbidden,
            }) as Arc<dyn Provider>)
        })
    }

    struct FixedRefresher {
        calls: AtomicUsize,
        next: OAuthCredential,
        reject: bool,
        delay_ms: u64,
    }

    #[async_trait]
    impl OAuthRefresher for FixedRefresher {
        async fn refresh(
            &self,
            _provider: OAuthProvider,
            _credential: &OAuthCredential,
        ) -> MimirResult<OAuthCredential> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delay_ms != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            if self.reject {
                Err(MimirError::Provider("refresh rejected".into()))
            } else {
                Ok(self.next.clone())
            }
        }
    }

    #[derive(Default)]
    struct Events(Mutex<Vec<ProviderEvent>>);

    #[async_trait]
    impl ProviderEventSink for Events {
        async fn emit(&self, event: ProviderEvent) {
            self.0.lock().await.push(event);
        }
    }

    #[tokio::test]
    async fn near_expiry_startup_refreshes_and_persists_rotated_tokens() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", 999);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let next = credential("new-access", "new-refresh", 10_000);
        let refresher = FixedRefresher {
            calls: AtomicUsize::new(0),
            next: next.clone(),
            reject: false,
            delay_ms: 0,
        };

        let rotated = refresh_stored_oauth_if_expired_with(
            &store,
            "anthropic",
            OAuthProvider::Anthropic,
            old,
            1_000,
            &refresher,
        )
        .await
        .expect("refresh");

        assert_eq!(rotated.access, "new-access");
        assert_eq!(rotated.refresh, "new-refresh");
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        let stored = store.get("anthropic").await.expect("stored");
        assert!(matches!(
            stored,
            Some(AuthCredential::OAuth(value))
                if value.access == "new-access" && value.refresh == "new-refresh"
        ));
    }

    #[tokio::test]
    async fn concurrent_startup_refreshes_coalesce_through_the_auth_store() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", 999);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let refresher = FixedRefresher {
            calls: AtomicUsize::new(0),
            next: credential("new-access", "new-refresh", 10_000),
            reject: false,
            delay_ms: 25,
        };

        let left = refresh_stored_oauth_if_expired_with(
            &store,
            "anthropic",
            OAuthProvider::Anthropic,
            old.clone(),
            1_000,
            &refresher,
        );
        let right = refresh_stored_oauth_if_expired_with(
            &store,
            "anthropic",
            OAuthProvider::Anthropic,
            old,
            1_000,
            &refresher,
        );
        let (left, right) = tokio::join!(left, right);

        assert_eq!(left.expect("left").access, "new-access");
        assert_eq!(right.expect("right").access, "new-access");
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejected_access_refreshes_once_retries_and_emits_redacted_lifecycle() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", u64::MAX);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let refresher = Arc::new(FixedRefresher {
            calls: AtomicUsize::new(0),
            next: credential("new-access", "new-refresh", u64::MAX),
            reject: false,
            delay_ms: 0,
        });
        let provider = RefreshingOAuthProvider::with_parts(
            store.clone(),
            "anthropic",
            OAuthProvider::Anthropic,
            old,
            refresher.clone(),
            provider_builder(false),
        )
        .expect("provider");
        let events = Events::default();

        let result = provider.stream(request(), &events).await.expect("retry");

        assert_eq!(result.message.text(), "done");
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *events.0.lock().await,
            vec![
                ProviderEvent::AuthenticationRefresh {
                    provider: "anthropic".into(),
                    status: AuthenticationRefreshStatus::Started,
                },
                ProviderEvent::AuthenticationRefresh {
                    provider: "anthropic".into(),
                    status: AuthenticationRefreshStatus::Succeeded,
                },
                ProviderEvent::TextDelta("done".into()),
            ]
        );
        let rendered = format!("{:?}", events.0.lock().await.as_slice());
        assert!(!rendered.contains("old-access"));
        assert!(!rendered.contains("old-refresh"));
        assert!(!rendered.contains("new-access"));
        assert!(!rendered.contains("new-refresh"));
        let stored = store.get("anthropic").await.expect("stored");
        assert!(matches!(
            stored,
            Some(AuthCredential::OAuth(value)) if value.refresh == "new-refresh"
        ));
    }

    #[tokio::test]
    async fn concurrent_rejections_coalesce_to_one_refresh() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", u64::MAX);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let refresher = Arc::new(FixedRefresher {
            calls: AtomicUsize::new(0),
            next: credential("new-access", "new-refresh", u64::MAX),
            reject: false,
            delay_ms: 25,
        });
        let provider = Arc::new(
            RefreshingOAuthProvider::with_parts(
                store,
                "anthropic",
                OAuthProvider::Anthropic,
                old,
                refresher.clone(),
                provider_builder(false),
            )
            .expect("provider"),
        );

        let left = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move { provider.complete(request()).await })
        };
        let right = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move { provider.complete(request()).await })
        };
        left.await.expect("left task").expect("left result");
        right.await.expect("right task").expect("right result");

        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_rejection_is_not_retried_and_preserves_stored_credential() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", u64::MAX);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let refresher = Arc::new(FixedRefresher {
            calls: AtomicUsize::new(0),
            next: credential("unused", "unused", u64::MAX),
            reject: true,
            delay_ms: 0,
        });
        let provider = RefreshingOAuthProvider::with_parts(
            store.clone(),
            "anthropic",
            OAuthProvider::Anthropic,
            old,
            refresher.clone(),
            provider_builder(false),
        )
        .expect("provider");

        let error = provider.complete(request()).await.expect_err("rejected");
        assert_eq!(error, ProviderError::Authentication);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        let stored = store.get("anthropic").await.expect("stored");
        assert!(matches!(
            stored,
            Some(AuthCredential::OAuth(value)) if value.access == "old-access"
        ));
    }

    #[tokio::test]
    async fn permission_failure_never_attempts_refresh() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", u64::MAX);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let refresher = Arc::new(FixedRefresher {
            calls: AtomicUsize::new(0),
            next: credential("unused", "unused", u64::MAX),
            reject: false,
            delay_ms: 0,
        });
        let provider = RefreshingOAuthProvider::with_parts(
            store,
            "anthropic",
            OAuthProvider::Anthropic,
            old,
            refresher.clone(),
            provider_builder(true),
        )
        .expect("provider");

        let error = provider.complete(request()).await.expect_err("forbidden");
        assert_eq!(error, ProviderError::Authentication);
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 0);
    }

    struct CancellableRefresher {
        calls: AtomicUsize,
        started: Semaphore,
        release_first: Semaphore,
        next: OAuthCredential,
    }

    #[async_trait]
    impl OAuthRefresher for CancellableRefresher {
        async fn refresh(
            &self,
            _provider: OAuthProvider,
            _credential: &OAuthCredential,
        ) -> MimirResult<OAuthCredential> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.started.add_permits(1);
                self.release_first
                    .acquire()
                    .await
                    .expect("release semaphore")
                    .forget();
            }
            Ok(self.next.clone())
        }
    }

    #[tokio::test]
    async fn cancellation_during_refresh_releases_coalescing_lock() {
        let state = tempfile::tempdir().expect("state");
        let store = AuthStore::new(state.path()).expect("auth store");
        let old = credential("old-access", "old-refresh", u64::MAX);
        store
            .set_oauth("anthropic", old.clone())
            .await
            .expect("seed auth");
        let refresher = Arc::new(CancellableRefresher {
            calls: AtomicUsize::new(0),
            started: Semaphore::new(0),
            release_first: Semaphore::new(0),
            next: credential("new-access", "new-refresh", u64::MAX),
        });
        let provider = Arc::new(
            RefreshingOAuthProvider::with_parts(
                store,
                "anthropic",
                OAuthProvider::Anthropic,
                old,
                refresher.clone(),
                provider_builder(false),
            )
            .expect("provider"),
        );
        let first = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move { provider.complete(request()).await })
        };
        refresher
            .started
            .acquire()
            .await
            .expect("refresh started")
            .forget();
        first.abort();
        let _ = first.await;

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            provider.complete(request()),
        )
        .await
        .expect("refresh lock released")
        .expect("second refresh succeeds");
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 2);
    }
}
