use std::{collections::HashMap, sync::Arc};

use anyhow::Context as _;
use assert_matches::assert_matches;
use mas_oidc_client::{
    requests::{
        account_management::AccountManagementActionFull,
        authorization_code::AuthorizationValidationData,
    },
    types::{
        errors::ClientErrorCode,
        iana::oauth::OAuthClientAuthenticationMethod,
        registration::{ClientMetadata, VerifiedClientMetadata},
        requests::Prompt,
    },
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::{async_test, test_json};
use ruma::ServerName;
use serde_json::{json, Value as JsonValue};
use stream_assert::{assert_next_matches, assert_pending};
use tempfile::tempdir;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{
    backend::mock::{MockImpl, AUTHORIZATION_URL, CLIENT_ID, ISSUER_URL},
    registrations::{ClientId, OidcRegistrations},
    AuthorizationCode, AuthorizationError, AuthorizationResponse, Oidc, OidcError, OidcSession,
    OidcSessionTokens, RedirectUriQueryParseError, UserSession,
};
use crate::{
    test_utils::{
        client::MockClientBuilder, no_retry_test_client_with_server, test_client_builder,
    },
    Client, Error,
};

const REDIRECT_URI_STRING: &str = "http://matrix.example.com/oidc/callback";

pub fn mock_client_metadata() -> VerifiedClientMetadata {
    ClientMetadata {
        redirect_uris: Some(vec![Url::parse(REDIRECT_URI_STRING).unwrap()]),
        token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
        ..ClientMetadata::default()
    }
    .validate()
    .expect("validate client metadata")
}

pub fn mock_registered_client_data() -> (ClientId, VerifiedClientMetadata) {
    (ClientId(CLIENT_ID.to_owned()), mock_client_metadata())
}

pub fn mock_session(tokens: OidcSessionTokens) -> OidcSession {
    let (client_id, metadata) = mock_registered_client_data();
    OidcSession {
        client_id,
        metadata,
        user: UserSession {
            meta: SessionMeta {
                user_id: ruma::user_id!("@u:e.uk").to_owned(),
                device_id: ruma::device_id!("XYZ").to_owned(),
            },
            tokens,
            issuer: ISSUER_URL.to_owned(),
        },
    }
}

pub async fn mock_environment(
) -> anyhow::Result<(Oidc, MockServer, VerifiedClientMetadata, OidcRegistrations)> {
    let server = MockServer::start().await;
    let issuer = ISSUER_URL.to_owned();
    let issuer_url = Url::parse(&issuer).unwrap();

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/account/whoami"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user_id": "@joe:example.org",
            "device_id": "D3V1C31D"
        })))
        .mount(&server)
        .await;

    let client = test_client_builder(Some(server.uri())).build().await?;

    let session_tokens = OidcSessionTokens {
        access_token: "4cc3ss".to_owned(),
        refresh_token: Some("r3fr3$h".to_owned()),
        latest_id_token: None,
    };

    let oidc = Oidc {
        client,
        backend: Arc::new(MockImpl::new().mark_insecure().next_session_tokens(session_tokens)),
    };

    let (client_id, client_metadata) = mock_registered_client_data();

    // The mock backend doesn't support registration so set a static registration.
    let mut static_registrations = HashMap::new();
    static_registrations.insert(issuer_url, client_id);

    let registrations_path = tempdir().unwrap().path().join("oidc").join("registrations.json");
    let registrations =
        OidcRegistrations::new(&registrations_path, client_metadata.clone(), static_registrations)
            .unwrap();

    Ok((oidc, server, client_metadata, registrations))
}

#[async_test]
async fn test_high_level_login() -> anyhow::Result<()> {
    // Given a fresh environment.
    let (oidc, _server, metadata, registrations) = mock_environment().await.unwrap();
    assert!(oidc.issuer().is_none());
    assert!(oidc.client_metadata().is_none());
    assert!(oidc.client_id().is_none());

    // When getting the OIDC login URL.
    let authorization_data =
        oidc.url_for_oidc(metadata.clone(), registrations, Prompt::Login).await.unwrap();

    // Then the client should be configured correctly.
    assert!(oidc.issuer().is_some());
    assert!(oidc.client_metadata().is_some());
    assert!(oidc.client_id().is_some());

    // When completing the login with a valid callback.
    let mut callback_uri = metadata.redirect_uris.clone().unwrap().first().unwrap().clone();
    callback_uri.set_query(Some(&format!("code=42&state={}", authorization_data.state)));

    // Then the login should succeed.
    oidc.login_with_oidc_callback(&authorization_data, callback_uri).await?;

    Ok(())
}

