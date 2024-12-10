use std::{future::poll_fn, net::SocketAddr, pin::pin, str::FromStr, sync::Arc};

use axum::{routing::get, Router};
use bytes::Bytes;
use h3::error::ErrorLevel;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::Mutex;
use tower::{Service, ServiceExt};
use tracing::{error, info, trace_span};

static ALPN: &[u8] = b"h3";

#[tokio::main]
async fn main() -> Result<(), Box<dyn core::error::Error>> {
    tracing_subscriber::fmt().init();

    let router = Arc::new(Mutex::new(
        Router::new().route("/", get(|| async { "Hello, World!" })),
    ));

    // both cert and key must be DER-encoded
    let cert = CertificateDer::from(include_bytes!("../server.cert").to_vec());
    let key = PrivateKeyDer::try_from(include_bytes!("../server.key").to_vec())?;

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;

    tls_config.max_early_data_size = u32::MAX;
    tls_config.alpn_protocols = vec![ALPN.into()];

    let server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls_config)?));
    let endpoint =
        quinn::Endpoint::server(server_config, SocketAddr::from_str("[::1]:4433").unwrap())?;

    info!("listening on {}", "[::1]:4433");

    // handle incoming connections and requests

    while let Some(new_conn) = endpoint.accept().await {
        trace_span!("New connection being attempted");

        let router = Arc::clone(&router);

        tokio::spawn(async move {
            match new_conn.await {
                Ok(conn) => {
                    info!("new connection established");

                    let mut h3_conn: h3::server::Connection<h3_quinn::Connection, Bytes> =
                        h3::server::Connection::new(h3_quinn::Connection::new(conn))
                            .await
                            .unwrap();

                    loop {
                        match h3_conn.accept().await {
                            Ok(Some((req, mut stream))) => {
                                info!("new request: {:#?}", req);

                                let req = http::Request::from_parts(
                                    req.into_parts().0,
                                    axum::body::Body::empty(),
                                );

                                let router = Arc::clone(&router);

                                tokio::spawn(async move {
                                    let (parts, body) = router
                                        .lock()
                                        .await
                                        .as_service()
                                        .ready()
                                        .await
                                        .unwrap()
                                        .call(req)
                                        .await
                                        .unwrap()
                                        .into_parts();

                                    stream
                                        .send_response(http::Response::from_parts(parts, ()))
                                        .await
                                        .unwrap();

                                    let mut body = pin!(body);

                                    while let Some(chunk) = {
                                        poll_fn(|ctx| {
                                            <axum::body::Body as http_body::Body>::poll_frame(
                                                body.as_mut(),
                                                ctx,
                                            )
                                        })
                                        .await
                                    } {
                                        stream
                                            .send_data(chunk.unwrap().into_data().unwrap())
                                            .await
                                            .unwrap();
                                    }

                                    stream.finish().await.unwrap();
                                });
                            }

                            // indicating no more streams to be received
                            Ok(None) => {
                                break;
                            }

                            Err(err) => {
                                error!("error on accept {}", err);
                                match err.get_error_level() {
                                    ErrorLevel::ConnectionError => break,
                                    ErrorLevel::StreamError => continue,
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    error!("accepting connection failed: {:?}", err);
                }
            }
        });
    }

    // shut down gracefully
    // wait for connections to be closed before exiting
    endpoint.wait_idle().await;

    Ok(())
}
