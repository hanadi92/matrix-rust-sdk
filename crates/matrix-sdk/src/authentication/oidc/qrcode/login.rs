// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::future::IntoFuture;

use eyeball::SharedObservable;
use futures_core::Stream;
use mas_oidc_client::types::{
    registration::VerifiedClientMetadata,
    scope::{MatrixApiScopeToken, ScopeToken},
};
use matrix_sdk_base::{
    boxed_into_future,
    crypto::types::qr_login::{QrCodeData, QrCodeMode},
    SessionMeta,
};
use oauth2::{DeviceCodeErrorResponseType, Scope, StandardDeviceAuthorizationResponse};
use ruma::OwnedDeviceId;
use tracing::trace;
use vodozemac::{ecies::CheckCode, Curve25519PublicKey};

use super::{
    messages::{LoginFailureReason, QrAuthMessage},
    secure_channel::EstablishedSecureChannel,
    DeviceAuthorizationOauthError, QRCodeLoginError, SecureChannelError,
};
#[cfg(doc)]
use crate::authentication::oidc::Oidc;
use crate::{
    authentication::oidc::{OidcError, OidcSessionTokens},
    Client,
};

async fn send_unexpected_message_error(
    channel: &mut EstablishedSecureChannel,
) -> Result<(), SecureChannelError> {
    channel
        .send_json(QrAuthMessage::LoginFailure {
            reason: LoginFailureReason::UnexpectedMessageReceived,
            homeserver: None,
        })
        .await
}

/// Type telling us about the progress of the QR code login.
#[derive(Clone, Debug, Default)]
pub enum LoginProgress {
    /// We're just starting up, this is the default and initial state.
    #[default]
    Starting,
    /// We have established the secure channel, but we need to let the other
    /// side know about the [`CheckCode`] so they can verify that the secure
    /// channel is indeed secure.
    EstablishingSecureChannel {
        /// The check code we need to, out of band, send to the other device.
        check_code: CheckCode,
    },
    /// We're waiting for the OAuth 2.0 authorization server to give us the
    /// access token. This will only happen if the other device allows the
    /// OAuth 2.0 authorization server to do so.
    WaitingForToken {
        /// The user code the OAuth 2.0 authorization server has given us, the
        /// OAuth 2.0 authorization server might ask the other device to
        /// enter this code.
        user_code: String,
    },
    /// The login process has completed.
    Done,
}

/// Named future for the [`Oidc::login_with_qr_code()`] method.
#[derive(Debug)]
pub struct LoginWithQrCode<'a> {
    client: &'a Client,
    client_metadata: VerifiedClientMetadata,
    qr_code_data: &'a QrCodeData,
    state: SharedObservable<LoginProgress>,
}

impl LoginWithQrCode<'_> {
    /// Subscribe to the progress of QR code login.
    ///
    /// It's usually necessary to subscribe to this to let the existing device
    /// know about the [`CheckCode`] which is used to verify that the two
    /// devices are communicating in a secure manner.
    pub fn subscribe_to_progress(&self) -> impl Stream<Item = LoginProgress> {
        self.state.subscribe()
    }
}

impl<'a> IntoFuture for LoginWithQrCode<'a> {
    type Output = Result<(), QRCodeLoginError>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // First things first, establish the secure channel. Since we're the one that
            // scanned the QR code, we're certain that the secure channel is
            // secure, under the assumption that we didn't scan the wrong QR code.
            let mut channel = self.establish_secure_channel().await?;

            trace!("Established the secure channel.");

            // The other side isn't yet sure that it's talking to the right device, show
            // a check code so they can confirm.
            let check_code = channel.check_code().to_owned();
            self.state.set(LoginProgress::EstablishingSecureChannel { check_code });

            // Register the client with the OAuth 2.0 authorization server.
            trace!("Registering the client with the OAuth 2.0 authorization server.");
            self.register_client().await?;