#[async_test]
async fn test_high_level_login_cancellation() -> anyhow::Result<()> {
    // Given a client ready to complete login.
    let (oidc, _server, metadata, registrations) = mock_environment().await.unwrap();
    let authorization_data =
        oidc.url_for_oidc(metadata.clone(), registrations, Prompt::Login).await.unwrap();

    assert!(oidc.issuer().is_some());
    assert!(oidc.client_metadata().is_some());
    assert!(oidc.client_id().is_some());

    // When completing login with a cancellation callback.
    let mut callback_uri = metadata.redirect_uris.clone().unwrap().first().unwrap().clone();
    callback_uri
        .set_query(Some(&format!("error=access_denied&state={}", authorization_data.state)));

    let error = oidc.login_with_oidc_callback(&authorization_data, callback_uri).await.unwrap_err();

    // Then a cancellation error should be thrown.
    assert_matches!(error, Error::Oidc(OidcError::CancelledAuthorization));

    Ok(())
}

#[async_test]
async fn test_high_level_login_invalid_state() -> anyhow::Result<()> {
    // Given a client ready to complete login.
    let (oidc, _server, metadata, registrations) = mock_environment().await.unwrap();
    let authorization_data =
        oidc.url_for_oidc(metadata.clone(), registrations, Prompt::Login).await.unwrap();

    assert!(oidc.issuer().is_some());
    assert!(oidc.client_metadata().is_some());
    assert!(oidc.client_id().is_some());

    // When completing login with an old/tampered state.
    let mut callback_uri = metadata.redirect_uris.clone().unwrap().first().unwrap().clone();
    callback_uri.set_query(Some("code=42&state=imposter_alert"));

    let error = oidc.login_with_oidc_callback(&authorization_data, callback_uri).await.unwrap_err();

    // Then the login should fail by flagging the invalid state.
    assert_matches!(error, Error::Oidc(OidcError::InvalidState));

    Ok(())
}

#[async_test]
async fn test_login() -> anyhow::Result<()> {
    let client = test_client_builder(Some("https://example.org".to_owned())).build().await?;

    let device_id = "D3V1C31D".to_owned(); // yo this is 1999 speaking

    let oidc = Oidc { client: client.clone(), backend: Arc::new(MockImpl::new()) };

    let (client_credentials, client_metadata) = mock_registered_client_data();
    oidc.restore_registered_client(ISSUER_URL.to_owned(), client_metadata, client_credentials);

    let redirect_uri_str = REDIRECT_URI_STRING;
    let redirect_uri = Url::parse(redirect_uri_str)?;
    let mut authorization_data = oidc.login(redirect_uri, Some(device_id.clone()))?.build().await?;

    tracing::debug!("authorization data URL = {}", authorization_data.url);

    let mut num_expected = 6;
    let mut nonce = None;

    for (key, val) in authorization_data.url.query_pairs() {
        match &*key {
            "response_type" => {
                assert_eq!(val, "code");
                num_expected -= 1;
            }
            "client_id" => {
                assert_eq!(val, CLIENT_ID);
                num_expected -= 1;
            }
            "redirect_uri" => {
                assert_eq!(val, redirect_uri_str);
                num_expected -= 1;
            }
            "scope" => {
                assert_eq!(val, format!("openid urn:matrix:org.matrix.msc2967.client:api:* urn:matrix:org.matrix.msc2967.client:device:{device_id}"));
                num_expected -= 1;
            }
            "state" => {
                num_expected -= 1;
                assert_eq!(val, authorization_data.state);
            }
            "nonce" => {
                num_expected -= 1;
                nonce = Some(val);
            }
            _ => panic!("unexpected query parameter: {key}={val}"),
        }
    }

    assert_eq!(num_expected, 0);

    let data = oidc.data().unwrap();
    let authorization_data_guard = data.authorization_data.lock().await;

    let state = authorization_data_guard.get(&authorization_data.state).context("missing state")?;
    let nonce = nonce.context("missing nonce")?;
    assert_eq!(nonce, state.nonce);

    authorization_data.url.set_query(None);
    assert_eq!(authorization_data.url, Url::parse(AUTHORIZATION_URL).unwrap(),);

    Ok(())
}

#[test]
fn test_authorization_response() -> anyhow::Result<()> {
    let uri = Url::parse("https://example.com")?;
    assert_matches!(
        AuthorizationResponse::parse_uri(&uri),
        Err(RedirectUriQueryParseError::MissingQuery)
    );

    let uri = Url::parse("https://example.com?code=123&state=456")?;
    assert_matches!(
        AuthorizationResponse::parse_uri(&uri),
        Ok(AuthorizationResponse::Success(AuthorizationCode { code, state })) => {
            assert_eq!(code, "123");
            assert_eq!(state, "456");
        }
    );

    let uri = Url::parse("https://example.com?error=invalid_grant&state=456")?;
    assert_matches!(
        AuthorizationResponse::parse_uri(&uri),
        Ok(AuthorizationResponse::Error(AuthorizationError { error, state })) => {
            assert_eq!(error.error, ClientErrorCode::InvalidGrant);
            assert_eq!(error.error_description, None);
            assert_eq!(state, "456");
        }
    );

    Ok(())
}

