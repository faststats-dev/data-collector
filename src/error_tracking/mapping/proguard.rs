use super::{MappingRequest, MappingResolver};
use uuid::Uuid;

const PROGUARD_DIR: &str = "proguard";

pub(super) async fn apply(
    resolver: &MappingResolver,
    request: MappingRequest<'_>,
) -> Option<String> {
    let mapping = resolver
        .load_proguard_mapping(request.project_id, request.build_id)
        .await?;
    let mapped_stacktrace = mapping.retrace(request.stacktrace);
    (mapped_stacktrace != request.stacktrace).then_some(mapped_stacktrace)
}

pub(super) fn s3_prefix(project_id: Uuid, build_id: &str) -> String {
    format!("{project_id}/{build_id}/{PROGUARD_DIR}/")
}

#[cfg(test)]
mod tests {
    use super::s3_prefix;
    use uuid::Uuid;

    #[test]
    fn builds_proguard_s3_prefix_with_trailing_slash() {
        let project_id = Uuid::parse_str("01954b9b-7b1d-72b8-8af3-f8d058f60b79").unwrap();
        assert_eq!(
            s3_prefix(project_id, "build-1"),
            "01954b9b-7b1d-72b8-8af3-f8d058f60b79/build-1/proguard/"
        );
    }
}
