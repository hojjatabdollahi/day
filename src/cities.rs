// SPDX-License-Identifier: GPL-3.0-only

use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct TzEntry {
    name: String,
    #[serde(rename = "countryName")]
    country_name: String,
    #[serde(rename = "mainCities", default)]
    main_cities: Vec<String>,
}

/// A single searchable city that maps to an IANA timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CityEntry {
    pub city: String,
    pub timezone: String,
    pub country: String,
}

impl std::fmt::Display for CityEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}, {}", self.city, self.country)
    }
}

/// All cities parsed from the bundled tzdb JSON, sorted alphabetically by city name.
pub static CITIES: LazyLock<Vec<CityEntry>> = LazyLock::new(|| {
    let raw = include_str!("../data/raw-time-zones.json");
    let entries: Vec<TzEntry> = serde_json::from_str(raw).expect("valid tzdb JSON");

    let mut cities: Vec<CityEntry> = entries
        .into_iter()
        .flat_map(|entry| {
            entry.main_cities.into_iter().map(move |city| CityEntry {
                city,
                timezone: entry.name.clone(),
                country: entry.country_name.clone(),
            })
        })
        .collect();

    // UTC has no "city" in the tzdb data, so add it manually as a searchable entry.
    cities.push(CityEntry {
        city: "UTC".to_string(),
        timezone: "UTC".to_string(),
        country: "Coordinated Universal Time".to_string(),
    });

    cities.sort_by(|a, b| a.city.cmp(&b.city));
    cities
});
