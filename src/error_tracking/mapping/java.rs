use super::{MappedStacktrace, MappingRequest, MappingResolver};
use uuid::Uuid;

const PROGUARD_DIR: &str = "proguard";
const MAPPING_KIND: &str = "r8";

pub async fn apply(
    resolver: &MappingResolver,
    request: MappingRequest<'_>,
) -> Option<MappedStacktrace> {
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
        mapping_used: format!("{MAPPING_KIND}:{}", request.build_id),
    })
}

pub(super) fn proguard_s3_prefix(project_id: Uuid, build_id: &str) -> String {
    format!("{project_id}/{build_id}/{PROGUARD_DIR}/")
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
