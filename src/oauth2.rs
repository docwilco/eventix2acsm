use anyhow::{Context, Result};
use axum::{extract, http::StatusCode, response::Html};
use axum_macros::debug_handler;
use log::{error, info};
use oauth2::{
    basic::BasicClient, reqwest::async_http_client, AccessToken, AuthUrl, AuthorizationCode,
    ClientId, ClientSecret, CsrfToken, ExtraTokenFields, RedirectUrl, RefreshToken,
    StandardTokenResponse, TokenResponse, TokenType, TokenUrl,
};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, sleep_until, sleep};

use crate::State;

#[derive(Debug, Deserialize)]
pub struct OAuth2CallbackParameters {
    pub code: String,
    pub state: String,
}

#[derive(Debug)]
pub struct OAuth2State {
    pub client: BasicClient,
    pub csrf_token: CsrfToken,
    pub token: Option<AccessToken>,
    pub token_expires: Option<Instant>,
    pub refresh_token: Option<RefreshToken>,
}

pub async fn setup_oauth2_client() -> Result<OAuth2State> {
    let client_id = ClientId::new(
        dotenv::var("EVENTIX_OAUTH2_CLIENT_ID").context("EVENTIX_OAUTH2_CLIENT_ID not set")?,
    );
    let client_secret = ClientSecret::new(
        dotenv::var("EVENTIX_OAUTH2_CLIENT_SECRET")
            .context("EVENTIX_OAUTH2_CLIENT_SECRET not set")?,
    );
    let auth_url = AuthUrl::new(
        dotenv::var("EVENTIX_OAUTH2_AUTH_URL")
            .context("EVENTIX_OAUTH2_AUTH_URL not set")?
            .to_string(),
    )
    .context("Failed to create OAuth2 AuthURL")?;
    let token_url = TokenUrl::new(
        dotenv::var("EVENTIX_OAUTH2_TOKEN_URL")
            .context("EVENTIX_OAUTH2_TOKEN_URL not set")?
            .to_string(),
    )
    .context("Failed to create OAuth2 TokenURL")?;
    let redirect_url = RedirectUrl::new(
        dotenv::var("EVENTIX_OAUTH2_REDIRECT_URL")
            .context("EVENTIX_OAUTH2_REDIRECT_URL not set")?
            .to_string(),
    )
    .context("Failed to create OAuth2 RedirectURL")?;
    let client = BasicClient::new(client_id, Some(client_secret), auth_url, Some(token_url))
        .set_redirect_uri(redirect_url);
    let (auth_url, csrf_token) = client.authorize_url(CsrfToken::new_random).url();
    println!("Browse to: {}", auth_url);
    Ok(OAuth2State {
        client,
        csrf_token,
        token: None,
        token_expires: None,
        refresh_token: None,
    })
}

#[debug_handler]
pub async fn handle_oauth2_callback(
    extract::State(state): extract::State<Arc<State>>,
    extract::Query(query): extract::Query<OAuth2CallbackParameters>,
) -> Result<Html<&'static str>, StatusCode> {
    info!("oauth2 callback: {:?}", query);
    let oauth2_state = state.oauth2_state.lock().await;
    if &query.state != oauth2_state.csrf_token.secret() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let token_result = oauth2_state
        .client
        .exchange_code(AuthorizationCode::new(query.code))
        .request_async(async_http_client)
        .await;
    drop(oauth2_state);
    match token_result {
        Ok(token_result) => {
            update_token_in_state(state.clone(), token_result).await;
        }
        Err(e) => {
            error!("Failed to exchange code for token: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    Ok(Html("authentication successful"))
}

async fn update_token_in_state<EF, TT>(
    state: Arc<State>,
    token_result: StandardTokenResponse<EF, TT>,
) where
    EF: ExtraTokenFields,
    TT: TokenType,
{
    info!("Received token");
    let mut oauth2_state = state.oauth2_state.lock().await;
    let token = token_result.access_token().clone();
    let refresh_token = token_result.refresh_token().cloned();
    let token_expires = token_result
        .expires_in()
        .map(|expires_in| Instant::now() + expires_in - Duration::from_secs(60));
    oauth2_state.token = Some(token);
    oauth2_state.refresh_token = refresh_token;
    oauth2_state.token_expires = token_expires;
    info!("Refresh token: {:?}", oauth2_state.refresh_token);
    info!("Token expires: {:?}", oauth2_state.token_expires);
    info!("Now: {:?}", Instant::now());
    drop(oauth2_state);
    crate::full_update_task(state).await;
}

async fn refresh_token(state: Arc<State>) {
    let oauth2_state = state.oauth2_state.lock().await;
    if oauth2_state.refresh_token.is_none() {
        error!("No OAuth2 refresh token, should not happen");
        return;
    }
    let refresh_token = oauth2_state.refresh_token.as_ref().unwrap().clone();
    let result = oauth2_state
        .client
        .exchange_refresh_token(&refresh_token)
        .request_async(async_http_client)
        .await;
    match result {
        Ok(token_result) => {
            update_token_in_state(state.clone(), token_result).await;
        }
        Err(e) => {
            error!("Failed to refresh token: {}", e);
        }
    }
}

pub async fn refresh_token_task(state: Arc<State>) {
    tokio::spawn(async move {
        loop {
            let mut oauth2_state = state.oauth2_state.lock().await;
            if let Some(token_expires) = oauth2_state.token_expires {
                if token_expires > Instant::now() {
                    drop(oauth2_state);
                    info!("Sleeping until token expires");
                    sleep_until(token_expires).await;
                    continue;
                }
                if oauth2_state.refresh_token.is_some() {
                    drop(oauth2_state);
                    refresh_token(state.clone()).await;
                } else {
                    oauth2_state.token_expires = None;
                }
            } else {
                drop(oauth2_state);
                info!("No token expiration, sleeping for 1 hour");
                sleep(Duration::from_secs(60 * 60)).await;
            }
        }
    });
}