            // We want to use the Curve25519 public key for the device ID, so let's generate
            // a new vodozemac `Account` now.
            let account = vodozemac::olm::Account::new();
            let public_key = account.identity_keys().curve25519;
            let device_id = public_key;

            // Let's tell the OAuth 2.0 authorization server that we want to log in using
            // the device authorization grant described in [RFC8628](https://datatracker.ietf.org/doc/html/rfc8628).
            trace!("Requesting device authorization.");
            let auth_grant_response = self.request_device_authorization(device_id).await?;

            // Now we need to inform the other device of the login protocols we picked and
            // the URL they should use to log us in.
            trace!("Letting the existing device know about the device authorization grant.");
            let message = QrAuthMessage::authorization_grant_login_protocol(
                (&auth_grant_response).into(),
                device_id,
            );
            channel.send_json(&message).await?;

            // Let's see if the other device agreed to our proposed protocols.
            match channel.receive_json().await? {
                QrAuthMessage::LoginProtocolAccepted => (),
                QrAuthMessage::LoginFailure { reason, homeserver } => {
                    return Err(QRCodeLoginError::LoginFailure { reason, homeserver });
                }
                message => {
                    send_unexpected_message_error(&mut channel).await?;

                    return Err(QRCodeLoginError::UnexpectedMessage {
                        expected: "m.login.protocol_accepted",
                        received: message,
                    });
                }
            }

            // The OAuth 2.0 authorization server may or may not show this user code to
            // double check that we're talking to the right server. Let us display this, so
            // the other device can double check this as well.
            let user_code = auth_grant_response.user_code();
            self.state
                .set(LoginProgress::WaitingForToken { user_code: user_code.secret().to_owned() });

            // Let's now wait for the access token to be provided to use by the OAuth 2.0
            // authorization server.
            trace!("Waiting for the OAuth 2.0 authorization server to give us the access token.");
            let session_tokens = match self.wait_for_tokens(&auth_grant_response).await {
                Ok(t) => t,
                Err(e) => {
                    // If we received an error, and it's one of the ones we should report to the
                    // other side, do so now.
                    if let Some(e) = e.as_request_token_error() {
                        match e {
                            DeviceCodeErrorResponseType::AccessDenied => {
                                channel.send_json(QrAuthMessage::LoginDeclined).await?;
                            }
                            DeviceCodeErrorResponseType::ExpiredToken => {
                                channel
                                    .send_json(QrAuthMessage::LoginFailure {
                                        reason: LoginFailureReason::AuthorizationExpired,
                                        homeserver: None,
                                    })
                                    .await?;
                            }
                            _ => (),
                        }
                    }

                    return Err(e.into());
                }
            };
            self.client.oidc().set_session_tokens(session_tokens);

            // We only received an access token from the OAuth 2.0 authorization server, we
            // have no clue who we are, so we need to figure out our user ID
            // now. TODO: This snippet is almost the same as the
            // Oidc::finish_login_method(), why is that method even a public
            // method and not called as part of the set session tokens method.
            trace!("Discovering our own user id.");
            let whoami_response =
                self.client.whoami().await.map_err(QRCodeLoginError::UserIdDiscovery)?;
            self.client
                .set_session_meta(
                    SessionMeta {
                        user_id: whoami_response.user_id,
                        device_id: OwnedDeviceId::from(device_id.to_base64()),
                    },
                    Some(account),
                )
                .await
                .map_err(QRCodeLoginError::SessionTokens)?;

            self.client.oidc().enable_cross_process_lock().await?;

            // Tell the existing device that we're logged in.
            trace!("Telling the existing device that we successfully logged in.");
            let message = QrAuthMessage::LoginSuccess;
            channel.send_json(&message).await?;

