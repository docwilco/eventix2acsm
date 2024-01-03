use anyhow::{anyhow, Context, Result};
use itertools::Itertools;
use std::collections::HashMap;

use crate::acsm::BasicDriver;

pub struct MetaDataIDs {
    pub first_name: String,
    pub last_name: String,
    pub team_name: String,
    pub steam_id: String,
}

pub async fn get_single_order(
    api_token: &str,
    event_guid: &str,
    ticket_to_car_map: &HashMap<String, String>,
    metadata_ids: &MetaDataIDs,
    order_id: &str,
) -> Result<Vec<BasicDriver>> {
    let client = reqwest::Client::new();
    let url = format!("https://api.eventix.io/3.0.0/order/{}", order_id);
    let request = client.get(url).bearer_auth(api_token);
    let response: serde_json::Value = request
        .send()
        .await
        .context("getting single order from Eventix API failed")?
        .error_for_status()
        .context("Eventix API returned error")?
        .json()
        .await
        .context("Eventix API returned bad JSON")?;
    if response
        .get("status")
        .context("Order is missing status field")?
        .as_str()
        .context("Order status is not a string")?
        != "paid"
    {
        return Err(anyhow!("Order is not paid, this should not happen"));
    }
    let tickets = response["tickets"]
        .as_array()
        .context("tickets is not an array")?;
    tickets
        .iter()
        .map(|ticket| {
            if ticket
                .get("ticket")
                .context("missing ticket member")?
                .get("event_id")
                .context("missing event_id member")?
                .as_str()
                .context("event_id is not a string")?
                != event_guid
            {
                Ok(None)
            } else {
                Ok(Some(ticket_to_driver(ticket_to_car_map, metadata_ids)(
                    ticket,
                )?))
            }
        })
        .filter_map_ok(|x| x)
        .collect()
}

pub async fn get_orders(
    api_token: &str,
    event_guid: &str,
    ticket_id_to_car_map: &HashMap<String, String>,
    metadata_ids: &MetaDataIDs,
) -> Result<Vec<BasicDriver>> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.eventix.io/3.0.0/statistics/event/{}",
        event_guid
    );
    let request = client.get(url).bearer_auth(api_token);
    let response: serde_json::Value = request
        .send()
        .await
        .context("Getting orders from Eventix API failed")?
        .error_for_status()
        .context("Eventix API returned error")?
        .json()
        .await
        .context("Eventix API returned bad JSON")?;
    let hits = response
        .get("hits")
        .context("Missing hits field in JSON")?
        .get("hits")
        .context("Missing hits->hits field in JSON")?
        .as_array()
        .context("hits->hits is not an array")?;
    hits.iter()
        .filter_map(|hit| {
            let source = hit["_source"].as_object().unwrap();
            let status = source["status"].as_str().unwrap();
            if status != "paid" {
                return None;
            }
            let tickets = source["tickets"].as_array().unwrap();
            Some(
                tickets
                    .iter()
                    .map(ticket_to_driver(ticket_id_to_car_map, metadata_ids)),
            )
        })
        .flatten()
        .collect()
}

fn ticket_to_driver<'a>(
    ticket_to_car_map: &'a HashMap<String, String>,
    metadata_ids: &'a MetaDataIDs,
) -> impl Fn(&serde_json::Value) -> Result<BasicDriver> + 'a {
    move |ticket| match ticket_to_car_map.get(ticket["ticket_id"].as_str().unwrap()) {
        Some(car) => {
            let mut first_name = None;
            let mut last_name = None;
            let mut team_name = None;
            let mut steam_id = None;
            for metadata_item in ticket["meta_data"].as_array().unwrap() {
                let metadata_id = metadata_item["metadata_id"].as_str().unwrap();
                if metadata_id == metadata_ids.first_name {
                    first_name = Some(metadata_item["value"].as_str().unwrap().trim());
                } else if metadata_id == metadata_ids.last_name {
                    last_name = Some(metadata_item["value"].as_str().unwrap().trim());
                } else if metadata_id == metadata_ids.team_name {
                    team_name = Some(metadata_item["value"].as_str().unwrap().trim());
                } else if metadata_id == metadata_ids.steam_id {
                    steam_id = Some(metadata_item["value"].as_str().unwrap().trim());
                }
            }
            if first_name.is_none() || last_name.is_none() || steam_id.is_none() {
                return Err(anyhow!(
                    "Missing metadata for ticket: {:?}",
                    ticket.get("guid")
                ));
            }

            Ok(BasicDriver {
                name: format!("{} {}", first_name.unwrap(), last_name.unwrap()),
                car: car.clone(),
                steam_id: steam_id.unwrap().parse().unwrap(),
                team_name: team_name.map(|x| x.to_string()),
            })
        }
        None => Err(anyhow!("No car found for ticket: {}", ticket["ticket_id"])),
    }
}
