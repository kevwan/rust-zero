use actix_web::{dev::Payload, error::ErrorBadRequest, web, Error, FromRequest, HttpRequest};
use futures::future::LocalBoxFuture;
use rust_zero_core::Validate;
use serde::de::DeserializeOwned;
use std::ops::{Deref, DerefMut};

/// JSON extractor that runs the request type's [`Validate`] implementation.
#[derive(Debug)]
pub struct ValidatedJson<T>(pub T);

impl<T> Deref for ValidatedJson<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedJson<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedJson<T>
where
    T: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let extraction = web::Json::<T>::from_request(request, payload);
        Box::pin(async move {
            let value = extraction.await?.into_inner();
            value.validate().map_err(ErrorBadRequest)?;
            Ok(Self(value))
        })
    }
}

/// Query-string extractor that runs the request type's [`Validate`] implementation.
#[derive(Debug)]
pub struct ValidatedQuery<T>(pub T);

impl<T> Deref for ValidatedQuery<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedQuery<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> FromRequest for ValidatedQuery<T>
where
    T: DeserializeOwned + Validate + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(request: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let extraction = web::Query::<T>::from_request(request, payload);
        Box::pin(async move {
            let value = extraction.await?.into_inner();
            value.validate().map_err(ErrorBadRequest)?;
            Ok(Self(value))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::StatusCode, test, web, App, HttpResponse};
    use rust_zero_core::{Validation, ValidationErrors};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Request {
        name: String,
    }

    impl Validate for Request {
        fn validate(&self) -> Result<(), ValidationErrors> {
            let mut validation = Validation::new();
            validation.required("name", &self.name);
            validation.finish()
        }
    }

    #[actix_web::test]
    async fn rejects_invalid_json_before_the_handler() {
        let app = test::init_service(App::new().route(
            "/",
            web::post().to(|_: ValidatedJson<Request>| async { HttpResponse::Ok().finish() }),
        ))
        .await;

        let request = test::TestRequest::post()
            .uri("/")
            .set_json(serde_json::json!({ "name": " " }))
            .to_request();
        let response = test::call_service(&app, request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn accepts_valid_json_and_query_values() {
        let app = test::init_service(
            App::new()
                .route(
                    "/json",
                    web::post().to(|value: ValidatedJson<Request>| async move {
                        HttpResponse::Ok().body(value.name.clone())
                    }),
                )
                .route(
                    "/query",
                    web::get().to(|value: ValidatedQuery<Request>| async move {
                        HttpResponse::Ok().body(value.name.clone())
                    }),
                ),
        )
        .await;

        let json_response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/json")
                .set_json(serde_json::json!({ "name": "Ada" }))
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(json_response).await, "Ada");

        let query_response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/query?name=Grace")
                .to_request(),
        )
        .await;
        assert_eq!(test::read_body(query_response).await, "Grace");
    }

    #[actix_web::test]
    async fn rejects_invalid_queries_before_the_handler() {
        let app = test::init_service(App::new().route(
            "/",
            web::get().to(|_: ValidatedQuery<Request>| async { HttpResponse::Ok().finish() }),
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/?name=%20").to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn wrappers_support_mutable_dereferencing() {
        let mut json = ValidatedJson(Request {
            name: "before".to_owned(),
        });
        json.name = "after".to_owned();
        assert_eq!(json.name, "after");

        let mut query = ValidatedQuery(Request {
            name: "before".to_owned(),
        });
        query.name = "after".to_owned();
        assert_eq!(query.name, "after");
    }
}
