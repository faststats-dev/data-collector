use super::{MappedStacktrace, MappingProvider, MappingRequest, MappingResolver};
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

const PROGUARD_DIR: &str = "proguard";

pub struct JavaMappingProvider;

impl MappingProvider for JavaMappingProvider {
    fn mapping_kind(&self) -> &'static str {
        "r8"
    }

    fn apply<'a>(
        &'a self,
        resolver: &'a MappingResolver,
        request: MappingRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Option<MappedStacktrace>> + Send + 'a>> {
        Box::pin(async move {
            if !resolver
                .build_exists(request.project_id, request.build_id)
                .await
            {
                return None;
            }

            let mapping = resolver
                .load_proguard_mapping(request.project_id, request.build_id)
                .await?;
            let mapped_stacktrace = mapping.retrace(request.stacktrace);
            if mapped_stacktrace == request.stacktrace {
                return None;
            }

            Some(MappedStacktrace {
                stacktrace: mapped_stacktrace,
                mapping_used: format!("{}:{}", self.mapping_kind(), request.build_id),
            })
        })
    }
}

pub(super) fn proguard_s3_prefix(project_id: Uuid, build_id: &str) -> String {
    let mut key = String::with_capacity(36 + 1 + build_id.len() + 1 + PROGUARD_DIR.len() + 1);
    use std::fmt::Write;
    let _ = write!(key, "{project_id}");
    key.push('/');
    key.push_str(build_id);
    key.push('/');
    key.push_str(PROGUARD_DIR);
    key.push('/');
    key
}

#[cfg(test)]
mod tests {
    use super::proguard_s3_prefix;
    use uuid::Uuid;

    #[test]
    fn builds_proguard_s3_prefix_with_trailing_slash() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            proguard_s3_prefix(project_id, "build-1"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/proguard/"
        );
    }
}
