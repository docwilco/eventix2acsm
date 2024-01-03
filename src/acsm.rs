use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use serde::Deserialize;
use serde_json::Value;
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::fs;

#[derive(Debug, Deserialize)]
pub struct BasicDriver {
    pub name: String,
    pub car: String,
    pub steam_id: u64,
    pub team_name: Option<String>,
}

async fn get_modified_time(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .await
        .context("failed to stat the JSON file")?
        .modified()
        .context("last modified time not available on this platform")
}

async fn read_json_file(json_file: &Path) -> Result<(Value, SystemTime)> {
    // To minimize chances of ACSM messing things up while we read/write, get
    // the modification time of the file before we start. If at any point this
    // changes, we restart the process.
    let last_modified = get_modified_time(json_file).await?;
    // Read file to JSON Value
    let json_text = fs::read_to_string(json_file)
        .await
        .context("Reading JSON file")?;
    let data: Value = serde_json::from_str(&json_text)?;
    // Check if the file has been modified since we started
    if last_modified != get_modified_time(json_file).await? {
        warn!(
            "File {} modified while reading, restarting add/update process",
            json_file.display()
        );
        return Err(anyhow!("JSON file modified while reading"));
    }
    Ok((data, last_modified))
}

async fn write_json_file(json_file: &Path, data: &Value, last_modified: SystemTime) -> Result<()> {
    // Check if the file has been modified since we started
    if last_modified != get_modified_time(json_file).await? {
        warn!("File {} modified while updating data", json_file.display());
        return Err(anyhow!("JSON file modified while updating data"));
    }
    // Write the file back out, to a temporary file first
    let random_extension = radix_fmt::radix(rand::random::<u64>(), 36).to_string();
    let mut tmp_filename = json_file.as_os_str().to_os_string();
    tmp_filename.push(".");
    tmp_filename.push(random_extension);
    let new_json_text = serde_json::to_string_pretty(&data)?;
    fs::write(&tmp_filename, new_json_text).await?;

    // Check if the file has been modified since we started
    if last_modified != get_modified_time(json_file).await? {
        warn!(
            "File {} modified while writing temporary file",
            json_file.display()
        );
        return Err(anyhow!("JSON file modified while writing temporary file"));
    }

    // If it has not changed, rename the original to a backup name, and then the
    // temporary file to the original file
    let mut backup_filename = json_file.as_os_str().to_os_string();
    backup_filename.push(".backup_");
    let since_epoch = last_modified.duration_since(UNIX_EPOCH).unwrap();
    backup_filename.push(&format!("{}", since_epoch.as_secs()));
    let backup_filename = Path::new(&backup_filename);
    fs::rename(json_file, &backup_filename).await?;
    fs::rename(tmp_filename, json_file).await?;
    info!(
        "Update complete, backup file: {}",
        backup_filename.display()
    );
    Ok(())
}

async fn delete_missing_drivers(
    data: &mut Value,
    drivers: &[BasicDriver],
    ignored_steam_ids: &[u64],
) -> Result<()> {
    // Get the classes array
    let classes = data
        .get_mut("Classes")
        .context("Classes not found in JSON")?
        .as_array_mut()
        .context("Classes is not an array")?;
    // Go through each class
    for class in classes {
        let available_cars = class
            .get("AvailableCars")
            .context("AvailableCars not found in class")?
            .as_array()
            .context("AvailableCars is not an array")?
            .iter()
            .map(|car| {
                Ok(car
                    .as_str()
                    .context("Contents of AvailableCars is not all Strings")?
                    .to_string())
            })
            .collect::<Result<Vec<_>>>()?;
        let entrants = class["Entrants"].as_object_mut().unwrap();
        // Go through each entrant
        for (_slot, entrant) in entrants.iter_mut() {
            // Check if the entrant is in the list of drivers
            let steam_id = entrant["GUID"].as_str().unwrap();
            if steam_id.is_empty() {
                continue;
            }
            let steam_id = entrant["GUID"].as_str().unwrap().parse::<u64>().unwrap();
            // Check if the entrant is in the list of ignored steam ids
            if ignored_steam_ids.contains(&steam_id) {
                continue;
            }
            // If there's any driver that has the Steam ID and has a car in the
            // current class, then we don't delete it
            if drivers.iter().any(|driver| {
                driver.steam_id == steam_id && available_cars.iter().any(|car| car == &driver.car)
            }) {
                continue;
            }
            // Otherwise, delete it
            debug!(
                "Driver not in Eventix, deleting: {} steam_id={} car={}{}",
                entrant["Name"],
                steam_id,
                entrant["Model"],
                if entrant["Team"].as_str().unwrap().is_empty() {
                    "".to_string()
                } else {
                    format!(" team_name={}", entrant["Team"])
                }
            );
            entrant["Name"] = "".into();
            entrant["Team"] = "".into();
            entrant["GUID"] = "".into();
        }
    }
    Ok(())
}

