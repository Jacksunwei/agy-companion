use ::url::{Host, Origin, Url, form_urlencoded};
use companion_core::{
    CoreError, decode_ascii_hex_nibble, is_printable_ascii_with_space, validate_api_key_str,
    validate_upstream_url, validate_url_path,
};
use http::header::HeaderValue;

use super::GEMINI_API_KEY_ENV_VAR;
use crate::ProviderError;

const GEMINI_MODELS_PREFIX: &str = "/v1beta/models/";

#[derive(Clone)]
pub(super) enum EnvKeySource {
    ProcessEnv,
    Disabled,
    #[cfg(test)]
    Fixed(Option<String>),
}

impl EnvKeySource {
    pub(super) fn read(&self) -> Result<Option<String>, ProviderError> {
        match self {
            Self::ProcessEnv => match std::env::var(GEMINI_API_KEY_ENV_VAR) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => Err(ProviderError::InvalidConfig(
                    "GEMINI_API_KEY environment variable contains invalid UTF-8".to_string(),
                )),
            },
            Self::Disabled => Ok(None),
            #[cfg(test)]
            Self::Fixed(value) => Ok(value.clone()),
        }
    }
}

pub(crate) fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

pub(super) fn normalize_base_url(raw: &str) -> Result<(Url, String, String), ProviderError> {
    validate_upstream_url(raw).map_err(|err| ProviderError::InvalidConfig(err.to_string()))?;

    let parsed = Url::parse(raw)
        .map_err(|err| ProviderError::InvalidConfig(format!("malformed base URL: {err}")))?;
    if !matches!(parsed.origin(), Origin::Tuple(..)) {
        return Err(ProviderError::InvalidConfig(
            "base URL must have a tuple origin".to_string(),
        ));
    }

    let origin = parsed.origin().ascii_serialization();
    let mut base_path = parsed.path().trim_end_matches('/');
    while let Some(stripped) = base_path.strip_suffix("/v1beta") {
        base_path = stripped.trim_end_matches('/');
    }

    let normalized_str = format!("{origin}{base_path}");
    let normalized = Url::parse(&normalized_str).map_err(|_| {
        ProviderError::InvalidConfig("normalized base URL failed re-parse".to_string())
    })?;

    let expected_path = if base_path.is_empty() { "/" } else { base_path };
    if normalized.scheme() != parsed.scheme()
        || normalized.host_str() != parsed.host_str()
        || normalized.port_or_known_default() != parsed.port_or_known_default()
        || !normalized.username().is_empty()
        || normalized.password().is_some()
        || normalized.query().is_some()
        || normalized.fragment().is_some()
        || normalized.path() != expected_path
    {
        return Err(ProviderError::InvalidConfig(
            "base URL normalization invariant failed".to_string(),
        ));
    }

    Ok((normalized, normalized_str, base_path.to_string()))
}

pub(super) fn build_target_url(
    base_url: &Url,
    normalized_base_url: &str,
    normalized_base_path: &str,
    path_and_query: &str,
) -> Result<Url, ProviderError> {
    if path_and_query.is_empty()
        || !path_and_query.bytes().all(|b| (0x21..=0x7E).contains(&b))
        || path_and_query.contains('#')
        || path_and_query.contains('\\')
    {
        return Err(ProviderError::InvalidPath(
            "request path must be non-empty printable ASCII without whitespace, controls, '#', or '\\'"
                .to_string(),
        ));
    }

    let (raw_path, raw_query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };

    let Some(rest) = raw_path.strip_prefix(GEMINI_MODELS_PREFIX) else {
        return Err(ProviderError::InvalidPath(
            "request path must start with '/v1beta/models/'".to_string(),
        ));
    };
    validate_model_path_rest(raw_path, rest)?;

    let sanitized_query = match raw_query {
        Some(q) => sanitize_query(q)?,
        None => None,
    };

    let expected_joined_path = format!("{normalized_base_path}{raw_path}");
    let candidate = match sanitized_query {
        Some(ref q) => format!("{normalized_base_url}{raw_path}?{q}"),
        None => format!("{normalized_base_url}{raw_path}"),
    };

    parse_and_verify_url(
        base_url,
        &candidate,
        &expected_joined_path,
        sanitized_query.as_deref(),
    )
}

pub(super) fn build_models_list_url(
    base_url: &Url,
    normalized_base_url: &str,
    normalized_base_path: &str,
    page_size: u16,
) -> Result<Url, ProviderError> {
    let expected_path = format!("{normalized_base_path}/v1beta/models");
    let expected_query = format!("pageSize={page_size}");
    let candidate = format!("{normalized_base_url}/v1beta/models?{expected_query}");
    parse_and_verify_url(base_url, &candidate, &expected_path, Some(&expected_query))
}

