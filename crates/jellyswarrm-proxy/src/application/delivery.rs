//! Add workflow stage context to transport-owned playlist/delivery failures.
use super::error::{ApplicationError, FailureStage};
use crate::upstream::jellyfin::{delivery::DeliveryError, hls::HlsError};

fn hls_error(error: HlsError) -> ApplicationError {
    match error {
        HlsError::Status(status) => {
            ApplicationError::UpstreamStatus(status).at(FailureStage::HlsStatus)
        }
        HlsError::InvalidPayload(reason) => ApplicationError::InvalidUpstreamPayload(reason),
        HlsError::Transport(error) => ApplicationError::UpstreamTransport(error),
        HlsError::Encoding(error) => {
            ApplicationError::Internal(error.into()).at(FailureStage::HlsTranslation)
        }
        HlsError::Translation(error) => {
            ApplicationError::Internal(error).at(FailureStage::HlsTranslation)
        }
        HlsError::Deadline(error) => {
            ApplicationError::Internal(error.into()).at(FailureStage::HlsDeadline)
        }
    }
}

pub(crate) fn delivery_error(error: DeliveryError) -> ApplicationError {
    match error {
        DeliveryError::Transport(error) => ApplicationError::UpstreamTransport(error),
        DeliveryError::Header(error) => ApplicationError::Internal(error.into()),
        DeliveryError::Playlist(error) => hls_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[tokio::test(start_paused = true)]
    async fn playlist_conversion_retains_stage_and_original_causes() {
        let encoding = String::from_utf8(vec![0xff]).unwrap_err();
        let elapsed = tokio::time::timeout(std::time::Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        for (error, deadline) in [
            (HlsError::Encoding(encoding), false),
            (HlsError::Deadline(elapsed), true),
        ] {
            let error = delivery_error(DeliveryError::Playlist(error));
            assert!(
                matches!(
                    &error,
                    ApplicationError::At {
                        stage: FailureStage::HlsDeadline,
                        ..
                    }
                ) == deadline
            );
            assert!(
                matches!(
                    &error,
                    ApplicationError::At {
                        stage: FailureStage::HlsTranslation,
                        ..
                    }
                ) != deadline
            );
            let cause = error.source().unwrap().source().unwrap();
            assert_eq!(cause.is::<tokio::time::error::Elapsed>(), deadline);
            assert_eq!(cause.is::<std::string::FromUtf8Error>(), !deadline);
        }
        let error = delivery_error(DeliveryError::Playlist(HlsError::Translation(
            anyhow::Error::new(sqlx::Error::PoolClosed),
        )));
        assert!(matches!(
            &error,
            ApplicationError::At {
                stage: FailureStage::HlsTranslation,
                ..
            }
        ));
        assert!(error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .is::<sqlx::Error>());
    }
}
