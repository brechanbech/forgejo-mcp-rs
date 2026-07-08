//! Read/write token resolution shared by the server modules.

use anyhow::Context as _;

/// The env-var names (and a short token description) a server uses for its read/write tokens, so
/// the shared [`resolve_tokens`] can name them in its error messages.
#[derive(Debug, Clone, Copy)]
pub struct TokenEnv {
    /// Primary read-token variable, e.g. `FORGEJO_TOKEN_READ_ONLY`.
    pub read_only: &'static str,
    /// Legacy/fallback read-token variable, e.g. `FORGEJO_TOKEN`.
    pub legacy: &'static str,
    /// Write-token variable, e.g. `FORGEJO_TOKEN_WRITE`.
    pub write: &'static str,
    /// How to describe the token to mint, e.g. `"a read-scoped token"`.
    pub kind: &'static str,
}

/// Resolves the read token (required) and optional write token from their env values, enforcing
/// two rules: a dedicated read token must exist (a write token alone is refused — even though it
/// could read), and the read token must differ from the write token (no reusing the write token in
/// the read slot). Empty strings count as unset. `env` supplies the variable names named in the
/// error messages.
///
/// # Errors
/// Fails if no read token is present (under either name), or if the read token equals the write
/// token.
pub fn resolve_tokens(
    read_only: Option<String>,
    legacy: Option<String>,
    write: Option<String>,
    env: TokenEnv,
) -> anyhow::Result<(String, Option<String>)> {
    let nonempty = |value: Option<String>| value.filter(|s| !s.is_empty());
    let read = nonempty(read_only).or_else(|| nonempty(legacy)).with_context(|| {
        format!(
            "a read-only token is required: set {} (or {}) to {}. A write token alone is refused \
             — reads must use a dedicated read-only token, even though a write token could \
             technically read.",
            env.read_only, env.legacy, env.kind
        )
    })?;
    let write = nonempty(write);
    if write.as_deref() == Some(read.as_str()) {
        anyhow::bail!(
            "the read token and {} must be different tokens — put a separate read-only token in \
             the read slot, not a copy of the write token.",
            env.write
        );
    }
    Ok((read, write))
}

#[cfg(test)]
mod tests {
    use super::{TokenEnv, resolve_tokens};

    const ENV: TokenEnv = TokenEnv {
        read_only: "TOKEN_READ_ONLY",
        legacy: "TOKEN",
        write: "TOKEN_WRITE",
        kind: "a token",
    };

    #[test]
    fn read_token_is_required() {
        assert!(
            resolve_tokens(None, None, None, ENV).is_err(),
            "nothing -> refused"
        );
        // The "clever" case: a write token alone is refused.
        assert!(
            resolve_tokens(None, None, Some("w".into()), ENV).is_err(),
            "write token only -> refused"
        );
        // Empty strings count as unset.
        assert!(resolve_tokens(Some(String::new()), None, Some("w".into()), ENV).is_err());
    }

    #[test]
    fn read_and_write_must_differ() {
        assert!(
            resolve_tokens(Some("same".into()), None, Some("same".into()), ENV).is_err(),
            "read == write -> refused"
        );
        let (r, w) = resolve_tokens(Some("r".into()), None, Some("w".into()), ENV).unwrap();
        assert_eq!((r.as_str(), w.as_deref()), ("r", Some("w")));
    }

    #[test]
    fn read_token_resolves_with_fallback_and_no_write() {
        // The legacy fallback works; no write token -> read-only.
        let (r, w) = resolve_tokens(None, Some("legacy".into()), None, ENV).unwrap();
        assert_eq!((r.as_str(), w), ("legacy", None));
        // An empty write token is treated as unset (not equal-to-read failure).
        let (r, w) = resolve_tokens(Some("r".into()), None, Some(String::new()), ENV).unwrap();
        assert_eq!((r.as_str(), w), ("r", None));
    }
}
