use crate::config::LetsEncryptConfig;
use futures::StreamExt;
use rustls_acme::axum::AxumAcceptor;
use rustls_acme::caches::DirCache;
use rustls_acme::tower::TowerHttp01ChallengeService;
use rustls_acme::{AcmeConfig, UseChallenge};
use tracing::{error, info};

/// Axum route pattern for [`Acme::challenge_service`]. The path is fixed by
/// RFC 8555 -- Let's Encrypt fetches exactly this and nothing else, so it is not
/// something the config gets to move.
pub const HTTP01_CHALLENGE_ROUTE: &str = "/.well-known/acme-challenge/{challenge_token}";

pub struct Acme {
    pub acceptor: AxumAcceptor,
    /// Serves the token Let's Encrypt fetches over plain HTTP to validate the
    /// order. Mount it at [`HTTP01_CHALLENGE_ROUTE`].
    pub challenge_service: TowerHttp01ChallengeService,
}

/// Drives the ACME order/renewal state machine on a background task.
///
/// The returned acceptor serves whichever certificate that task has most recently
/// obtained, so it can be handed to a server before the first order completes.
/// Handshakes attempted in the meantime fail; they succeed once a certificate lands.
pub fn spawn(config: &LetsEncryptConfig) -> Acme {
    let mut state = AcmeConfig::new(config.domains.clone())
        .cache(DirCache::new(config.certs_dir.clone()))
        .directory_lets_encrypt(config.prod_letsencrypt)
        .challenge_type(UseChallenge::Http01)
        .state();

    let acme = Acme {
        acceptor: state.axum_acceptor(state.default_rustls_config()),
        challenge_service: state.http01_challenge_tower_service(),
    };

    tokio::spawn(async move {
        while let Some(event) = state.next().await {
            match event {
                Ok(event) => info!(?event, "acme"),
                Err(err) => error!(?err, "acme order failed"),
            }
        }
        error!("acme state machine ended, certificates will no longer renew");
    });

    acme
}