            // Let's wait for the secrets bundle to be sent to us, otherwise we won't be a
            // fully E2EE enabled device.
            trace!("Waiting for the secrets bundle.");
            let bundle = match channel.receive_json().await? {
                QrAuthMessage::LoginSecrets(bundle) => bundle,
                QrAuthMessage::LoginFailure { reason, homeserver } => {
                    return Err(QRCodeLoginError::LoginFailure { reason, homeserver });
                }
                message => {
                    send_unexpected_message_error(&mut channel).await?;

                    return Err(QRCodeLoginError::UnexpectedMessage {
                        expected: "m.login.secrets",
                        received: message,
                    });
                }
            };

            // Import the secrets bundle, this will allow us to sign the device keys with
            // the master key when we upload them.
            self.client.encryption().import_secrets_bundle(&bundle).await?;

            // Upload the device keys, this will ensure that other devices see us as a fully
            // verified device ass soon as this method returns.
            self.client
                .encryption()
                .ensure_device_keys_upload()
                .await
                .map_err(QRCodeLoginError::DeviceKeyUpload)?;

            // Run and wait for the E2EE initialization tasks, this will ensure that we
            // ourselves see us as verified and the recovery/backup states will
            // be known. If we did receive all the secrets in the secrets
            // bundle, then backups will be enabled after this step as well.
            self.client.encryption().spawn_initialization_task(None);
            self.client.encryption().wait_for_e2ee_initialization_tasks().await;

            trace!("successfully logged in and enabled E2EE.");

            // Tell our listener that we're done.
            self.state.set(LoginProgress::Done);

            // And indeed, we are done with the login.
            Ok(())
        })
    }
}

impl<'a> LoginWithQrCode<'a> {
    pub(crate) fn new(
        client: &'a Client,
        client_metadata: VerifiedClientMetadata,
        qr_code_data: &'a QrCodeData,
    ) -> LoginWithQrCode<'a> {
        LoginWithQrCode { client, client_metadata, qr_code_data, state: Default::default() }
    }

    async fn establish_secure_channel(
        &self,
    ) -> Result<EstablishedSecureChannel, SecureChannelError> {
        let http_client = self.client.inner.http_client.inner.clone();

        let channel = EstablishedSecureChannel::from_qr_code(
            http_client,
            self.qr_code_data,
            QrCodeMode::Login,
        )
        .await?;

        Ok(channel)
    }

    /// Register the client with the OAuth 2.0 authorization server.
    async fn register_client(&self) -> Result<(), DeviceAuthorizationOauthError> {
        let oidc = self.client.oidc();
        oidc.register_client(self.client_metadata.clone(), None).await?;
        Ok(())
    }

    async fn request_device_authorization(
        &self,
        device_id: Curve25519PublicKey,
    ) -> Result<StandardDeviceAuthorizationResponse, DeviceAuthorizationOauthError> {
        let scopes = [
            ScopeToken::MatrixApi(MatrixApiScopeToken::Full),
            ScopeToken::try_with_matrix_device(device_id.to_base64()).expect(
                "We should be able to create a scope token from a \
                 Curve25519 public key encoded as base64",
            ),
        ]
        .into_iter()
        .map(|scope| Scope::new(scope.to_string()))
        .collect();

        let oidc = self.client.oidc();
        let client_id =
            oauth2::ClientId::new(oidc.client_id().ok_or(OidcError::NotRegistered)?.0.clone());
        let server_metadata = oidc.provider_metadata().await.map_err(OidcError::from)?;
        let device_authorization_endpoint =
            server_metadata
                .device_authorization_endpoint
                .clone()
                .ok_or(DeviceAuthorizationOauthError::NoDeviceAuthorizationEndpoint)?;

        let response = oidc
            .backend
            .request_device_authorization(device_authorization_endpoint, client_id, scopes)
            .await?;
        Ok(response)
    }

    async fn wait_for_tokens(
        &self,
        auth_response: &StandardDeviceAuthorizationResponse,
    ) -> Result<OidcSessionTokens, DeviceAuthorizationOauthError> {
        let oidc = self.client.oidc();
        let client_id =
            oauth2::ClientId::new(oidc.client_id().ok_or(OidcError::NotRegistered)?.0.clone());
        let server_metadata = oidc.provider_metadata().await.map_err(OidcError::from)?;
        let token_endpoint = server_metadata.token_endpoint().clone();

        let tokens =
            oidc.backend.exchange_device_code(token_endpoint, client_id, auth_response).await?;
        Ok(tokens)
    }
}

