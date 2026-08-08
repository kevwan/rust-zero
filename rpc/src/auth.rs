use std::sync::Arc;

use tonic::{
    metadata::{Ascii, MetadataValue},
    service::Interceptor,
    Request, Status,
};

/// Adds a bearer credential to every outgoing RPC request.
#[derive(Clone)]
pub struct BearerToken {
    authorization: MetadataValue<Ascii>,
}

impl BearerToken {
    pub fn new(token: &str) -> Result<Self, tonic::metadata::errors::InvalidMetadataValue> {
        let authorization = format!("Bearer {token}").parse()?;
        Ok(Self { authorization })
    }

    pub(crate) fn authorization(&self) -> MetadataValue<Ascii> {
        self.authorization.clone()
    }
}

impl Interceptor for BearerToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.authorization.clone());
        Ok(request)
    }
}

type Validator<T> = dyn Fn(&str) -> Option<T> + Send + Sync;

/// Validates bearer credentials on incoming RPC requests.
pub struct RpcBearerAuth<T> {
    validator: Arc<Validator<T>>,
}

impl<T> Clone for RpcBearerAuth<T> {
    fn clone(&self) -> Self {
        Self {
            validator: Arc::clone(&self.validator),
        }
    }
}

impl<T> RpcBearerAuth<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(validator: impl Fn(&str) -> Option<T> + Send + Sync + 'static) -> Self {
        Self {
            validator: Arc::new(validator),
        }
    }

    /// Returns the identity installed in a validated request.
    pub fn authenticated<U>(request: &Request<U>) -> Option<T> {
        request.extensions().get::<T>().cloned()
    }
}

impl<T> Interceptor for RpcBearerAuth<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let identity = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .and_then(|token| (self.validator)(token))
            .ok_or_else(|| Status::unauthenticated("valid bearer credentials are required"))?;

        request.extensions_mut().insert(identity);
        Ok(request)
    }
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer")
        && !token.is_empty()
        && !token.contains(char::is_whitespace))
    .then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn client_and_server_interceptors_exchange_identity() {
        let mut client = BearerToken::new("valid").unwrap();
        let request = client.call(Request::new(())).unwrap();
        let mut server =
            RpcBearerAuth::new(|token| (token == "valid").then(|| "service-account".to_owned()));
        let request = server.call(request).unwrap();

        assert_eq!(
            RpcBearerAuth::<String>::authenticated(&request),
            Some("service-account".to_owned())
        );
    }

    #[test]
    fn server_rejects_missing_credentials() {
        let mut server = RpcBearerAuth::new(|_| Some(()));
        let error = server.call(Request::new(())).unwrap_err();

        assert_eq!(error.code(), Code::Unauthenticated);
    }
}
