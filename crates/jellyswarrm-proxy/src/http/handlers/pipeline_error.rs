//! HTTP compatibility policy for typed pipeline failures. Workflow code reports
//! a cause and stage; only this endpoint adapter chooses response status codes.
use crate::application::{
    context::TargetError,
    error::{ApplicationError, FailureStage},
    playback::target::ResolutionError,
};
use hyper::StatusCode;

fn target_status(error: &TargetError) -> StatusCode {
    match error {
        TargetError::AccessDenied | TargetError::ViewerMismatch => StatusCode::FORBIDDEN,
        TargetError::UnavailableOwner => StatusCode::SERVICE_UNAVAILABLE,
    }
}

impl ApplicationError {
    /// Also used by transitional status-only wrappers outside the typed pipeline.
    pub(crate) fn detail_status(&self) -> StatusCode {
        match self {
            Self::At { stage, cause } => match stage {
                FailureStage::CallerAuthentication => StatusCode::UNAUTHORIZED,
                FailureStage::CatalogEncoding => StatusCode::FORBIDDEN,
                FailureStage::CatalogDeadline => StatusCode::GATEWAY_TIMEOUT,
                FailureStage::PlaybackCommit => StatusCode::SERVICE_UNAVAILABLE,
                FailureStage::ResourceLookup => StatusCode::NOT_FOUND,
                FailureStage::NegotiationSelection => match cause.as_ref() {
                    Self::PlaybackResolution(
                        ResolutionError::OfflineOwner
                        | ResolutionError::MissingOwner
                        | ResolutionError::Target(TargetError::UnavailableOwner),
                    ) => StatusCode::SERVICE_UNAVAILABLE,
                    Self::PlaybackResolution(
                        ResolutionError::Target(TargetError::ViewerMismatch)
                        | ResolutionError::ViewerMismatch(_),
                    ) => StatusCode::FORBIDDEN,
                    Self::PlaybackResolution(ResolutionError::Storage(_)) => {
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                    _ => StatusCode::BAD_REQUEST,
                },
                FailureStage::DeliverySelection => match cause.as_ref() {
                    Self::PlaybackResolution(
                        ResolutionError::ViewerMismatch(_)
                        | ResolutionError::Target(
                            TargetError::AccessDenied | TargetError::ViewerMismatch,
                        ),
                    ) => StatusCode::FORBIDDEN,
                    Self::PlaybackResolution(ResolutionError::Target(
                        TargetError::UnavailableOwner,
                    )) => StatusCode::UNAUTHORIZED,
                    Self::PlaybackResolution(
                        ResolutionError::OfflineOwner | ResolutionError::MissingOwner,
                    ) => StatusCode::NOT_FOUND,
                    Self::PlaybackResolution(ResolutionError::Storage(_)) => {
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                    _ => StatusCode::BAD_REQUEST,
                },
                FailureStage::LiveBindingLookup => match cause.as_ref() {
                    Self::PlaybackResolution(ResolutionError::Target(error)) => {
                        target_status(error)
                    }
                    _ => StatusCode::BAD_REQUEST,
                },
                FailureStage::LiveSessionLookup => match cause.as_ref() {
                    Self::PlaybackResolution(ResolutionError::ViewerMismatch(_)) => {
                        StatusCode::FORBIDDEN
                    }
                    _ => StatusCode::BAD_REQUEST,
                },
            },
            Self::TargetSelection(error) => target_status(error),
            Self::InvalidInput(_)
            | Self::InvalidRouting(_)
            | Self::InvalidRequestJson(_)
            | Self::PlaybackResolution(_) => StatusCode::BAD_REQUEST,
            Self::AccessDenied => StatusCode::FORBIDDEN,
            Self::MissingIdentity(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::UnavailableOwner | Self::SourcesFailed(_) | Self::IncompleteCatalog { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Shared(cause) => cause.detail_status(),
            Self::UpstreamTransport(_)
            | Self::InvalidUpstreamJson(_)
            | Self::InvalidUpstreamPayload(_) => StatusCode::BAD_GATEWAY,
            Self::UpstreamStatus(status) | Self::Compatibility(status) => *status,
            Self::Endpoint { status, .. } => *status,
            Self::Storage(_)
            | Self::Serialization(_)
            | Self::InvalidProcessedJson(_)
            | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn endpoint_policy_distinguishes_selection_stages_without_flattening_causes() {
        for (stage, expected) in [
            (
                FailureStage::NegotiationSelection,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (FailureStage::DeliverySelection, StatusCode::NOT_FOUND),
            (FailureStage::LiveBindingLookup, StatusCode::BAD_REQUEST),
        ] {
            let error =
                ApplicationError::PlaybackResolution(ResolutionError::OfflineOwner).at(stage);
            assert_eq!(error.detail_status(), expected);
            let cause = error
                .source()
                .unwrap()
                .downcast_ref::<ApplicationError>()
                .unwrap();
            assert!(matches!(
                cause,
                ApplicationError::PlaybackResolution(ResolutionError::OfflineOwner)
            ));
            assert!(cause.source().unwrap().is::<ResolutionError>());
        }
        let upstream = ApplicationError::UpstreamStatus(StatusCode::FORBIDDEN)
            .at(FailureStage::PlaybackCommit);
        assert_eq!(upstream.detail_status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(upstream.allows_detail_fallback());
        assert!(!ApplicationError::AccessDenied
            .at(FailureStage::PlaybackCommit)
            .allows_detail_fallback());
    }
}