#[async_test]
async fn test_finish_authorization() -> anyhow::Result<()> {
    let client = test_client_builder(Some("https://example.org".to_owned())).build().await?;

    let session_tokens = OidcSessionTokens {
        access_token: "4cc3ss".to_owned(),
        refresh_token: Some("r3fr3$h".to_owned()),
        latest_id_token: None,
    };
    let oidc = Oidc {
        client: client.clone(),
        backend: Arc::new(MockImpl::new().next_session_tokens(session_tokens.clone())),
    };

    let (client_credentials, client_metadata) = mock_registered_client_data();
    oidc.restore_registered_client(ISSUER_URL.to_owned(), client_metadata, client_credentials);

    // If the state is missing, then any attempt to finish authorizing will fail.
    let res = oidc
        .finish_authorization(AuthorizationCode { code: "42".to_owned(), state: "none".to_owned() })
        .await;

    assert_matches!(res, Err(OidcError::InvalidState));
    assert!(oidc.session_tokens().is_none());

    // Assuming a non-empty state "123"...
    let state = "state".to_owned();
    let redirect_uri = REDIRECT_URI_STRING;
    let auth_validation_data = AuthorizationValidationData {
        state: state.clone(),
        nonce: "nonce".to_owned(),
        redirect_uri: Url::parse(redirect_uri)?,
        code_challenge_verifier: None,
    };

    {
        let data = oidc.data().context("missing data")?;
        let prev = data.authorization_data.lock().await.insert(state.clone(), {
            AuthorizationValidationData { ..auth_validation_data.clone() }
        });
        assert!(prev.is_none());
    }

    // Finishing the authorization for another state won't work.
    let res = oidc
        .finish_authorization(AuthorizationCode {
            code: "1337".to_owned(),
            state: "none".to_owned(),
        })
        .await;

    assert_matches!(res, Err(OidcError::InvalidState));
    assert!(oidc.session_tokens().is_none());
    assert!(oidc.data().unwrap().authorization_data.lock().await.get(&state).is_some());

    // Finishing the authorization for the expected state will work.
    oidc.finish_authorization(AuthorizationCode { code: "1337".to_owned(), state: state.clone() })
        .await?;

    assert_eq!(oidc.session_tokens(), Some(session_tokens));
    assert!(oidc.data().unwrap().authorization_data.lock().await.get(&state).is_none());

    Ok(())
}

#[async_test]
async fn test_oidc_session() -> anyhow::Result<()> {
    let client = test_client_builder(Some("https://example.org".to_owned())).build().await?;

    let backend = Arc::new(MockImpl::new());
    let oidc = Oidc { client: client.clone(), backend: backend.clone() };

    let tokens = OidcSessionTokens {
        access_token: "4cc3ss".to_owned(),
        refresh_token: Some("r3fr3sh".to_owned()),
        latest_id_token: None,
    };

    let session = mock_session(tokens.clone());
    oidc.restore_session(session.clone()).await?;

    // Test a few extra getters.
    assert_eq!(*oidc.client_metadata().unwrap(), session.metadata);
    assert_eq!(oidc.access_token().unwrap(), tokens.access_token);
    assert_eq!(oidc.refresh_token(), tokens.refresh_token);

    let user_session = oidc.user_session().unwrap();
    assert_eq!(user_session.meta, session.user.meta);
    assert_eq!(user_session.tokens, tokens);
    assert_eq!(user_session.issuer, ISSUER_URL);

    let full_session = oidc.full_session().unwrap();

    assert_eq!(full_session.client_id.0, CLIENT_ID);
    assert_eq!(full_session.metadata, session.metadata);
    assert_eq!(full_session.user.meta, session.user.meta);
    assert_eq!(full_session.user.tokens, tokens);
    assert_eq!(full_session.user.issuer, ISSUER_URL);

    Ok(())
}

