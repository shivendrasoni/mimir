# Project guidance

## P0: Mimir must remain independent of Betterloop

This is a non-negotiable product and architecture invariant. Mimir must remain a
fully usable standalone harness. Never require Betterloop installation, setup,
an account, enrollment, credentials, a configuration repository, or service
availability to install, authenticate with a model provider, launch, or use
Mimir's core CLI, TUI, tools, sessions, and RLM capabilities.

- Betterloop integration is optional and must require explicit user opt-in.
  Never auto-enroll users or gate standalone use behind an enrollment prompt.
- Keep standalone configuration and execution paths complete. Betterloop must
  not become a mandatory build dependency or runtime service dependency.
- An absent, unconfigured, or unavailable Betterloop integration must not block
  or degrade an unenrolled user's core harness. Provider authentication and
  explicitly selected optional capabilities may retain their own requirements.
- Policies accepted through explicit enterprise enrollment apply only to that
  enrolled mode. Preserve their enforcement, including fail-closed mandatory
  controls, without extending those requirements to standalone users. Keep an
  explicit unenrollment path back to standalone use.
- For changes touching startup, authentication, configuration, enterprise
  integration, or capability loading, verify standalone operation without any
  Betterloop setup or credentials, and cover relevant regressions with tests.
  Reject any change that makes Betterloop compulsory for core Mimir use.
