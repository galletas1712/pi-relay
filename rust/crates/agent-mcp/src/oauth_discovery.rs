use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationMetadata;
use rmcp::transport::AuthorizationManager;

pub(crate) async fn discover_metadata(
    manager: &AuthorizationManager,
) -> Result<AuthorizationMetadata, AuthError> {
    manager.discover_metadata().await
}

pub(crate) fn normalize_scopes(scopes: Option<Vec<String>>) -> Option<Vec<String>> {
    let scopes = scopes?;
    let mut normalized = Vec::new();
    for scope in scopes {
        let scope = scope.trim();
        if !scope.is_empty() && !normalized.iter().any(|existing| existing == scope) {
            normalized.push(scope.to_string());
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}
