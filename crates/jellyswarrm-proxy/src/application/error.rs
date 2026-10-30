//! Cause-preserving workflow errors. HTTP contracts remain endpoint-specific.

use std::{error::Error, fmt};

use hyper::StatusCode;

/// Workflow context, not a response code. Endpoint adapters own its HTTP policy.
pub(crate) enum FailureStage {
    CallerAuthentication,
    CatalogEncoding,
    CatalogDeadline,
    NegotiationSelection,
    DeliverySelection,
    LiveBindingLookup,
    LiveSessionLookup,
    PlaybackCommit,
    ResourceLookup,
    HlsStatus,
    HlsTranslation,
    HlsDeadline,
}

pub(crate) enum ApplicationError {
    InvalidInput(&'static str),
    InvalidRequestJson(serde_json::Error),
    InvalidRouting(anyhow::Error),
    RequestBody(anyhow::Error),
    PlaylistResponse(crate::processors::response_processor::PlaylistResponseError),
    PlaybackResolution(super::playback::target::ResolutionError),
    TargetSelection(super::context::TargetError),
    At {
        stage: FailureStage,
        cause: Box<ApplicationError>,
    },
    AccessDenied,
    MissingIdentity(&'static str),
    UnavailableOwner,
    UpstreamTransport(reqwest::Error),
    UpstreamStatus(StatusCode),
    InvalidUpstreamJson(serde_json::Error),
    InvalidUpstreamPayload(&'static str),
    InvalidProcessedJson(serde_json::Error),
    Storage(sqlx::Error),
    Serialization(serde_json::Error),
    Internal(anyhow::Error),
    SourcesFailed(Vec<ApplicationError>),
    Shared(std::sync::Arc<ApplicationError>),
    IncompleteCatalog {
        reason: super::catalog::scan::PartialReason,
        cause: Option<Box<ApplicationError>>,
    },
}

impl ApplicationError {
    pub(crate) fn at(self, stage: FailureStage) -> Self {
        Self::At {
            stage,
            cause: Box::new(self),
        }
    }
    /// Configured-library fallback applies to source failures, not local denial.
    pub(crate) fn allows_detail_fallback(&self) -> bool {
        match self {
            Self::At { cause, .. } => cause.allows_detail_fallback(),
            Self::Shared(cause) => cause.allows_detail_fallback(),
            Self::UpstreamTransport(_)
            | Self::InvalidUpstreamJson(_)
            | Self::InvalidUpstreamPayload(_) => true,
            Self::UpstreamStatus(status) => {
                matches!(
                    *status,
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND
                ) || status.is_server_error()
            }
            _ => false,
        }
    }
}

impl From<crate::outbound::json::JsonBodyError> for ApplicationError {
    fn from(error: crate::outbound::json::JsonBodyError) -> Self {
        use crate::outbound::json::JsonBodyError;
        match error {
            JsonBodyError::InvalidInput(reason) => Self::InvalidInput(reason),
            JsonBodyError::Decode(error) => Self::InvalidRequestJson(error),
            JsonBodyError::Encode(error) => Self::Serialization(error),
            JsonBodyError::Header(error) => Self::Internal(error.into()),
        }
    }
}

/// Request construction failures are routing failures; storage causes stay internal.
impl From<crate::application::request::UpstreamBuildError> for ApplicationError {
    fn from(error: crate::application::request::UpstreamBuildError) -> Self {
        super::input::routing_error(error.0)
    }
}

impl From<crate::processors::error::ProcessingError> for ApplicationError {
    fn from(error: crate::processors::error::ProcessingError) -> Self {
        use crate::processors::error::ProcessingError;
        match error {
            ProcessingError::RequestBody(error) => super::input::request_body_error(error),
            ProcessingError::Response(error) => Self::Internal(error),
            ProcessingError::Playlist(error) => Self::PlaylistResponse(error),
            ProcessingError::JsonBody(error) => error.into(),
        }
    }
}

// Deliberately do not format causes: transport URLs, payload validation errors,
// and preparation errors may contain credentials or private upstream data.
impl fmt::Display for ApplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // These descriptions are static call-site labels, never request data.
        match self {
            Self::InvalidInput(reason) => return write!(f, "invalid operation input: {reason}"),
            Self::InvalidUpstreamPayload(reason) => {
                return write!(f, "invalid upstream payload: {reason}")
            }
            Self::MissingIdentity(identity) => {
                return write!(f, "missing operation identity: {identity}")
            }
            _ => {}
        }
        f.write_str(match self {
            Self::InvalidInput(_) => "invalid operation input",
            Self::InvalidRequestJson(_) => "invalid request JSON",
            Self::InvalidRouting(_) => "invalid operation routing",
            Self::RequestBody(_) => "request body translation failed",
            Self::PlaylistResponse(_) => "playlist response translation failed",
            Self::PlaybackResolution(_) => "playback authority resolution failed",
            Self::TargetSelection(_) => "target selection failed",
            Self::At { .. } => "workflow stage failed",
            Self::AccessDenied => "access denied",
            Self::MissingIdentity(_) => "missing operation identity",
            Self::UnavailableOwner => "selected owner unavailable",
            Self::UpstreamTransport(_) => "upstream transport failure",
            Self::UpstreamStatus(_) => "upstream response failure",
            Self::InvalidUpstreamJson(_) => "invalid upstream JSON",
            Self::InvalidUpstreamPayload(_) => "invalid upstream payload",
            Self::InvalidProcessedJson(_) => "invalid processed JSON",
            Self::Storage(_) => "workflow storage failure",
            Self::Serialization(_) => "workflow serialization failure",
            Self::Internal(_) => "workflow preparation failure",
            Self::SourcesFailed(_) => "catalog sources failed",
            Self::Shared(_) => "shared catalog workflow failure",
            Self::IncompleteCatalog { .. } => "incomplete catalog inventory",
        })
    }
}

impl fmt::Debug for ApplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for ApplicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UpstreamTransport(error) => Some(error),
            Self::PlaybackResolution(error) => Some(error),
            Self::TargetSelection(error) => Some(error),
            Self::InvalidRequestJson(error)
            | Self::InvalidUpstreamJson(error)
            | Self::InvalidProcessedJson(error)
            | Self::Serialization(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::Internal(error) | Self::InvalidRouting(error) => Some(error.as_ref()),
            Self::RequestBody(error) => Some(error.as_ref()),
            Self::PlaylistResponse(error) => Some(error),
            Self::At { cause, .. } => Some(cause.as_ref()),
            Self::SourcesFailed(causes) => causes.first().map(|cause| cause as _),
            Self::Shared(cause) => Some(cause.as_ref()),
            Self::IncompleteCatalog { cause, .. } => cause.as_deref().map(|cause| cause as _),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_conversion_classifies_body_failures_without_losing_storage_causes() {
        use crate::processors::{error::ProcessingError, ProcessingErrors};
        for storage_failed in [false, true] {
            let mut causes = vec![anyhow::anyhow!("invalid selection")];
            if storage_failed {
                causes.push(anyhow::Error::new(sqlx::Error::PoolClosed).context("body field"));
            }
            let error = ApplicationError::from(ProcessingError::RequestBody(
                ProcessingErrors { causes }.into(),
            ));
            assert_eq!(
                matches!(error, ApplicationError::Internal(_)),
                storage_failed
            );
            assert_eq!(
                matches!(error, ApplicationError::RequestBody(_)),
                !storage_failed
            );
            let cause = error
                .source()
                .unwrap()
                .downcast_ref::<ProcessingErrors>()
                .unwrap();
            assert_eq!(
                ProcessingErrors::contains_cause::<sqlx::Error>(cause),
                storage_failed
            );
        }
    }

    #[test]
    fn outgoing_json_conversion_preserves_request_and_encoding_causes() {
        use crate::outbound::json::JsonBodyError;
        let json_error = || serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        for (body_error, decoding) in [
            (JsonBodyError::Decode(json_error()), true),
            (JsonBodyError::Encode(json_error()), false),
        ] {
            let error = ApplicationError::from(body_error);
            assert_eq!(
                matches!(error, ApplicationError::InvalidRequestJson(_)),
                decoding
            );
            assert_eq!(
                matches!(error, ApplicationError::Serialization(_)),
                !decoding
            );
            assert!(error.source().unwrap().is::<serde_json::Error>());
        }
    }

    #[test]
    fn endpoint_contract_retains_typed_cause_without_reclassifying_upstream_denial() {
        let error =
            ApplicationError::Storage(sqlx::Error::RowNotFound).at(FailureStage::HlsTranslation);
        assert!(!error.allows_detail_fallback());
        let wrapped = error
            .source()
            .unwrap()
            .downcast_ref::<ApplicationError>()
            .unwrap();
        assert!(matches!(wrapped, ApplicationError::Storage(_)));
        assert!(wrapped.source().unwrap().is::<sqlx::Error>());
        let upstream = ApplicationError::UpstreamStatus(StatusCode::FORBIDDEN)
            .at(FailureStage::PlaybackCommit);
        assert!(upstream.allows_detail_fallback());
        assert!(matches!(
            upstream
                .source()
                .unwrap()
                .downcast_ref::<ApplicationError>(),
            Some(ApplicationError::UpstreamStatus(StatusCode::FORBIDDEN))
        ));
        assert!(!matches!(
            upstream
                .source()
                .unwrap()
                .downcast_ref::<ApplicationError>(),
            Some(ApplicationError::AccessDenied)
        ));
    }

    #[test]
    fn upstream_forbidden_and_local_denial_have_distinct_fallback_policy() {
        let upstream = ApplicationError::UpstreamStatus(StatusCode::FORBIDDEN);
        let local = ApplicationError::AccessDenied;
        assert!(upstream.allows_detail_fallback());
        assert!(!local.allows_detail_fallback());
        assert!(ApplicationError::UpstreamStatus(StatusCode::UNAUTHORIZED).allows_detail_fallback());
    }

    #[test]
    fn storage_and_preparation_causes_are_preserved() {
        let storage = ApplicationError::Storage(sqlx::Error::RowNotFound);
        assert!(storage.source().unwrap().is::<sqlx::Error>());
        let preparation = ApplicationError::Internal(anyhow::Error::new(std::io::Error::other(
            "private preparation detail",
        )));
        assert!(preparation.source().unwrap().is::<std::io::Error>());
        assert!(!format!("{preparation:?}").contains("private"));
    }

    #[test]
    fn routing_cause_is_preserved_without_exposing_request_details() {
        let error = ApplicationError::InvalidRouting(anyhow::Error::new(std::io::Error::other(
            "private routing context",
        )));
        assert!(!error.allows_detail_fallback());
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert!(!format!("{error:?}").contains("private"));
    }
}
