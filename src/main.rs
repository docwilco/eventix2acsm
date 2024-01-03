use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{self, Request},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use axum_macros::debug_handler;
use itertools::Itertools;
use log::{debug, error, info, warn};
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    task::JoinHandle,
    time::sleep,
};

mod acsm;
mod eventix;
mod oauth2;

use crate::oauth2::{OAuth2State, handle_oauth2_callback, refresh_token_task, setup_oauth2_client};

struct State {
    acsm_json_file: Mutex<PathBuf>,
    eventix_event_guid: String,
    ticket_id_to_car_map: HashMap<String, String>,
    metadata_ids: eventix::MetaDataIDs,
    ignored_steam_ids: Vec<u64>,
    oauth2_state: Mutex<OAuth2State>,
    full_update_task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookPayload {
    date_time: String,
    event: String,
    event_key: String,
    guid: String,
}

async fn full_update(state: Arc<State>) -> Result<()> {
    let oauth2_state = state.oauth2_state.lock().await;
    if oauth2_state.token.is_none() {
        error!("No OAuth2 token, skipping full update");
        return Ok(());
    }
    let api_token = oauth2_state.token.as_ref().unwrap().secret().clone();
    drop(oauth2_state);
    let all_drivers = eventix::get_orders(
        &api_token,
        &state.eventix_event_guid,
        &state.ticket_id_to_car_map,
        &state.metadata_ids,
    )
    .await
    .context("Failed to get orders")?;
    let acsm_json_file = state.acsm_json_file.lock().await;
    acsm::update_drivers(
        true,
        &acsm_json_file,
        &all_drivers,
        &state.ignored_steam_ids,
    )
    .await
    .context("Failed to update drivers")?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let state = State {
        acsm_json_file: Mutex::new(
            dotenv::var("ACSM_JSON_FILE")
                .context("ACSM_JSON_FILE not set")?
                .into(),
        ),
        eventix_event_guid: dotenv::var("EVENTIX_EVENT_GUID")
            .context("EVENTIX_EVENT_GUID not set")?,
        ticket_id_to_car_map: dotenv::var("TICKET_ID_TO_CAR_MAP")
            .context("TICKET_ID_TO_CAR_MAP not set")?
            .split(',')
            .map(|pair| {
                let pair = pair
                    .split_once(':')
                    .context("Missing : separator in TICKET_ID_TO_CAR_MAP")?;
                Ok((pair.0.to_string(), pair.1.to_string()))
            })
            .collect::<Result<_>>()?,
        metadata_ids: eventix::MetaDataIDs {
            first_name: dotenv::var("EVENTIX_METADATA_FIRST_NAME")
                .context("EVENTIX_METADATA_FIRST_NAME not set")?,
            last_name: dotenv::var("EVENTIX_METADATA_LAST_NAME")
                .context("EVENTIX_METADATA_LAST_NAME not set")?,
            team_name: dotenv::var("EVENTIX_METADATA_TEAM_NAME")
                .context("EVENTIX_METADATA_TEAM_NAME not set")?,
            steam_id: dotenv::var("EVENTIX_METADATA_STEAM_ID")
                .context("EVENTIX_METADATA_STEAM_ID not set")?,
        },
        ignored_steam_ids: dotenv::var("IGNORED_STEAM_IDS")
            .unwrap_or_else(|_| "".to_string())
            .split(',')
            .map(|id| {
                if id.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(id.parse::<u64>()?))
                }
            })
            .filter_map_ok(|id| id)
            .collect::<Result<Vec<_>>>()?,
        oauth2_state: Mutex::new(setup_oauth2_client().await?),
        full_update_task: Mutex::new(None),
    };
    if state.ticket_id_to_car_map.is_empty() {
        return Err(anyhow!("TICKET_ID_TO_CAR_MAP is empty"));
    }
    let state = Arc::new(state);
    let app = Router::new()
        .route(
            "/eventix/webhook-old/v1/order-paid",
            post(handle_order_paid),
        )
        .route("/eventix/oauth2/v1/callback", get(handle_oauth2_callback))
        .fallback(handler)
        .with_state(state.clone())
        .layer(middleware::from_fn(log_request));

    let listen_address = dotenv::var("LISTEN_ADDRESS").context("LISTEN_ADDRESS not set")?;
    let listener = tokio::net::TcpListener::bind(&listen_address)
        .await
        .with_context(|| format!("Failed to bind to {}", listen_address))?;
    info!("listening on {}", listener.local_addr().unwrap());
    refresh_token_task(state).await;
    axum::serve(listener, app)
        .await
        .context("Failed to start Axum server")?;
    Ok(())
}

async fn full_update_task(state: Arc<State>) {
    let state_clone = state.clone();
    let mut full_update_task = state.full_update_task.lock().await;
    if full_update_task.is_some() {
        return;
    }
    full_update_task.replace(tokio::spawn(async move {
        loop {
            let result = full_update(state_clone.clone()).await;
            if let Err(e) = result {
                error!("Full update failed: {}", e);
            }
            sleep(Duration::from_secs(60 * 60)).await;
        }
    }));
}

async fn log_request(
    req: Request,
    next: Next,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    info!("request: {:?}", req);
    let res = next.run(req).await;
    info!("response: {:?}", res);
    Ok(res)
}

async fn handler(extract::Json(payload): extract::Json<serde_json::Value>) -> Html<&'static str> {
    info!("payload: {:?}", payload.to_string());
    Html("received")
}


#[debug_handler]
async fn handle_order_paid(
    extract::State(state): extract::State<Arc<State>>,
    Json(payload): Json<WebhookPayload>,
) -> Result<Html<&'static str>, StatusCode> {
    debug!(
        "order-paid payload: guid={} event={} event_key={} date_time={}",
        payload.guid, payload.event, payload.event_key, payload.date_time
    );
    if payload.event != "order-paid" {
        warn!("Received event {} instead of order-paid", payload.event);
        return Err(StatusCode::BAD_REQUEST);
    }
    let oauth2_state = state.oauth2_state.lock().await;
    if oauth2_state.token.is_none() {
        error!("No OAuth2 token, skipping order update");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let api_token = oauth2_state.token.as_ref().unwrap().secret().clone();
    drop(oauth2_state);
    let new_drivers = eventix::get_single_order(
        &api_token,
        &state.eventix_event_guid,
        &state.ticket_id_to_car_map,
        &state.metadata_ids,
        &payload.guid,
    )
    .await
    .unwrap();
    let acsm_json_file = state.acsm_json_file.lock().await;
    acsm::update_drivers(
        false,
        &acsm_json_file,
        &new_drivers,
        &state.ignored_steam_ids,
    )
    .await
    .unwrap();
    Ok(Html("received"))
}

