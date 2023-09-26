use url::Url;

/// Normalize url according to pip standards
pub fn normalize_index_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url
}