async fn update_drivers_inner(
    delete_missing: bool,
    json_file: &Path,
    drivers: &[BasicDriver],
    ignored_steam_ids: &[u64],
) -> Result<()> {
    let (mut data, last_modified) = read_json_file(json_file).await?;
    if delete_missing {
        delete_missing_drivers(&mut data, drivers, ignored_steam_ids).await?;
    }
    // Get the classes array
    let classes = data
        .get_mut("Classes")
        .context("Classes not found in JSON")?
        .as_array_mut()
        .context("Classes is not an array")?;
    // Go through each supplied driver and update them, or add them to the
    // correct class
    for driver in drivers {
        debug!(
            "Adding driver: {} steam_id={} car={}{}",
            driver.name,
            driver.steam_id,
            driver.car,
            if let Some(team_name) = &driver.team_name {
                format!(" team_name={}", team_name)
            } else {
                "".to_string()
            }
        );
        let class = classes
            .iter_mut()
            .find(|class| {
                class
                    .get("AvailableCars")
                    .context("AvailableCars not found in class")
                    .unwrap()
                    .as_array()
                    .context("AvailableCars is not an array")
                    .unwrap()
                    .contains(&driver.car.clone().into())
            })
            .unwrap_or_else(|| panic!("Can't find class with car: {}", driver.car));
        let entrants = class["Entrants"].as_object_mut().unwrap();
        // Check by steam id if the driver is already there
        let steam_id_str = driver.steam_id.to_string();
        let mut entry_slot = entrants.iter_mut().find_map(|(_, entrant)| {
            if entrant["GUID"] == steam_id_str {
                debug!("Updating existing driver by steam_id={}", driver.steam_id);
                Some(entrant)
            } else {
                None
            }
        });
        // If not, get empty slot (which should be by empty GUID)
        if entry_slot.is_none() {
            entry_slot = entrants.iter_mut().find_map(|(slot, entrant)| {
                if entrant["GUID"].as_str().unwrap().is_empty() {
                    debug!("Adding new driver to slot: {}", slot);
                    Some(entrant)
                } else {
                    None
                }
            })
        }
        if let Some(entry_slot) = entry_slot {
            entry_slot["Name"] = driver.name.clone().into();
            entry_slot["Team"] = driver.team_name.clone().unwrap_or_default().into();
            entry_slot["GUID"] = steam_id_str.into();
        } else {
            return Err(anyhow!("Couldn't find empty slot for: {:?}", driver));
        }
    }
    write_json_file(json_file, &data, last_modified).await
}

pub async fn update_drivers(
    delete_missing: bool,
    json_file: &Path,
    drivers: &[BasicDriver],
    ignored_steam_ids: &[u64],
) -> Result<()> {
    info!(
        "Adding/updating {} drivers to {}",
        drivers.len(),
        json_file.display()
    );
    let mut retries = 0_usize;
    let mut wait_time = Duration::from_millis(125);
    let max_wait_time = Duration::from_secs(16);
    loop {
        match update_drivers_inner(delete_missing, json_file, drivers, ignored_steam_ids).await {
            Ok(_) => break,
            Err(e) => {
                warn!(
                    "Error adding/updating drivers: {} (retries: {})",
                    e, retries
                );
                /* TODO: alert if retries above X */
                tokio::time::sleep(wait_time).await;
                if wait_time < max_wait_time {
                    wait_time *= 2;
                }
            }
        }
        retries += 1;
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::fs;
    use test_case::test_case;

    #[test_case("fixtures/test.json", "fixtures/test_add_all_new_drivers.json"; "add all new drivers")]
    #[test_case("fixtures/test.json", "fixtures/test_add_one_update_one.json"; "add one update one")]
    #[tokio::test]
    async fn test(in_json: &str, drivers_json: &str) {
        let out_json = drivers_json.replace(".json", "_output.json");
        // Copy the test file from fixtures to a tempdir
        let tempdir = tempfile::tempdir().unwrap();
        let json_file = tempdir.path().join("test.json");
        fs::copy(in_json, &json_file).unwrap();
        let drivers_strings = fs::read_to_string(drivers_json).unwrap();
        let drivers: Vec<BasicDriver> = serde_json::from_str(&drivers_strings).unwrap();
        update_drivers_inner(false, &json_file, &drivers, &[])
            .await
            .unwrap();
        // diff the output file with the expected output file
        let output = fs::read_to_string(&json_file).unwrap();
        let expected_output = fs::read_to_string(out_json).unwrap();
        assert!(output == expected_output);
    }

    #[test_case("fixtures/test.json", "fixtures/too_many_drivers.json"; "too many drivers")]
    #[tokio::test]
    #[should_panic]
    async fn panic_test(in_json: &str, drivers_json: &str) {
        let tempdir = tempfile::tempdir().unwrap();
        let json_file = tempdir.path().join("test.json");
        fs::copy(in_json, &json_file).unwrap();
        let drivers_strings = fs::read_to_string(drivers_json).unwrap();
        let drivers: Vec<BasicDriver> = serde_json::from_str(&drivers_strings).unwrap();
        update_drivers_inner(false, &json_file, &drivers, &[])
            .await
            .unwrap();
    }
}
