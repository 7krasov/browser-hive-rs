/// Extract domain from URL
pub fn extract_domain(url: &str) -> anyhow::Result<String> {
    url::Url::parse(url)?
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| anyhow::anyhow!("No host in URL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_domain() {
        assert_eq!(
            extract_domain("https://example.com/path").unwrap(),
            "example.com"
        );
        assert_eq!(
            extract_domain("http://sub.example.com:8080/path").unwrap(),
            "sub.example.com"
        );
    }
}
