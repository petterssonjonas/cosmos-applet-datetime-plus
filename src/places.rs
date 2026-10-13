// SPDX-License-Identifier: GPL-3.0-only

//! Offline GeoNames cities5000 search. Data attribution and coverage are bundled
//! in resources/places-source.txt; regenerate with resources/build-places.py.

use std::sync::LazyLock;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

const MAX_RESULTS: usize = 50;
const MAX_QUERY_BYTES: usize = 512;
const MAX_LABEL_BYTES: usize = 160;
const DATA: &str = include_str!("../resources/places.tsv");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub name: &'static str,
    pub region: &'static str,
    pub country: &'static str,
    pub timezone: &'static str,
}

impl Place {
    /// Geographic label, independent of timezone, within saved-clock limits.
    pub fn label(&self) -> String {
        // Preserve geographic context even when the original city name is long.
        let (country, country_end) = shortened(self.country, 56);
        let (region, region_end) = shortened(self.region, 48);
        let suffix_bytes = 2
            + country.len()
            + country_end.len()
            + if region.is_empty() {
                0
            } else {
                2 + region.len() + region_end.len()
            };
        let name_bytes = MAX_LABEL_BYTES - suffix_bytes;
        // Duplicate source names carry stable IDs; keep that suffix when a long
        // original name must be shortened, so distinct cities remain distinct.
        let (city, identity) = self
            .name
            .rfind(" (GeoNames ")
            .filter(|&offset| {
                self.name[offset + 11..]
                    .strip_suffix(')')
                    .is_some_and(|id| {
                        !id.is_empty()
                            && id.len() <= 20
                            && id.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
            .map_or((self.name, ""), |offset| self.name.split_at(offset));
        let (name, name_end) = shortened(city, name_bytes.saturating_sub(identity.len()));
        let mut label =
            String::with_capacity(name.len() + name_end.len() + identity.len() + suffix_bytes);
        label.push_str(name);
        label.push_str(name_end);
        label.push_str(identity);
        if !region.is_empty() {
            label.push_str(", ");
            label.push_str(region);
            label.push_str(region_end);
        }
        label.push_str(", ");
        label.push_str(country);
        label.push_str(country_end);
        label
    }
}

fn shortened(text: &str, bytes: usize) -> (&str, &'static str) {
    if text.len() <= bytes {
        return (text, "");
    }
    let mut end = bytes.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], "…")
}

struct Metadata {
    name: &'static str,
    keys: &'static str,
}

struct IndexedPlace {
    place: Place,
    names: &'static str,
    region_keys: &'static str,
    country_keys: &'static str,
}

struct PlaceIndex {
    countries: Vec<Metadata>,
    regions: Vec<Metadata>,
    places: Vec<IndexedPlace>,
}

// Fields borrow the embedded TSV. Shared country/region/zone metadata is stored
// once in the file; parsing allocates only this index, never copies city names.
static PLACES: LazyLock<PlaceIndex> = LazyLock::new(|| {
    let mut countries = Vec::<Metadata>::new();
    let mut regions = Vec::<Metadata>::new();
    let mut zones = Vec::<Option<&'static str>>::new();
    let mut places =
        Vec::with_capacity(DATA.lines().filter(|line| line.starts_with("P\t")).count());
    for line in DATA.lines() {
        let mut fields = line.split('\t');
        match fields.next() {
            Some("C" | "R") => {
                let kind = line.as_bytes()[0];
                if let (Some(_id), Some(name), Some(keys)) =
                    (fields.next(), fields.next(), fields.next())
                {
                    let metadata = Metadata { name, keys };
                    if kind == b'C' {
                        countries.push(metadata);
                    } else {
                        regions.push(metadata);
                    }
                }
            }
            Some("T") => {
                if let (Some(_id), Some(zone)) = (fields.next(), fields.next()) {
                    // Validate each distinct zone once, not every city or query.
                    zones.push(zone.parse::<chrono_tz::Tz>().ok().map(|_| zone));
                }
            }
            Some("P") => {
                let (Some(name), Some(region), Some(country), Some(zone), Some(names)) = (
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                ) else {
                    continue;
                };
                let Some(region) = region.parse::<usize>().ok().and_then(|id| regions.get(id))
                else {
                    continue;
                };
                let Some(country) = country
                    .parse::<usize>()
                    .ok()
                    .and_then(|id| countries.get(id))
                else {
                    continue;
                };
                let Some(timezone) = zone
                    .parse::<usize>()
                    .ok()
                    .and_then(|id| zones.get(id))
                    .copied()
                    .flatten()
                else {
                    continue;
                };
                places.push(IndexedPlace {
                    place: Place {
                        name,
                        region: region.name,
                        country: country.name,
                        timezone,
                    },
                    names,
                    region_keys: region.keys,
                    country_keys: country.keys,
                });
            }
            _ => {}
        }
    }
    PlaceIndex {
        countries,
        regions,
        places,
    }
});

fn normalize(query: &str) -> String {
    let mut normalized = String::with_capacity(query.len());
    let mut separator = false;
    for character in query.nfkd().flat_map(char::to_lowercase) {
        // Removing marks, rather than adding spaces, supports decomposed accents.
        if is_combining_mark(character) {
            continue;
        }
        if character.is_alphanumeric() {
            if separator && !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    normalized
}

fn exact(keys: &str, query: &str) -> bool {
    keys.split('|').any(|key| key == query)
}

fn word_match(keys: &str, token: &str) -> bool {
    keys.match_indices(token).any(|(offset, _)| {
        let bytes = keys.as_bytes();
        let start = offset == 0 || matches!(bytes[offset - 1], b' ' | b'|');
        let end = offset + token.len();
        // Short fragments are whole words/codes, not accidental substring hits.
        start && (token.len() > 2 || end == bytes.len() || matches!(bytes[end], b' ' | b'|'))
    })
}

/// Search city aliases, administrative regions and countries without networking.
/// Results prioritize exact city names, then prefixes, then region/country
/// matches; population and source ID order break ties deterministically.
/// All tokens must match word boundaries. At most 50 results are returned.
pub fn search(query: &str) -> Vec<Place> {
    if query.len() > MAX_QUERY_BYTES {
        return Vec::new();
    }
    let query = normalize(query);
    if query.is_empty() {
        return Vec::new();
    }
    let tokens: Vec<_> = query.split_whitespace().collect();
    // ISO country codes must not turn into prefixes of unrelated city names.
    let country_code = query.len() <= 3
        && PLACES
            .countries
            .iter()
            .any(|country| exact(country.keys, &query));
    let region_code = !country_code
        && query.len() <= 2
        && PLACES
            .regions
            .iter()
            .any(|region| exact(region.keys, &query));
    let mut matches = [(u8::MAX, usize::MAX); MAX_RESULTS];
    let mut match_count = 0;
    for (index, place) in PLACES.places.iter().enumerate() {
        if (country_code && !exact(place.country_keys, &query))
            || (region_code && !exact(place.region_keys, &query))
            || !tokens.iter().all(|token| {
                word_match(place.names, token)
                    || word_match(place.region_keys, token)
                    || word_match(place.country_keys, token)
            })
        {
            continue;
        }
        let score = if place.names.split('|').next() == Some(query.as_str()) {
            0
        } else if exact(place.names, &query) {
            1
        } else if place
            .names
            .split('|')
            .any(|name| name.starts_with(query.as_str()))
        {
            2
        } else if exact(place.region_keys, &query) {
            3
        } else if exact(place.country_keys, &query) {
            4
        } else {
            5
        };
        let candidate = (score, index);
        let position = matches[..match_count].partition_point(|existing| *existing < candidate);
        if position < MAX_RESULTS {
            let next_count = (match_count + 1).min(MAX_RESULTS);
            matches.copy_within(position..next_count - 1, position + 1);
            matches[position] = candidate;
            match_count = next_count;
        }
    }
    matches[..match_count]
        .iter()
        .map(|&(_, index)| PLACES.places[index].place)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn city(query: &str, name: &str, region: &str, country: &str) -> Place {
        search(query)
            .into_iter()
            .find(|place| place.name == name && place.region == region && place.country == country)
            .unwrap_or_else(|| panic!("No {name}, {region}, {country} result for {query}"))
    }

    #[test]
    fn texas_cities_have_their_actual_shared_timezone_and_distinct_labels() {
        let dallas = city("Dallas", "Dallas", "Texas", "United States");
        let austin = city("Austin", "Austin", "Texas", "United States");
        assert_eq!(dallas.timezone, "America/Chicago");
        assert_eq!(austin.timezone, "America/Chicago");
        assert_ne!(dallas.label(), austin.label());
        let texas = search("Texas");
        assert!(texas.contains(&dallas) && texas.contains(&austin));
    }

    #[test]
    fn region_search_returns_cities_on_both_sides_of_a_timezone_boundary() {
        let texas = search("Texas");
        assert!(
            texas
                .iter()
                .any(|place| place.name == "El Paso" && place.timezone == "America/Denver")
        );
        assert!(
            texas
                .iter()
                .any(|place| place.name == "Dallas" && place.timezone == "America/Chicago")
        );
    }

    #[test]
    fn country_and_state_codes_are_not_arbitrary_city_substrings() {
        assert!(
            search("US")
                .iter()
                .all(|place| place.country == "United States")
        );
        assert_eq!(search("US").len(), MAX_RESULTS);
        assert!(search("TX").iter().all(|place| place.region == "Texas"));
        assert_eq!(
            city("Dallas TX US", "Dallas", "Texas", "United States").timezone,
            "America/Chicago"
        );
        assert!(!word_match("austin|dallas", "us"));
        assert!(!word_match("dallas", "allas"));
    }

    #[test]
    fn worldwide_country_region_and_source_aliases_are_searchable() {
        assert_eq!(
            city("Munich Bavaria Germany", "Munich", "Bavaria", "Germany").timezone,
            "Europe/Berlin"
        );
        assert_eq!(
            city("München", "Munich", "Bavaria", "Germany").timezone,
            "Europe/Berlin"
        );
        assert_eq!(
            city("Calcutta", "Kolkata", "West Bengal", "India").timezone,
            "Asia/Kolkata"
        );
        assert_eq!(
            city("Delhi India", "Delhi", "Delhi", "India").timezone,
            "Asia/Kolkata"
        );
        assert_eq!(
            city("New Delhi India", "New Delhi", "Delhi", "India").timezone,
            "Asia/Kolkata"
        );
        assert!(
            search("GBR")
                .iter()
                .all(|place| place.country == "United Kingdom")
        );
    }

    #[test]
    fn accent_and_ascii_searches_share_normalization() {
        let expected = city("Sao Paulo Brazil", "São Paulo", "São Paulo", "Brazil");
        assert!(search("SÃO PAULO Brazil").contains(&expected));
        assert!(search("Sa\u{0303}o Paulo Brazil").contains(&expected));
        assert_eq!(normalize("  MÜNCHEN,  Germany "), "munchen germany");
    }

    #[test]
    fn search_boundaries_and_order_are_deterministic() {
        assert!(search("").is_empty());
        assert!(search(" \t--- ").is_empty());
        assert!(search(&"a".repeat(MAX_QUERY_BYTES + 1)).is_empty());
        assert!(search("thiscitydoesnotexistanywhere").is_empty());
        let first = search("India");
        assert_eq!(first.len(), MAX_RESULTS);
        assert_eq!(first, search("India"));
    }

    #[test]
    fn all_bundled_places_have_valid_timezones_and_utf8_bounded_labels() {
        // A dependency's timezone database must not silently drop bundled cities.
        assert_eq!(
            PLACES.places.len(),
            DATA.lines().filter(|line| line.starts_with("P\t")).count()
        );
        for indexed in &PLACES.places {
            let label = indexed.place.label();
            assert!(!label.trim().is_empty() && label.len() <= MAX_LABEL_BYTES);
            assert!(!label.chars().any(char::is_control));
        }
    }

    #[test]
    fn long_multibyte_labels_preserve_city_region_and_country() {
        let place = Place {
            name: "東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都東京都",
            region: "Île-de-France",
            country: "France",
            timezone: "Europe/Paris",
        };
        let label = place.label();
        assert!(label.len() <= MAX_LABEL_BYTES && label.ends_with(", Île-de-France, France"));
        assert!(label.starts_with("東京都") && label.contains('…'));
    }
}