fn parse_and_verify_url(
    base_url: &Url,
    candidate: &str,
    expected_path: &str,
    expected_query: Option<&str>,
) -> Result<Url, ProviderError> {
    let parsed = Url::parse(candidate).map_err(|_| {
        ProviderError::InvalidPath("request URL failed canonical parse".to_string())
    })?;

    if parsed.scheme() != base_url.scheme()
        || parsed.host_str() != base_url.host_str()
        || parsed.port_or_known_default() != base_url.port_or_known_default()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != expected_path
        || parsed.query() != expected_query
    {
        return Err(ProviderError::InvalidPath(
            "request URL failed post-parse equivalence check".to_string(),
        ));
    }

    Ok(parsed)
}

fn validate_model_path_rest(raw_path: &str, rest: &str) -> Result<(), ProviderError> {
    if rest.is_empty() {
        return Err(ProviderError::InvalidPath(
            "request path must include a model target after '/v1beta/models/'".to_string(),
        ));
    }
    if rest.split('/').any(str::is_empty) {
        return Err(ProviderError::InvalidPath(
            "request path must not contain empty segments or a trailing '/'".to_string(),
        ));
    }

    validate_url_path(raw_path).map_err(|err| match err {
        CoreError::InvalidBaseUrl { reason } => {
            ProviderError::InvalidPath(reason.replace("base URL path", "request path"))
        }
        other => ProviderError::InvalidPath(other.to_string()),
    })
}

fn decode_form_component(raw: &str) -> Result<String, ProviderError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let (Some(&hi_b), Some(&lo_b)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                    return Err(ProviderError::InvalidPath(
                        "request query contains a truncated percent-encoding sequence".to_string(),
                    ));
                };
                let (Some(hi), Some(lo)) =
                    (decode_ascii_hex_nibble(hi_b), decode_ascii_hex_nibble(lo_b))
                else {
                    return Err(ProviderError::InvalidPath(
                        "request query contains a malformed percent-encoding sequence".to_string(),
                    ));
                };
                out.push((hi << 4) | lo);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| {
        ProviderError::InvalidPath(
            "request query contains invalid UTF-8 percent-encoding".to_string(),
        )
    })
}

fn sanitize_query(raw_query: &str) -> Result<Option<String>, ProviderError> {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_val_opt) = match pair.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (pair, None),
        };

        let decoded_key = decode_form_component(raw_key)?;
        if decoded_key.chars().any(char::is_control) {
            return Err(ProviderError::InvalidPath(
                "request query parameter name contains control characters".to_string(),
            ));
        }
        if decoded_key.contains('%') {
            return Err(ProviderError::InvalidPath(
                "request query parameter name contains nested percent-encoding".to_string(),
            ));
        }

        let decoded_val = match raw_val_opt {
            Some(raw_val) => {
                let value = decode_form_component(raw_val)?;
                if value.chars().any(char::is_control) {
                    return Err(ProviderError::InvalidPath(
                        "request query parameter value contains control characters".to_string(),
                    ));
                }
                Some(value)
            }
            None => None,
        };

        if decoded_key.trim().eq_ignore_ascii_case("key") {
            continue;
        }

        if decoded_key.is_empty() || !decoded_key.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            return Err(ProviderError::InvalidPath(
                "request query parameter name must be non-empty printable ASCII".to_string(),
            ));
        }

        match decoded_val {
            Some(val) => {
                serializer.append_pair(&decoded_key, &val);
            }
            None => {
                serializer.append_key_only(&decoded_key);
            }
        }
    }

    let encoded = serializer.finish();
    Ok(if encoded.is_empty() {
        None
    } else {
        Some(encoded)
    })
}

pub(super) fn validate_api_key_string(raw: &str) -> Result<Option<String>, ProviderError> {
    let Some(trimmed) = validate_api_key_str(raw).map_err(|_| {
        ProviderError::InvalidConfig("api key contains invalid header characters".to_string())
    })?
    else {
        return Ok(None);
    };

    if HeaderValue::from_str(trimmed).is_err() {
        return Err(ProviderError::InvalidConfig(
            "api key contains invalid header characters".to_string(),
        ));
    }

    Ok(Some(trimmed.to_string()))
}

pub(super) fn build_api_key_header(raw: &str) -> Result<Option<HeaderValue>, ProviderError> {
    let Some(trimmed) = validate_api_key_str(raw).map_err(|_| {
        ProviderError::InvalidConfig("api key contains invalid header characters".to_string())
    })?
    else {
        return Ok(None);
    };

    let mut hv = HeaderValue::from_str(trimmed).map_err(|_| {
        ProviderError::InvalidConfig("api key contains invalid header characters".to_string())
    })?;
    hv.set_sensitive(true);
    Ok(Some(hv))
}

pub(super) fn resolve_content_type_header(
    content_type: Option<&str>,
) -> Result<HeaderValue, ProviderError> {
    let Some(raw) = content_type else {
        return Ok(HeaderValue::from_static("application/json"));
    };

    if !is_printable_ascii_with_space(raw.as_bytes()) {
        return Err(ProviderError::InvalidConfig(
            "Content-Type header contains invalid characters".to_string(),
        ));
    }

    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return Err(ProviderError::InvalidConfig(
            "Content-Type header must not be empty".to_string(),
        ));
    }

    HeaderValue::from_str(trimmed).map_err(|_| {
        ProviderError::InvalidConfig("Content-Type header contains invalid characters".to_string())
    })
}