#[cfg(test)]
mod test {
    use assert_matches2::assert_let;
    use futures_util::{join, StreamExt};
    use mas_oidc_client::types::{
        iana::oauth::OAuthClientAuthenticationMethod,
        oidc::ApplicationType,
        registration::{ClientMetadata, Localized},
        requests::GrantType,
    };
    use matrix_sdk_base::crypto::types::{qr_login::QrCodeModeData, SecretsBundle};
    use matrix_sdk_test::{async_test, test_json};
    use serde_json::{json, Value};
    use url::Url;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::{
        authentication::oidc::qrcode::{
            messages::LoginProtocolType,
            secure_channel::{test::MockedRendezvousServer, SecureChannel},
        },
        config::RequestConfig,
        http_client::HttpClient,
    };

    enum AliceBehaviour {
        HappyPath,
        DeclinedProtocol,
        UnexpectedMessage,
        UnexpectedMessageInsteadOfSecrets,
        RefuseSecrets,
    }

    fn client_metadata() -> VerifiedClientMetadata {
        let client_uri = Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
            .expect("Couldn't parse client URI");

        ClientMetadata {
            application_type: Some(ApplicationType::Native),
            redirect_uris: None,
            grant_types: Some(vec![GrantType::DeviceCode]),
            token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
            client_name: Some(Localized::new("test-matrix-rust-sdk-qrlogin".to_owned(), [])),
            contacts: Some(vec!["root@127.0.0.1".to_owned()]),
            client_uri: Some(Localized::new(client_uri.clone(), [])),
            policy_uri: Some(Localized::new(client_uri.clone(), [])),
            tos_uri: Some(Localized::new(client_uri, [])),
            ..Default::default()
        }
        .validate()
        .unwrap()
    }

    fn open_id_configuration(server: &MockServer) -> Value {
        let issuer_url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let account_management_uri = issuer_url.join("account").unwrap();
        let authorization_endpoint = issuer_url.join("authorize").unwrap();
        let device_authorization_endpoint = issuer_url.join("oauth2/device").unwrap();
        let jwks_url = issuer_url.join("oauth2/keys.json").unwrap();
        let registration_endpoint = issuer_url.join("oauth2/registration").unwrap();
        let token_endpoint = issuer_url.join("oauth2/token").unwrap();

        json!({
            "account_management_actions_supported": [
                "org.matrix.profile",
                "org.matrix.sessions_list",
                "org.matrix.session_view",
                "org.matrix.session_end",
                "org.matrix.cross_signing_reset"
            ],
            "account_management_uri": account_management_uri,
            "authorization_endpoint": authorization_endpoint,
            "claim_types_supported": [
                "normal"
            ],
            "claims_parameter_supported": false,
            "claims_supported": [
                "iss",
                "sub",
                "aud",
                "iat",
                "exp",
                "nonce",
                "auth_time",
                "at_hash",
                "c_hash"
            ],
            "code_challenge_methods_supported": [
                "plain",
                "S256"
            ],
            "device_authorization_endpoint": device_authorization_endpoint,
            "display_values_supported": [
                "page"
            ],
            "grant_types_supported": [
                "authorization_code",
                "refresh_token",
                "client_credentials",
                "urn:ietf:params:oauth:grant-type:device_code"
            ],
            "id_token_signing_alg_values_supported": [
                "RS256",
                "RS384",
                "RS512",
                "ES256",
                "ES384",
                "PS256",
                "PS384",
                "PS512",
                "ES256K"
            ],
            "issuer": issuer_url.to_string().trim_end_matches("/"),
            "jwks_uri": jwks_url,
            "prompt_values_supported": [
                "none",
                "login",
                "create"
            ],
            "registration_endpoint": registration_endpoint,
            "request_parameter_supported": false,
            "request_uri_parameter_supported": false,
            "response_modes_supported": [
                "form_post",
                "query",
                "fragment"
            ],
            "response_types_supported": [
                "code",
                "id_token",
                "code id_token"
            ],
            "scopes_supported": [
                "openid",
                "email"
            ],
            "subject_types_supported": [
                "public"
            ],
            "token_endpoint": token_endpoint,
            "token_endpoint_auth_methods_supported": [
                "client_secret_basic",
                "client_secret_post",
                "client_secret_jwt",
                "private_key_jwt",
                "none"
            ],
        })
    }

