use super::{MAX_RETRIES, QueuedEvent, calculate_retry_delay};
use crate::tinybird::TinybirdClient;
use collector_message::{ErrorOccurrence, ModsEvent, WebEvent, WebVital};
use tracing::{error, warn};

#[derive(Debug, Default)]
pub(super) struct Batch {
    web_events: Vec<WebEvent>,
    mods_events: Vec<ModsEvent>,
    error_occurrences_v3: Vec<ErrorOccurrence>,
    web_vitals: Vec<WebVital>,
}

impl Batch {
    pub(super) fn is_empty(&self) -> bool {
        self.web_events.is_empty()
            && self.mods_events.is_empty()
            && self.error_occurrences_v3.is_empty()
            && self.web_vitals.is_empty()
    }

    pub(super) fn total_count(&self) -> usize {
        self.web_events.len()
            + self.mods_events.len()
            + self.error_occurrences_v3.len()
            + self.web_vitals.len()
    }

    pub(super) fn push(&mut self, event: QueuedEvent) {
        match event {
            QueuedEvent::WebEvent { row } => self.web_events.push(*row),
            QueuedEvent::ModsEvent { row } => self.mods_events.push(row),
            QueuedEvent::ErrorOccurrenceV3 { row, .. } => self.error_occurrences_v3.push(*row),
            QueuedEvent::WebVital { row } => self.web_vitals.push(row),
        }
    }
}

#[derive(Debug, Default)]
struct DeliveryResult {
    retryable: Batch,
    permanent_failure_count: usize,
    errors: Vec<String>,
}

impl DeliveryResult {
    fn error_summary(&self) -> String {
        if self.errors.is_empty() {
            "unknown error".to_string()
        } else {
            self.errors.join("; ")
        }
    }
}

fn classify_delivery<T>(
    outcome: Result<(), crate::tinybird::TinybirdError>,
    rows: Vec<T>,
    datasource: &'static str,
    errors: &mut Vec<String>,
) -> (Vec<T>, usize) {
    let Err(error) = outcome else {
        return (Vec::new(), 0);
    };
    let permanence = if error.is_transient() {
        "transient"
    } else {
        "permanent"
    };
    errors.push(format!(
        "{datasource} rows={} {permanence}: {error}",
        rows.len()
    ));
    if error.is_transient() {
        (rows, 0)
    } else {
        let count = rows.len();
        (Vec::new(), count)
    }
}

pub(super) async fn send_with_retry(client: &TinybirdClient, batch: Batch) {
    let mut retry_count = 0u32;
    let mut current_batch = batch;

    loop {
        let result = send_batch(client, current_batch).await;

        if result.permanent_failure_count > 0 {
            let error_summary = result.error_summary();
            error!(
                errors = %error_summary,
                "Dropping {} events after a permanent delivery failure",
                result.permanent_failure_count,
            );
        }

        if result.retryable.is_empty() {
            return;
        }
        retry_count += 1;

        if retry_count >= MAX_RETRIES {
            let error_summary = result.error_summary();
            error!(
                errors = %error_summary,
                "Dropping {} events after {} delivery attempts",
                result.retryable.total_count(),
                retry_count
            );
            return;
        }

        let error_summary = result.error_summary();
        current_batch = result.retryable;

        let delay = calculate_retry_delay(retry_count);
        warn!(
            errors = %error_summary,
            "Batch send failed (attempt {}), retrying {} events in {:?}",
            retry_count,
            current_batch.total_count(),
            delay
        );

        tokio::time::sleep(delay).await;
    }
}

async fn send_batch(client: &TinybirdClient, batch: Batch) -> DeliveryResult {
    let mut result = DeliveryResult::default();

    let Batch {
        web_events,
        mods_events,
        error_occurrences_v3,
        web_vitals,
    } = batch;

    let (web_events_res, mods_events_res, error_occurrences_v3_res, web_vitals_res) = tokio::join!(
        client.insert_web_events(&web_events),
        client.insert_mods_events(&mods_events),
        client.insert_error_occurrences_v3(&error_occurrences_v3),
        client.insert_web_vitals(&web_vitals),
    );

    let (retryable, permanent) =
        classify_delivery(web_events_res, web_events, "web_events", &mut result.errors);
    result.retryable.web_events = retryable;
    result.permanent_failure_count += permanent;

    let (retryable, permanent) = classify_delivery(
        mods_events_res,
        mods_events,
        "mods_events",
        &mut result.errors,
    );
    result.retryable.mods_events = retryable;
    result.permanent_failure_count += permanent;

    let (retryable, permanent) = classify_delivery(
        error_occurrences_v3_res,
        error_occurrences_v3,
        "error_tracking_v3",
        &mut result.errors,
    );
    result.retryable.error_occurrences_v3 = retryable;
    result.permanent_failure_count += permanent;

    let (retryable, permanent) =
        classify_delivery(web_vitals_res, web_vitals, "web_vitals", &mut result.errors);
    result.retryable.web_vitals = retryable;
    result.permanent_failure_count += permanent;

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_delivery_per_datasource() {
        let mut errors = Vec::new();
        let (retryable, permanent) =
            classify_delivery(Ok(()), vec![1, 2], "successful_source", &mut errors);
        assert!(retryable.is_empty());
        assert_eq!(permanent, 0);
        assert!(errors.is_empty());

        let (retryable, permanent) = classify_delivery(
            Err(crate::tinybird::TinybirdError::Api {
                status: 503,
                message: "unavailable".into(),
            }),
            vec![1, 2],
            "transient_source",
            &mut errors,
        );
        assert_eq!(retryable, vec![1, 2]);
        assert_eq!(permanent, 0);

        let (retryable, permanent) = classify_delivery(
            Err(crate::tinybird::TinybirdError::Api {
                status: 400,
                message: "invalid".into(),
            }),
            vec![3, 4, 5],
            "permanent_source",
            &mut errors,
        );
        assert!(retryable.is_empty());
        assert_eq!(permanent, 3);
        assert_eq!(errors.len(), 2);
    }
}
