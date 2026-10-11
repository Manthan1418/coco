// API keys live in the freedesktop Secret Service (gnome-keyring / KWallet),
// never on disk and never in the front end — the island can only ask whether a
// key is present. This is the Linux counterpart of the Windows Credential
// Manager the upstream app used.

use keyring::Entry;

const SERVICE: &str = "dev.coco.island";

/// Every key Coco may store. Anything outside this list is refused.
pub const KNOWN_KEYS: &[&str] = &[
    "anthropic-api-key",
    "openrouter-api-key",
    "openai-api-key",
    "custom-api-key",
    "brave-api-key",
    "opencode-api-key",
    "n8n-url",
    "n8n-api-key",
    "vercel-token",
    "github-token",
    "stripe-api-key",
    "resend-api-key",
    "notion-api-key",
    "calcom-api-key",
    // The SMTP password for the island's "send by email". Host and addresses are
    // plain settings; only the password is a secret.
    "smtp-password",
];

/// Secret-service key holding the API key for a chat provider.
/// Unknown names fall back to Anthropic rather than refusing to chat.
pub fn key_for_provider(provider: &str) -> &'static str {
    match provider {
        "openrouter" => "openrouter-api-key",
        "openai" => "openai-api-key",
        "custom" => "custom-api-key",
        _ => "anthropic-api-key",
    }
}

fn entry(key: &str) -> Option<Entry> {
    if !KNOWN_KEYS.contains(&key) {
        return None;
    }
    Entry::new(SERVICE, key).ok()
}

pub fn get(key: &str) -> Option<String> {
    entry(key)?.get_password().ok().filter(|v| !v.is_empty())
}

pub fn set(key: &str, value: &str) -> Result<(), String> {
    let entry = entry(key).ok_or_else(|| format!("unknown key {key}"))?;
    if value.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    entry.set_password(value).map_err(|e| e.to_string())
}

pub fn clear(key: &str) -> Result<(), String> {
    let entry = entry(key).ok_or_else(|| format!("unknown key {key}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn present(key: &str) -> bool {
    get(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chat_provider_has_a_storable_key() {
        for provider in ["anthropic", "openrouter", "openai", "custom"] {
            let key = key_for_provider(provider);
            assert!(
                KNOWN_KEYS.contains(&key),
                "provider {provider} maps to {key}, which the allowlist would refuse"
            );
        }
    }

    #[test]
    fn unknown_providers_fall_back_to_anthropic() {
        assert_eq!(key_for_provider(""), "anthropic-api-key");
        assert_eq!(key_for_provider("whatever"), "anthropic-api-key");
    }
}
