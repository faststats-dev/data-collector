use super::{MappedStacktrace, MappingProvider, MappingRequest, MappingResolver};
use std::future::Future;
use std::pin::Pin;

pub struct PhpMappingProvider;

impl MappingProvider for PhpMappingProvider {
    fn mapping_kind(&self) -> &'static str {
        "none"
    }

    fn apply<'a>(
        &'a self,
        _resolver: &'a MappingResolver,
        _request: MappingRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Option<MappedStacktrace>> + Send + 'a>> {
        Box::pin(async { None })
    }
}
