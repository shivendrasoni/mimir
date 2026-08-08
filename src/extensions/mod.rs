mod actions;
mod catalog;
mod embedded;
mod host;
mod manager;
mod manifest;
mod package;
mod rlm;
mod rlm_integration;
mod rlm_runtime;
mod runtime;

pub use actions::{
    ExtensionCommandInfo, ExtensionContextUsage, ExtensionDelivery, ExtensionFlagValue,
    ExtensionForkPosition, ExtensionHostAction, ExtensionHostSnapshot, ExtensionSessionAction,
    ExtensionToolInfo, FlagDescriptor, FlagKind, ShortcutDescriptor,
};
pub use catalog::{CatalogEntry, ExtensionCatalog, ManifestSource};
pub use embedded::EmbeddedJsExtensionHost;
pub use host::{HostLimits, HostRequest, HostResponse, HostResponseStatus, JsonLineExtensionHost};
pub use manager::{DiscoveredResourcePaths, ExtensionDispatch, ExtensionManager};
pub use manifest::{Capability, ExtensionEntrypoint, ExtensionManifest};
pub use package::{ExtensionPackageManager, InstalledExtensionPackage, RemovedExtensionPackage};
pub use rlm::{RlmLimits, RlmStore};
pub use rlm_integration::{
    AgentRuntimeChildExecutor, AuthStoreModelCatalog, AuthStoreProviderAuthentication,
    ProviderAuthenticationProbe, RlmChildRuntimePolicy, RlmChildToolRegistryFactory,
    RlmHostOperations, RlmProviderFactory,
};
pub use rlm_runtime::{
    AuthenticatedModelCatalog, DEFAULT_RLM_MODEL_SEARCH_LIMIT, MAX_RLM_MODEL_SEARCH_LIMIT,
    RlmChildExecutor, RlmChildStatus, RlmDeleteResult, RlmExecutionRequest, RlmExecutionResult,
    RlmModel, RlmRunRequest, RlmRuntime, RlmRuntimeLimits, RlmSpawnHandle, RlmSubagent,
};
pub use runtime::{
    CommandArgumentCompletion, CommandArgumentCompletions, CommandDescriptor,
    CommandInvocationResult, ExtensionCallStatus, ExtensionProviderEvent,
    ExtensionProviderStreamResult, ExtensionRegistrations, ExtensionRuntime, LifecycleEvent,
    LifecycleEventKind, LifecycleInterception, LifecycleMutation, LifecycleOutcome,
    LifecycleReplacement, ProviderApi, ProviderDescriptor, ProviderTransport, RenderOutput,
    RendererDescriptor, ResourceDiscoveryReason, RuntimeLimits, SessionForkPosition,
    SessionStartReason, SessionSwitchReason, ToolDescriptor, ToolInvocationResult, UiRequest,
    UiRequestKind, UiResponseContinuation,
};