    fn keys_json() -> Value {
        json!({
            "keys": [
                {
                    "e": "AQAB",
                    "kid": "hxdHWoF9mn",
                    "kty": "RSA",
                    "n": "u4op7tDV41j-f_-DqsqjjCObiySB0q2CGS1JVjJXbV5jctHP6Wp_oMb2aIImMdHDcnTvxaID\
                        WwuKA8o-0SBfkHFifMHHRvePz_l7NxxUMyGX8Bfu_EVkECe50BXpFydcEEl1eIIsPW-F0WJKFYR\
                        5cscmBgRX3zv_w7WFbaOLh711S9DNu21epdSvFSrKRe9oG_FbeOFfDl-YU7BLGFvEozg9Z3hKF\
                        SomOlz-t3ABvRUweGuLCpHFKsI6yhGCoqPyS7o5gpfenizdfHLqq-l7kgyr7lSbW_mTSyYutby\
                        DpQ_HM98Lt-4a9zwlGfiqPS3svkH6KSd1mBcayCI0Cm9FuQ",
                    "use": "sig"
                },
                {
                    "crv": "P-256",
                    "kid": "IRbxoGCBjs",
                    "kty": "EC",
                    "use": "sig",
                    "x": "1AYfsklcgvscvJiNZ1Og7vQePzIBf-flJKlANWJ7D4g",
                    "y": "L4b-jMZVZlnLhXCpV0EOc6zdEz1e6ONgKQZVE3jOBhY"
                },
                {
                    "crv": "P-384",
                    "kid": "FjEZp4JjqW",
                    "kty": "EC",
                    "use": "sig",
                    "x": "bZP2bPUEQGeGaDICINswZSTCHdoVmDD3LIJE1Szxw27ruCJBW-sy_lY3dhA2FjWm",
                    "y": "3HMgAu___-4JG9IXZFXwzr5nU_GUPvmWJHqgS7vzK1S91s0v1GXiqQMHwYA0keYG"
                },
                {
                    "crv": "secp256k1",
                    "kid": "7ohCuHzgqB",
                    "kty": "EC",
                    "use": "sig",
                    "x": "80KXhBY8JBy8qO9-wMBaGtgOgtagowHJ4dDGfVr4eVw",
                    "y": "0ALeT-J40AjdIS4S1YDgMrPkyE_rnw9wVm7Dvz_9Np4"
                }
            ]
        })
    }

    fn device_code(server: &MockServer) -> Value {
        let issuer_url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let verification_uri = issuer_url.join("link").unwrap();
        let mut verification_uri_complete = issuer_url.join("link").unwrap();
        verification_uri_complete.set_query(Some("code=N32YVC"));

        json!({
            "device_code": "N8NAYD9fOhMulpm37mSthx0xSw2p7vdR",
            "expires_in": 1200,
            "interval": 5,
            "user_code": "N32YVC",
            "verification_uri": verification_uri,
            "verification_uri_complete": verification_uri_complete,
        })
    }