#[async_test]
async fn test_insecure_clients() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let server_url = server.uri();

    Mock::given(method("GET"))
        .and(path("/.well-known/matrix/client"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            test_json::WELL_KNOWN.to_string().replace("HOMESERVER_URL", server_url.as_ref()),
            "application/json",
        ))
        .mount(&server)
        .await;

    let prev_tokens = OidcSessionTokens {
        access_token: "prev-access-token".to_owned(),
        refresh_token: Some("prev-refresh-token".to_owned()),
        latest_id_token: None,
    };

    let next_tokens = OidcSessionTokens {
        access_token: "next-access-token".to_owned(),
        refresh_token: Some("next-refresh-token".to_owned()),
        latest_id_token: None,
    };

    for client in [
        // Create an insecure client with the homeserver_url method.
        Client::builder().homeserver_url("http://example.org").build().await?,
        // Create an insecure client with the insecure_server_name_no_tls method.
        Client::builder()
            .insecure_server_name_no_tls(&ServerName::parse(
                server_url.strip_prefix("http://").unwrap(),
            )?)
            .build()
            .await?,
    ] {
        let backend = Arc::new(
            MockImpl::new()
                .mark_insecure()
                .next_session_tokens(next_tokens.clone())
                .expected_refresh_token(prev_tokens.refresh_token.as_ref().unwrap().clone()),
        );
        let oidc = Oidc { client: client.clone(), backend: backend.clone() };

        // Restore the previous session so we have an existing set of refresh tokens.
        oidc.restore_session(mock_session(prev_tokens.clone())).await?;

        let mut session_token_stream = oidc.session_tokens_stream().expect("stream available");

        assert_pending!(session_token_stream);

        // A refresh in insecure mode should work Just Fine.
        oidc.refresh_access_token().await?;

        assert_next_matches!(session_token_stream, new_tokens => {
            assert_eq!(new_tokens, next_tokens);
        });

        assert_pending!(session_token_stream);

        // There should have been exactly one refresh.
        assert_eq!(*backend.num_refreshes.lock().unwrap(), 1);
    }

    Ok(())
}

#[async_test]
async fn test_register_client() {
    let client = test_client_builder(Some("https://example.org".to_owned())).build().await.unwrap();
    let client_metadata = mock_client_metadata();

    // Server doesn't support registration, it fails.
    let backend = Arc::new(MockImpl::new().registration_endpoint(None));
    let oidc = Oidc { client: client.clone(), backend };

    let result = oidc.register_client(client_metadata.clone(), None).await;
    assert_matches!(result, Err(OidcError::NoRegistrationSupport));

    // Server supports registration, it succeeds.
    let backend = Arc::new(MockImpl::new());
    let oidc = Oidc { client: client.clone(), backend };

    let response = oidc.register_client(client_metadata.clone(), None).await.unwrap();
    assert_eq!(response.client_id, CLIENT_ID);

    let auth_data = oidc.data().unwrap();
    assert_eq!(auth_data.issuer, ISSUER_URL);
    assert_eq!(auth_data.client_id.0, response.client_id);
    assert_eq!(auth_data.metadata, client_metadata);
}

#[async_test]
async fn test_management_url_cache() {
    let client = MockClientBuilder::new("http://localhost".to_owned()).unlogged().build().await;
    let backend = Arc::new(
        MockImpl::new().mark_insecure().account_management_uri("http://localhost".to_owned()),
    );
    let oidc = Oidc { client: client.clone(), backend: backend.clone() };

    let tokens = OidcSessionTokens {
        access_token: "4cc3ss".to_owned(),
        refresh_token: Some("r3fr3sh".to_owned()),
        latest_id_token: None,
    };

    let session = mock_session(tokens.clone());
    oidc.restore_session(session.clone())
        .await
        .expect("We should be able to restore an OIDC session");

    // The cache should not contain the entry.
    assert!(!client.inner.caches.provider_metadata.lock().await.contains("PROVIDER_METADATA"));

    let management_url = oidc
        .account_management_url(Some(AccountManagementActionFull::Profile))
        .await
        .expect("We should be able to fetch the account management url");

    assert!(management_url.is_some());

    // Check that the provider metadata has been inserted into the cache.
    assert!(client.inner.caches.provider_metadata.lock().await.contains("PROVIDER_METADATA"));
}

fn mock_oidc_provider_metadata(issuer: &str) -> JsonValue {
    json!({
        "issuer": issuer,
        "authorization_endpoint": issuer,
        "token_endpoint": issuer,
        "jwks_uri": issuer,
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["rs256"],
    })
}

#[async_test]
async fn test_provider_metadata() {
    let (client, server) = no_retry_test_client_with_server().await;
    let oidc = client.oidc();
    let issuer = server.uri();

    // The endpoint is not mocked so it is not supported.
    let error = oidc.provider_metadata().await.unwrap_err();
    assert!(error.is_not_supported());

    // Mock the `GET /auth_issuer` fallback endpoint.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc2965/auth_issuer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issuer": issuer})))
        .expect(1)
        .named("auth_issuer")
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_oidc_provider_metadata(&issuer)),
        )
        .expect(1)
        .named("openid-configuration")
        .mount(&server)
        .await;
    oidc.provider_metadata().await.unwrap();

    // Mock the `GET /auth_metadata` endpoint.
    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc2965/auth_metadata"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_oidc_provider_metadata(&issuer)),
        )
        .expect(1)
        .named("auth_metadata")
        .mount(&server)
        .await;
    oidc.provider_metadata().await.unwrap();
}
