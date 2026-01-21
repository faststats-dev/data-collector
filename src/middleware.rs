use axum::body::Body;
use axum::http::{HeaderValue, Request};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{Layer, Service};

/// Middleware that detects compression from magic bytes and sets Content-Encoding header.
/// This allows sendBeacon requests (which can't set custom headers) to work with
/// tower-http's RequestDecompressionLayer.
#[derive(Clone)]
pub struct DetectEncodingLayer;

impl<S> Layer<S> for DetectEncodingLayer {
    type Service = DetectEncodingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        DetectEncodingService { inner }
    }
}

#[derive(Clone)]
pub struct DetectEncodingService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for DetectEncodingService<S>
where
    S: Service<Request<Body>, Response = axum::response::Response> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Skip if Content-Encoding is already set
            if req.headers().contains_key("content-encoding") {
                return inner.call(req).await;
            }

            let (parts, body) = req.into_parts();

            let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => {
                    let req = Request::from_parts(parts, Body::empty());
                    return inner.call(req).await;
                }
            };

            let encoding = detect_encoding(&bytes);
            let mut req = Request::from_parts(parts, Body::from(bytes));

            if let Some(enc) = encoding {
                req.headers_mut()
                    .insert("content-encoding", HeaderValue::from_static(enc));
            }

            inner.call(req).await
        })
    }
}

fn detect_encoding(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        Some("gzip")
    } else if data.len() >= 4 && data[0..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        Some("zstd")
    } else if data.len() >= 2
        && data[0] == 0x78
        && (data[1] == 0x01 || data[1] == 0x5e || data[1] == 0x9c || data[1] == 0xda)
    {
        Some("deflate")
    } else {
        None
    }
}