    fn token() -> Value {
        json!({
            "access_token": "mat_z65RpDAbvR5aTr7MzD0aPw40xFbwch_09xTgn",
            "expires_in": 300,
            "id_token": "eyJhbGciOiJSUzI1NiIsImtpZCI6Imh4ZEhXb0Y5bW4ifQ.eyJhdWQiOiIwMUhZRlpEQ1\
                BTV1dCREVWWkQyRlRBUVlFViIsInN1YiI6IjAxSFYxNzNTSjQxUDBGMFgxQ0FRU1lBVENQIiwiaWF0IjoxN\
                zE2Mzc1NzIwLCJpc3MiOiJodHRwczovL2F1dGgtb2lkYy5sYWIuZWxlbWVudC5kZXYvIiwiZXhwIjoxNzE2\
                Mzc5MzIwLCJhdF9oYXNoIjoieGZIS21qQW83cEVCRmUwTkM5ODJEQSJ9.HQs7Si5gU_5tm2hYaCa3jg0kPO\
                MXGNdpV88MWzG6N9x3yXK0ZGgn58i38HiQTbiyPuhw8OH6baMSjbcVP-KXSDpsSPZbkmp7Ozb50dC0eIebD\
                aVK0EyZ35KQRVc5BFPQBPbq0r_TrcUgjoLRKpoexvdmjfEb2dE-kKse25jfs-bTHKP6jeAyFgR9Emn0RfVx\
                32He32-bRP1NfkBnPNnJse32tF1o8gs7zG-cm7kSUx1wiQbvfSGfETx_mJ-aFGABbVGKQlTrCe32HUTvNbp\
                tT2WXa1t7d3eDuEV_6hZS9LFRdIXhgEcGIZMz_ss3WQsSOKN8Yq2NC8_bNxRAQ-1J3A",
            "refresh_token": "mar_CHFh124AMHsdishuHgLSx1svdKMVQA_080gj2",
            "scope": "openid \
                urn:matrix:org.matrix.msc2967.client:api:* \
                urn:matrix:org.matrix.msc2967.client:device:\
                lKa+6As0PSFtqOMKALottO6hlt3gCpZtaVfHanSUnEE",
            "token_type": "Bearer"
        })
    }

    fn secrets_bundle() -> SecretsBundle {
        let json = json!({
            "cross_signing": {
                "master_key": "rTtSv67XGS6k/rg6/yTG/m573cyFTPFRqluFhQY+hSw",
                "self_signing_key": "4jbPt7jh5D2iyM4U+3IDa+WthgJB87IQN1ATdkau+xk",
                "user_signing_key": "YkFKtkjcsTxF6UAzIIG/l6Nog/G2RigCRfWj3cjNWeM",
            },
        });

        serde_json::from_value(json).expect("We should be able to deserialize a secrets bundle")
    }

    /// This is most of the code that is required to be the other side, the
    /// existing device, of the QR login dance.
    ///
    /// TODO: Expose this as a feature user can use.
    async fn grant_login(
        alice: SecureChannel,
        check_code_receiver: tokio::sync::oneshot::Receiver<CheckCode>,
        behavior: AliceBehaviour,
    ) {
        let alice = alice.connect().await.expect("Alice should be able to connect the channel");

        let check_code =
            check_code_receiver.await.expect("We should receive the check code from bob");

        let mut alice = alice
            .confirm(check_code.to_digit())
            .expect("Alice should be able to confirm the secure channel");

        let message = alice
            .receive_json()
            .await
            .expect("Alice should be able to receive the initial message from Bob");

        assert_let!(QrAuthMessage::LoginProtocol { protocol, .. } = message);
        assert_eq!(protocol, LoginProtocolType::DeviceAuthorizationGrant);

        let message = match behavior {
            AliceBehaviour::DeclinedProtocol => QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::UnsupportedProtocol,
                homeserver: None,
            },
            AliceBehaviour::UnexpectedMessage => QrAuthMessage::LoginDeclined,
            _ => QrAuthMessage::LoginProtocolAccepted,
        };

