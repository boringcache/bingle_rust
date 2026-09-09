use algo_ops::AlgoChainConfig;
use serde::Deserialize;
use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};

/// Parse a decimal ALGOs string into microAlgos (u64).
/// Accepts forms like "1", "0.5", "1.234567"; up to 6 fractional digits.
/// Returns an error on negative numbers, more than 6 fractional digits, or overflow.
pub fn parse_algos_decimal_to_microalgos(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("price must not be empty".to_string());
    }
    if t.starts_with('-') {
        return Err("price must be non-negative".to_string());
    }
    // Split on decimal point
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() > 2 {
        return Err(format!("invalid price '{}': too many decimal points", s));
    }
    let whole_str = parts[0];
    let frac_str = if parts.len() == 2 { parts[1] } else { "" };
    if frac_str.len() > 6 {
        return Err(format!(
            "invalid price '{}': more than 6 fractional digits",
            s
        ));
    }
    // Parse digits only
    if !whole_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid price '{}': non-digit characters", s));
    }
    if !frac_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid price '{}': non-digit characters", s));
    }
    let whole: u128 = if whole_str.is_empty() {
        0
    } else {
        whole_str
            .parse()
            .map_err(|e| format!("invalid price '{}': {}", s, e))?
    };
    // Pad fractional to 6 digits
    let mut frac_padded = String::from(frac_str);
    while frac_padded.len() < 6 {
        frac_padded.push('0');
    }
    let frac: u128 = if frac_padded.is_empty() {
        0
    } else {
        frac_padded[..6]
            .parse()
            .map_err(|e| format!("invalid price '{}': {}", s, e))?
    };
    let micro: u128 = whole
        .checked_mul(1_000_000u128)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(|| format!("invalid price '{}': overflow", s))?;
    if micro > u64::MAX as u128 {
        return Err(format!("invalid price '{}': overflow", s));
    }
    Ok(micro as u64)
}

#[cfg(test)]
mod tests_do_not_use_inline {
    // Intentionally left empty per project guideline: tests live under tests/.
}

/// Parse a comma or whitespace separated list of socket addresses or hostnames with ports.
/// Accepts entries like "1.2.3.4:3478" or "stun.example.com:3478".
pub fn parse_stun_list(s: &str) -> Result<Vec<SocketAddr>, String> {
    // First, strip out line comments: anything after '#' on a line is ignored.
    let mut cleaned = String::with_capacity(s.len());
    for line in s.lines() {
        let line_no_comment = match line.find('#') {
            Some(idx) => &line[..idx],
            None => line,
        };
        cleaned.push_str(line_no_comment);
        cleaned.push('\n');
    }

    let mut addrs = Vec::new();
    for part in cleaned.split(|c: char| c == ',' || c.is_whitespace()) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        // Try direct SocketAddr parse first, filtering for IPv4
        let direct_parsed = p.parse::<SocketAddr>().ok();

        // If it parsed as IPv6 directly, we skip it (as requested: "only take the first IPV4 address")
        // NOTE: The requirement says "only take the first IPV4 address" for each entry.
        // If an entry is explicitly IPv6, we should ignore it rather than erroring
        // if we want to be robust, OR error if it's invalid.
        // Given the requirement "only take the first IPV4 address", if we find an IPv6 we should look for IPv4.

        let parsed = direct_parsed
            .filter(|addr| addr.is_ipv4())
            // Fallback to DNS resolution via ToSocketAddrs for host:port strings
            .or_else(|| {
                p.to_socket_addrs()
                    .ok()
                    .and_then(|it| it.filter(|addr| addr.is_ipv4()).next())
            });

        if let Some(addr) = parsed {
            addrs.push(addr);
        } else if direct_parsed.is_some() {
            // It was a valid SocketAddr but IPv6, skip it as per requirement to only take IPv4
            tracing::warn!("Ignoring unsupported IPv6 STUN server entry '{}'", p);
            continue;
        } else {
            return Err(format!(
                "Invalid STUN server entry '{}': must be <host:port> or <ip:port>",
                p
            ));
        }
    }
    if addrs.is_empty() {
        Err("No valid STUN servers provided".to_string())
    } else {
        Ok(addrs)
    }
}

/// Read a file and parse a list of socket addresses (one per line or comma/space separated).
pub fn parse_stun_file(path: &str) -> Result<Vec<SocketAddr>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read STUN servers file '{}': {}", path, e))?;
    parse_stun_list(&content)
}

#[derive(Deserialize)]
struct NodeFile {
    network: Option<String>,
    client_api_url: String,
    client_api_port: u16,
    indexer_api_url: String,
    indexer_api_port: u16,
    token: Option<String>,
    token_key: Option<String>,
    app_id: Option<u64>,
    asset_id: Option<u64>,
}

/// Parse a Bingle node file, returning its network name, Algorand chain configuration, and the
/// application and asset ids it declares (if any).
///
/// # Errors
///
/// Returns an error string if the file cannot be read or its JSON cannot be parsed.
pub fn parse_node_file_with_ids(
    path: &str,
) -> Result<(Option<String>, AlgoChainConfig, Option<u64>, Option<u64>), String> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read node file in {} '{}': {}", cwd, path, e))?;
    let nf: NodeFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse node file '{}': {}", path, e))?;
    Ok((
        nf.network,
        AlgoChainConfig {
            client_api_url: nf.client_api_url,
            client_api_port: nf.client_api_port,
            indexer_api_url: nf.indexer_api_url,
            indexer_api_port: nf.indexer_api_port,
            token: nf.token,
            token_key: nf.token_key,
            app_id: nf.app_id,
            asset_id: nf.asset_id,
            // Throttling/budget knobs are not expressed in the node file; take the
            // algo_ops defaults (both `None`). `..Default::default()` also absorbs
            // any further optional fields algo_ops adds without breaking this call.
            ..Default::default()
        },
        nf.app_id,
        nf.asset_id,
    ))
}

/// Resolve the effective application and asset ids from the node-file values and any
/// command-line overrides, requiring exactly one source for each.
///
/// # Errors
///
/// Returns an error string if both a node-file value and a command-line override are given for the
/// same id, or if neither supplies a required id.
pub fn resolve_app_asset_ids(
    node_app_id: Option<u64>,
    node_asset_id: Option<u64>,
    cli_app_id: Option<u64>,
    cli_asset_id: Option<u64>,
) -> Result<(u64, u64), String> {
    if node_app_id.is_some() && cli_app_id.is_some() {
        return Err("--app-id provided but node file also contains app_id; remove one".to_string());
    }
    if node_asset_id.is_some() && cli_asset_id.is_some() {
        return Err(
            "--asset-id provided but node file also contains asset_id; remove one".to_string(),
        );
    }

    let env_app = std::env::var("APP_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let env_asset = std::env::var("ASSET_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());

    let final_app = node_app_id.or(cli_app_id).or(env_app).ok_or_else(|| {
        "Missing app_id: provide in node file, via --app-id, or set APP_ID".to_string()
    })?;
    let final_asset = node_asset_id
        .or(cli_asset_id)
        .or(env_asset)
        .ok_or_else(|| {
            "Missing asset_id: provide in node file, via --asset-id, or set ASSET_ID".to_string()
        })?;

    Ok((final_app, final_asset))
}
