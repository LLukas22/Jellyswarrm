//! Map application errors and their workflow stage to HTTP status codes.
use crate::{
    application::{
        error::{ApplicationError, FailureStage},
        playback::target::ResolutionError,
        request::context::TargetError,
    },
    processors::response_processor::PlaylistResponseError,
};
use axum::response::{IntoResponse, Response};
use hyper::StatusCode;

/// Handlers return `ApplicationError` directly; only the status code reaches the client.
impl IntoResponse for ApplicationError {
    fn into_response(self) -> Response {
        status(&self).into_response()
    }
}

fn target_status(error: &TargetError) -> StatusCode {
    match error {
        TargetError::AccessDenied | TargetError::ViewerMismatch => StatusCode::FORBIDDEN,
        TargetError::UnavailableOwner => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub(crate) fn status(error: &ApplicationError) -> StatusCode {
    match error {
        ApplicationError::At { stage, cause } => match stage {
            FailureStage::CallerAuthentication => StatusCode::UNAUTHORIZED,
            FailureStage::CatalogEncoding => StatusCode::FORBIDDEN,
            FailureStage::CatalogDeadline => StatusCode::GATEWAY_TIMEOUT,
            FailureStage::PlaybackCommit => StatusCode::SERVICE_UNAVAILABLE,
            FailureStage::ResourceLookup => StatusCode::NOT_FOUND,
            FailureStage::HlsDeadline => StatusCode::GATEWAY_TIMEOUT,
            FailureStage::HlsTranslation => StatusCode::BAD_GATEWAY,
            FailureStage::HlsStatus => match cause.as_ref() {
                ApplicationError::UpstreamStatus(status)
                    if status.is_client_error() || status.is_server_error() =>
                {
                    *status
                }
                _ => StatusCode::BAD_GATEWAY,
            },
            FailureStage::NegotiationSelection => match cause.as_ref() {
                ApplicationError::PlaybackResolution(
                    ResolutionError::OfflineOwner
                    | ResolutionError::MissingOwner
                    | ResolutionError::Target(TargetError::UnavailableOwner),
                ) => StatusCode::SERVICE_UNAVAILABLE,
                ApplicationError::PlaybackResolution(
                    ResolutionError::Target(TargetError::ViewerMismatch)
                    | ResolutionError::ViewerMismatch(_),
                ) => StatusCode::FORBIDDEN,
                ApplicationError::PlaybackResolution(ResolutionError::Storage(_)) => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
                _ => StatusCode::BAD_REQUEST,
            },
            FailureStage::DeliverySelection => match cause.as_ref() {
                ApplicationError::PlaybackResolution(
                    ResolutionError::ViewerMismatch(_)
                    | ResolutionError::Target(
                        TargetError::AccessDenied | TargetError::ViewerMismatch,
                    ),
                ) => StatusCode::FORBIDDEN,
                ApplicationError::PlaybackResolution(ResolutionError::Target(
                    TargetError::UnavailableOwner,
                )) => StatusCode::UNAUTHORIZED,
                ApplicationError::PlaybackResolution(
                    ResolutionError::OfflineOwner | ResolutionError::MissingOwner,
                ) => StatusCode::NOT_FOUND,
                ApplicationError::PlaybackResolution(ResolutionError::Storage(_)) => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
                _ => StatusCode::BAD_REQUEST,
            },
            FailureStage::LiveBindingLookup => match cause.as_ref() {
                ApplicationError::PlaybackResolution(ResolutionError::Target(error)) => {
                    target_status(error)
                }
                _ => StatusCode::BAD_REQUEST,
            },
            FailureStage::LiveSessionLookup => match cause.as_ref() {
                ApplicationError::PlaybackResolution(ResolutionError::ViewerMismatch(_)) => {
                    StatusCode::FORBIDDEN
                }
                _ => StatusCode::BAD_REQUEST,
            },
        },
        ApplicationError::TargetSelection(error) => target_status(error),
        ApplicationError::PlaylistResponse(error) => match error {
            PlaylistResponseError::IncompleteSharing => StatusCode::CONFLICT,
            PlaylistResponseError::InvalidSharing => StatusCode::BAD_GATEWAY,
            PlaylistResponseError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        },
        ApplicationError::InvalidInput(_)
        | ApplicationError::RequestBody(_)
        | ApplicationError::InvalidRouting(_)
        | ApplicationError::InvalidRequestJson(_)
        | ApplicationError::PlaybackResolution(_) => StatusCode::BAD_REQUEST,
        ApplicationError::AccessDenied => StatusCode::FORBIDDEN,
        ApplicationError::MissingIdentity(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ApplicationError::UnavailableOwner
        | ApplicationError::SourcesFailed(_)
        | ApplicationError::IncompleteCatalog { .. } => StatusCode::SERVICE_UNAVAILABLE,
        ApplicationError::Shared(cause) => status(cause),
        ApplicationError::UpstreamTransport(_)
        | ApplicationError::InvalidUpstreamJson(_)
        | ApplicationError::InvalidUpstreamPayload(_) => StatusCode::BAD_GATEWAY,
        ApplicationError::UpstreamStatus(status) => *status,
        ApplicationError::Storage(_)
        | ApplicationError::Serialization(_)
        | ApplicationError::InvalidProcessedJson(_)
        | ApplicationError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn input_aggregates_with_storage_failure_are_not_bad_requests() {
        use crate::application::request::input::{request_body_error, routing_error};
        use crate::processors::ProcessingErrors;
        for classify in [routing_error, request_body_error] {
            for storage_failed in [false, true] {
                let mut causes = vec![anyhow::anyhow!("invalid selection")];
                if storage_failed {
                    causes.push(anyhow::Error::new(sqlx::Error::PoolClosed).context("body field"));
                }
                let error = classify(ProcessingErrors { causes }.into());
                assert_eq!(
                    status(&error),
                    if storage_failed {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                );
            }
        }
    }

    #[test]
    fn json_failure_policy_and_upstream_status_preservation_belong_to_the_adapter() {
        let json_error = || serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        for (error, expected) in [
            (
                ApplicationError::InvalidInput("missing JSON body"),
                StatusCode::BAD_REQUEST,
            ),
            (
                ApplicationError::InvalidRequestJson(json_error()),
                StatusCode::BAD_REQUEST,
            ),
            (
                ApplicationError::InvalidUpstreamJson(json_error()),
                StatusCode::BAD_GATEWAY,
            ),
            (
                ApplicationError::InvalidProcessedJson(json_error()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(status(&error), expected);
        }
        for expected in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert_eq!(
                status(&ApplicationError::UpstreamStatus(expected)),
                expected
            );
        }
    }

    #[test]
    fn routing_policy_distinguishes_storage_denial_availability_and_input() {
        use crate::application::request::routing::RoutingError;
        for (error, expected) in [
            (
                RoutingError::Storage(sqlx::Error::PoolClosed),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (RoutingError::AccessDenied, StatusCode::FORBIDDEN),
            (
                RoutingError::UnavailableOwner,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                RoutingError::InvalidSelection(anyhow::anyhow!("conflicting source IDs")),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            assert_eq!(status(&error.into()), expected);
        }
    }

    #[test]
    fn detail_policy_does_not_confuse_upstream_forbidden_with_local_denial() {
        let upstream = ApplicationError::UpstreamStatus(StatusCode::FORBIDDEN);
        let local = ApplicationError::AccessDenied;
        assert_eq!(status(&upstream), status(&local));
        assert!(upstream.allows_detail_fallback());
        assert!(!local.allows_detail_fallback());
        let error =
            ApplicationError::Storage(sqlx::Error::RowNotFound).at(FailureStage::HlsTranslation);
        assert_eq!(status(&error), StatusCode::BAD_GATEWAY);
    }

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
            assert_eq!(status(&error), expected);
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
        assert_eq!(status(&upstream), StatusCode::SERVICE_UNAVAILABLE);
        assert!(upstream.allows_detail_fallback());
        assert!(!ApplicationError::AccessDenied
            .at(FailureStage::PlaybackCommit)
            .allows_detail_fallback());
    }
}