        alice.send_json(message).await.unwrap();

        let message: QrAuthMessage = alice.receive_json().await.unwrap();
        assert_let!(QrAuthMessage::LoginSuccess = message);

        let message = match behavior {
            AliceBehaviour::UnexpectedMessageInsteadOfSecrets => QrAuthMessage::LoginDeclined,
            AliceBehaviour::RefuseSecrets => QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::DeviceNotFound,
                homeserver: None,
            },
            _ => QrAuthMessage::LoginSecrets(secrets_bundle()),
        };

        alice.send_json(message).await.unwrap();
    }

    async fn mock_oauth_authorization_server(
        server: &MockServer,
        token_response: ResponseTemplate,
    ) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/unstable/org.matrix.msc2965/auth_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(open_id_configuration(server)))
            .expect(1..)
            .named("auth_metadata")
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/registration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "client_id": "01HYFZDCPSWWBDEVZD2FTAQYEV",
                "client_id_issued_at": 1716375696
            })))
            .expect(1)
            .named("registration_endpoint")
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/oauth2/keys.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(keys_json()))
            .named("jwks")
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_code(server)))
            .expect(1)
            .named("device_authorization_endpoint")
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(token_response)
            .named("token_endpoint")
            .mount(server)
            .await;
    }

    #[async_test]
    async fn test_qr_login() {
        let server = MockServer::start().await;
        let rendezvous_server = MockedRendezvousServer::new(&server, "abcdEFG12345").await;
        let (sender, receiver) = tokio::sync::oneshot::channel();

        mock_oauth_authorization_server(&server, ResponseTemplate::new(200).set_body_json(token()))
            .await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/upload"))
            .and(header("authorization", "Bearer mat_z65RpDAbvR5aTr7MzD0aPw40xFbwch_09xTgn"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::KEYS_UPLOAD))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/query"))
            .and(header("authorization", "Bearer mat_z65RpDAbvR5aTr7MzD0aPw40xFbwch_09xTgn"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::new(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Alice should be able to create a secure channel.");

        assert_let!(QrCodeModeData::Reciprocate { server_name } = &alice.qr_code_data().mode_data);

        let bob = Client::builder()
            .server_name_or_homeserver_url(server_name)
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the URL in the QR code");

        let qr_code = alice.qr_code_data().clone();

        let oidc = bob.oidc();
        let login_bob = oidc.login_with_qr_code(&qr_code, client_metadata());
        let mut updates = login_bob.subscribe_to_progress();

        let updates_task = tokio::spawn(async move {
            let mut sender = Some(sender);

            while let Some(update) = updates.next().await {
                match update {
                    LoginProgress::EstablishingSecureChannel { check_code } => {
                        sender
                            .take()
                            .expect("The establishing secure channel update should be received only once")
                            .send(check_code)
                            .expect("Bob should be able to send the check code to Alice");
                    }
                    LoginProgress::Done => break,
                    _ => (),
                }
            }
        });
        let alice_task =
            tokio::spawn(async { grant_login(alice, receiver, AliceBehaviour::HappyPath).await });

        join!(
            async {
                login_bob.await.expect("Bob should be able to login");
            },
            async {
                alice_task.await.expect("Alice should have completed it's task successfully");
            },
            async { updates_task.await.unwrap() }
        );

        assert!(bob.encryption().cross_signing_status().await.unwrap().is_complete());
        let own_identity =
            bob.encryption().get_user_identity(bob.user_id().unwrap()).await.unwrap().unwrap();

        assert!(own_identity.is_verified());
    }

    async fn test_failure(
        token_response: ResponseTemplate,
        alice_behavior: AliceBehaviour,
    ) -> Result<(), QRCodeLoginError> {
        let server = MockServer::start().await;
        let rendezvous_server = MockedRendezvousServer::new(&server, "abcdEFG12345").await;
        let (sender, receiver) = tokio::sync::oneshot::channel();

        mock_oauth_authorization_server(&server, token_response).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::new(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Alice should be able to create a secure channel.");

        assert_let!(QrCodeModeData::Reciprocate { server_name } = &alice.qr_code_data().mode_data);

        let bob = Client::builder()
            .server_name_or_homeserver_url(server_name)
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the URL in the QR code");

        let qr_code = alice.qr_code_data().clone();

        let oidc = bob.oidc();
        let login_bob = oidc.login_with_qr_code(&qr_code, client_metadata());
        let mut updates = login_bob.subscribe_to_progress();

        let _updates_task = tokio::spawn(async move {
            let mut sender = Some(sender);

            while let Some(update) = updates.next().await {
                match update {
                    LoginProgress::EstablishingSecureChannel { check_code } => {
                        sender
                            .take()
                            .expect("The establishing secure channel update should be received only once")
                            .send(check_code)
                            .expect("Bob should be able to send the check code to Alice");
                    }
                    LoginProgress::Done => break,
                    _ => (),
                }
            }
        });
        let _alice_task =
            tokio::spawn(async move { grant_login(alice, receiver, alice_behavior).await });
        login_bob.await
    }

    #[async_test]
    async fn test_qr_login_refused_access_token() {
        let result = test_failure(
            ResponseTemplate::new(400).set_body_json(json!({
                "error": "access_denied",
            })),
            AliceBehaviour::HappyPath,
        )
        .await;

        assert_let!(Err(QRCodeLoginError::Oauth(e)) = result);
        assert_eq!(
            e.as_request_token_error(),
            Some(&DeviceCodeErrorResponseType::AccessDenied),
            "The server should have told us that access has been denied."
        );
    }

    #[async_test]
    async fn test_qr_login_expired_token() {
        let result = test_failure(
            ResponseTemplate::new(400).set_body_json(json!({
                "error": "expired_token",
            })),
            AliceBehaviour::HappyPath,
        )
        .await;

        assert_let!(Err(QRCodeLoginError::Oauth(e)) = result);
        assert_eq!(
            e.as_request_token_error(),
            Some(&DeviceCodeErrorResponseType::ExpiredToken),
            "The server should have told us that access has been denied."
        );
    }

    #[async_test]
    async fn test_qr_login_declined_protocol() {
        let result = test_failure(
            ResponseTemplate::new(200).set_body_json(token()),
            AliceBehaviour::DeclinedProtocol,
        )
        .await;

        assert_let!(Err(QRCodeLoginError::LoginFailure { reason, .. }) = result);
        assert_eq!(
            reason,
            LoginFailureReason::UnsupportedProtocol,
            "Alice should have told us that the protocol is unsupported."
        );
    }

    #[async_test]
    async fn test_qr_login_unexpected_message() {
        let result = test_failure(
            ResponseTemplate::new(200).set_body_json(token()),
            AliceBehaviour::UnexpectedMessage,
        )
        .await;

        assert_let!(Err(QRCodeLoginError::UnexpectedMessage { expected, .. }) = result);
        assert_eq!(expected, "m.login.protocol_accepted");
    }

    #[async_test]
    async fn test_qr_login_unexpected_message_instead_of_secrets() {
        let result = test_failure(
            ResponseTemplate::new(200).set_body_json(token()),
            AliceBehaviour::UnexpectedMessageInsteadOfSecrets,
        )
        .await;

        assert_let!(Err(QRCodeLoginError::UnexpectedMessage { expected, .. }) = result);
        assert_eq!(expected, "m.login.secrets");
    }

    #[async_test]
    async fn test_qr_login_refuse_secrets() {
        let result = test_failure(
            ResponseTemplate::new(200).set_body_json(token()),
            AliceBehaviour::RefuseSecrets,
        )
        .await;

        assert_let!(Err(QRCodeLoginError::LoginFailure { reason, .. }) = result);
        assert_eq!(reason, LoginFailureReason::DeviceNotFound);
    }
}
