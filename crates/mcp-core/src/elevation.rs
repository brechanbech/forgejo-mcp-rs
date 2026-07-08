//! Time-boxed write-mode elevation.
//!
//! [`Elevation`] holds an optional write client and a sliding auto-revert window. Writes are
//! refused unless a write client was configured *and* the window is currently active; each
//! successful write slides the window forward, and it otherwise expires back to read-only. This
//! is deliberately not a permanent mode — there is a hard cap on the window length.
//!
//! The type is generic over the client `C`, so any server (Forgejo, Woodpecker, …) reuses the
//! same gating by parameterizing it with its own write client.

// Every accessor locks the private `state` Mutex with `.unwrap()`. The only way that panics is a
// poisoned lock, which cannot happen here: no code panics while holding it. So the pedantic
// `# Panics` sections would document an unreachable state — suppress the lint crate-locally.
#![allow(clippy::missing_panics_doc)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::ErrorData as McpError;

/// Write-mode gate around an optional write client `C`.
pub struct Elevation<C> {
    /// Write client — present only if a write token was configured.
    write: Option<Arc<C>>,
    /// Active window as `(expires_at, window_length)`; `None` = read mode.
    state: Mutex<Option<(Instant, Duration)>>,
    /// Default window used by [`Elevation::enable`] when no explicit length is given.
    default_window: Duration,
    /// Hard cap on the window length (minutes).
    max_minutes: u64,
    /// Name of the env var that supplies the write token, for the "not configured" message
    /// (e.g. `FORGEJO_TOKEN_WRITE`).
    write_env: &'static str,
}

impl<C> std::fmt::Debug for Elevation<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Elevation")
            .field("configured", &self.write.is_some())
            .field("max_minutes", &self.max_minutes)
            .finish_non_exhaustive()
    }
}

impl<C> Elevation<C> {
    /// Builds the gate. `default_minutes` is clamped to `1..=max_minutes`.
    #[must_use]
    pub fn new(
        write: Option<Arc<C>>,
        default_minutes: u64,
        max_minutes: u64,
        write_env: &'static str,
    ) -> Self {
        let default_minutes = default_minutes.clamp(1, max_minutes);
        Self {
            write,
            state: Mutex::new(None),
            default_window: Duration::from_secs(default_minutes * 60),
            max_minutes,
            write_env,
        }
    }

    /// Whether a write client is configured at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.write.is_some()
    }

    /// The default window length in minutes.
    #[must_use]
    pub fn default_minutes(&self) -> u64 {
        self.default_window.as_secs() / 60
    }

    /// The hard cap on the window length in minutes.
    #[must_use]
    pub fn max_minutes(&self) -> u64 {
        self.max_minutes
    }

    /// The error returned when no write token is configured — same wording used by the write
    /// tools and by `enable_write_mode`.
    #[must_use]
    pub fn not_configured_error(&self) -> McpError {
        McpError::invalid_params(
            format!(
                "read-only: no {} is configured for this server",
                self.write_env
            ),
            None,
        )
    }

    /// The write client, but only while write mode is active; otherwise a clear error explaining
    /// how to proceed (no write token, or not elevated).
    ///
    /// # Errors
    /// `invalid_params` if no write token is configured, or if write mode is not currently active.
    pub fn client(&self) -> Result<&C, McpError> {
        let Some(client) = self.write.as_deref() else {
            return Err(self.not_configured_error());
        };
        let active = self
            .state
            .lock()
            .unwrap()
            .is_some_and(|(until, _)| Instant::now() < until);
        if !active {
            return Err(McpError::invalid_params(
                "write mode is not active — call enable_write_mode first (and tell the user)"
                    .to_owned(),
                None,
            ));
        }
        Ok(client)
    }

    /// Enters write mode for `minutes` (default when `None`), clamped to `1..=max_minutes`.
    /// Returns the applied window length. Callers should reject up-front with
    /// [`Elevation::not_configured_error`] when no write client exists.
    pub fn enable(&self, minutes: Option<u32>) -> u64 {
        let minutes = minutes
            .map_or_else(|| self.default_minutes(), u64::from)
            .clamp(1, self.max_minutes);
        let window = Duration::from_secs(minutes * 60);
        *self.state.lock().unwrap() = Some((Instant::now() + window, window));
        minutes
    }

    /// Leaves write mode immediately (back to read-only).
    pub fn disable(&self) {
        *self.state.lock().unwrap() = None;
    }

    /// Slides the auto-revert window forward after a successful write.
    pub fn extend(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some((_, window)) = *state {
            *state = Some((Instant::now() + window, window));
        }
    }

    /// Minutes left in the current window (0 if inactive).
    #[must_use]
    pub fn minutes_remaining(&self) -> u64 {
        match *self.state.lock().unwrap() {
            Some((until, _)) => until
                .saturating_duration_since(Instant::now())
                .as_secs()
                .div_ceil(60),
            None => 0,
        }
    }

    /// A short note about the current window, appended to write results.
    #[must_use]
    pub fn window_note(&self) -> String {
        let left = self.minutes_remaining();
        if left == 0 {
            "write mode inactive".to_owned()
        } else {
            format!("write mode active — about {left} min remaining (auto-reverts)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Arc, Duration, Elevation, Instant};

    /// A stand-in write client; the gating logic never touches it.
    struct DummyClient;

    fn gate(with_write: bool) -> Elevation<DummyClient> {
        let write = with_write.then(|| Arc::new(DummyClient));
        Elevation::new(write, 10, 60, "TOKEN_WRITE")
    }

    /// Sets the elevation window to expire at `until` (with a fixed 10-minute slide length).
    fn set_until(e: &Elevation<DummyClient>, until: Instant) {
        *e.state.lock().unwrap() = Some((until, Duration::from_secs(600)));
    }

    fn in_future() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    fn in_past() -> Instant {
        Instant::now().checked_sub(Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn no_write_token_always_refuses() {
        let e = gate(false);
        assert!(e.client().is_err(), "no token -> refused");
        set_until(&e, in_future());
        assert!(
            e.client().is_err(),
            "no token, even 'elevated' -> still refused"
        );
    }

    #[test]
    fn gating_requires_active_window() {
        let e = gate(true);
        assert!(e.client().is_err(), "not elevated -> refused");
        assert_eq!(e.minutes_remaining(), 0);

        set_until(&e, in_future());
        assert!(e.client().is_ok(), "elevated -> allowed");
        assert!(e.minutes_remaining() >= 9);

        set_until(&e, in_past());
        assert!(e.client().is_err(), "expired -> refused");
        assert_eq!(e.minutes_remaining(), 0);
    }

    #[test]
    fn extend_window_re_arms() {
        let e = gate(true);
        set_until(&e, Instant::now()); // on the edge of expiry
        e.extend(); // slides forward by the stored window
        assert!(e.client().is_ok());
        assert!(e.minutes_remaining() >= 9);
    }

    #[test]
    fn enable_clamps_and_disable_reverts() {
        let e = gate(true);
        assert_eq!(e.enable(Some(999)), 60, "clamped to max");
        assert!(e.client().is_ok());
        assert_eq!(e.enable(Some(0)), 1, "clamped to min");
        assert_eq!(e.enable(None), 10, "default window");
        e.disable();
        assert!(e.client().is_err(), "disabled -> refused");
    }
}
