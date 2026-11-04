//! Apps-controller immutable deployment identity allocation.

use std::time::{SystemTime, UNIX_EPOCH};

use dotcore_domain::valid_id;

use super::{ApplicationLifecycleError, configuration, configuration_with_source};

/// Produces a cryptographically random bounded deployment-ID suffix.
pub(super) fn random_collision_suffix() -> Result<u64, ApplicationLifecycleError> {
    let mut bytes = [0_u8; 5];
    getrandom::fill(&mut bytes).map_err(|source| {
        configuration_with_source("Application identity entropy failed", EntropyError(source))
    })?;
    Ok(bytes
        .into_iter()
        .fold(0_u64, |value, byte| (value << 8) | u64::from(byte)))
}

/// Allocates an immutable deployment identity that no declaration uses. The hosting node fails
/// closed on application data it holds under an identity without ownership evidence.
pub(super) fn allocate_deployment_id<'a>(
    application_id: &str,
    now: SystemTime,
    existing_ids: impl IntoIterator<Item = &'a str>,
    suffixes: impl IntoIterator<Item = Result<u64, ApplicationLifecycleError>>,
) -> Result<String, ApplicationLifecycleError> {
    let duration = now.duration_since(UNIX_EPOCH).map_err(|source| {
        configuration_with_source("System clock is before the Unix epoch", source)
    })?;
    let timestamp = deployment_id_timestamp(duration)?;
    let association = deployment_id_association(application_id);
    let existing_ids = existing_ids
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    for suffix in suffixes {
        let id = format!(
            "{association}-{timestamp}-{:010x}",
            suffix? & 0xff_ffff_ffff
        );
        if id.len() <= 40 && valid_id(&id) && !existing_ids.contains(id.as_str()) {
            return Ok(id);
        }
    }
    Err(configuration(
        "Application deployment ID allocation exhausted collision retries",
    ))
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct EntropyError(getrandom::Error);

fn deployment_id_timestamp(
    duration: std::time::Duration,
) -> Result<String, ApplicationLifecycleError> {
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| {
        configuration("System clock timestamp exceeds the supported signed Unix timestamp range")
    })?;
    let timestamp = time::OffsetDateTime::from_unix_timestamp(seconds).map_err(|source| {
        configuration_with_source("System clock is after the supported UTC date range", source)
    })?;
    Ok(format!(
        "{:02}{:02}{:02}t{:02}{:02}{:02}z",
        timestamp.year().rem_euclid(100),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    ))
}

fn deployment_id_association(application_id: &str) -> String {
    const MAX_ASSOCIATION_LENGTH: usize = 14;
    if application_id.len() <= MAX_ASSOCIATION_LENGTH {
        return application_id.to_owned();
    }
    let mut hash = 0x811c9dc5_u32;
    for byte in application_id.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{}-{:05x}", &application_id[..8], hash & 0x0f_ffff)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{allocate_deployment_id, deployment_id_timestamp};

    #[test]
    fn deployment_id_timestamp_formats_utc_calendar_boundaries() {
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(0)).expect("Unix epoch"),
            "700101t000000z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(951_782_400)).expect("leap day"),
            "000229t000000z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(4_107_542_400)).expect("non-leap year"),
            "000301t000000z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(946_684_799)).expect("year end"),
            "991231t235959z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(946_684_800)).expect("year rollover"),
            "000101t000000z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(86_399)).expect("day end"),
            "700101t235959z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::from_secs(86_400)).expect("next day"),
            "700102t000000z"
        );
        assert_eq!(
            deployment_id_timestamp(Duration::new(1, 999_999_999)).expect("truncate nanos"),
            "700101t000001z"
        );
    }

    #[test]
    fn allocate_deployment_id_reports_pre_epoch_clock_context() {
        let error = allocate_deployment_id(
            "application",
            UNIX_EPOCH - Duration::from_secs(1),
            std::iter::empty(),
            std::iter::empty(),
        )
        .expect_err("pre-epoch clock");

        assert_eq!(
            error.to_string(),
            "Application lifecycle configuration failed: System clock is before the Unix epoch"
        );
        let crate::application::controller::ApplicationLifecycleError::Configuration(source) =
            error
        else {
            panic!("expected configuration error");
        };
        assert_eq!(
            source.root_cause().to_string(),
            "second time provided was later than self"
        );
    }

    #[test]
    fn deployment_id_timestamp_rejects_unsupported_ranges() {
        let seconds_overflow = deployment_id_timestamp(Duration::new(i64::MAX as u64 + 1, 0))
            .expect_err("seconds beyond signed range");
        assert_eq!(
            seconds_overflow.to_string(),
            "Application lifecycle configuration failed: System clock timestamp exceeds the supported signed Unix timestamp range"
        );

        let date_overflow = deployment_id_timestamp(Duration::new(i64::MAX as u64, 0))
            .expect_err("timestamp beyond UTC date range");
        assert_eq!(
            date_overflow.to_string(),
            "Application lifecycle configuration failed: System clock is after the supported UTC date range"
        );
    }
}
