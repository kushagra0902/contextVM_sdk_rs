//! Thin wrappers around `tracing` for stable call sites (e.g. rmcp conversion).
//!
//! Note: `tracing` macros require a string-literal `target:` for callsite registration.
//! The `logical_target` field preserves the module-style string passed by callers.

/// Log at error level; `logical_target` is the caller’s notion of module/path (for filters in output).
pub fn error_with_target(logical_target: &str, message: impl std::fmt::Display) {
    tracing::error!(
        target: "contextvm_sdk",
        logical_target = logical_target,
        "{}",
        message
    );
}
