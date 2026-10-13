#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Generate offline GeoNames city search assets using Python's standard library.

Usage: python3 resources/build-places.py --source-dir /path/to/geonames-snapshot
The directory must contain cities5000.zip, admin1CodesASCII.txt, countryInfo.txt.
Without --source-dir, download current public GeoNames exports into a temporary
folder. Preserve the three original exports to reproduce a particular snapshot;
places-source.txt records their SHA-256 hashes. No private data is read or sent.
"""

import argparse
import collections
import datetime
import hashlib
import io
from pathlib import Path
import tempfile
import unicodedata
import urllib.request
import zipfile
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

BASE = "https://download.geonames.org/export/dump/"
FILES = ("cities5000.zip", "admin1CodesASCII.txt", "countryInfo.txt")


def fold(character):
    if unicodedata.category(character).startswith("M"):
        return ""
    text = unicodedata.normalize("NFKD", character).lower()
    return "".join(c if c.isalnum() else " " for c in text if not unicodedata.category(c).startswith("M"))


def normalize(text):
    return " ".join("".join(map(fold, text)).split())


def aliases(values):
    # Keep every distinct source alias, including non-Latin names and supplied
    # ASCII transliterations. Pipes separate complete names for exact matching.
    return "|".join(dict.fromkeys(key for value in values if (key := normalize(value))))


def clean(text):
    return " ".join(text.split())


def rows(content):
    return [line.split("\t") for line in content.decode("utf-8").splitlines() if line and not line.startswith("#")]


def generate(source, output, snapshot_date):
    contents = {name: (source / name).read_bytes() for name in FILES}
    countries = {row[0]: row for row in rows(contents["countryInfo.txt"])}
    regions = {row[0]: row for row in rows(contents["admin1CodesASCII.txt"])}
    with zipfile.ZipFile(io.BytesIO(contents["cities5000.zip"])) as archive:
        cities = rows(archive.read("cities5000.txt"))
    source_count = len(cities)
    skipped = collections.Counter()
    accepted = []
    zones = set()
    for row in cities:
        if len(row) != 19 or row[6] != "P":
            skipped["malformed/non-populated feature"] += 1
            continue
        if row[8] not in countries:
            skipped["missing country metadata"] += 1
            continue
        try:
            ZoneInfo(row[17])
        except (ValueError, ZoneInfoNotFoundError):
            skipped["missing/unknown IANA timezone"] += 1
            continue
        accepted.append(row)
        zones.add(row[17])
    # Population order makes bounded results useful for entire countries/states;
    # source IDs break ties without relying on upstream input order.
    accepted.sort(key=lambda row: (-int(row[14]), normalize(row[1]), int(row[0])))
    display_regions = {code: clean(row[1]) for code, row in regions.items()}
    duplicates = collections.Counter(
        (clean(row[1]), row[8], display_regions.get(row[8] + "." + row[10], ""))
        for row in accepted
    )
    used_countries = sorted({row[8] for row in accepted})
    used_regions = sorted({row[8] + "." + row[10] for row in accepted})
    zone_ids = {zone: str(index) for index, zone in enumerate(sorted(zones))}
    country_ids = {code: str(index) for index, code in enumerate(used_countries)}
    region_ids = {code: str(index) for index, code in enumerate(used_regions)}
    lines = [
        "# GeoNames cities5000 derivative; CC BY 4.0; https://www.geonames.org/",
        f"# Snapshot {snapshot_date}; attribution, coverage and input hashes: places-source.txt",
        "# C: index, country, normalized aliases; R: index, region, normalized aliases",
        "# T: index, IANA timezone; P: name, region index, country index, timezone index, normalized city aliases",
    ]
    for code in used_countries:
        row = countries[code]
        values = [row[4], row[0], row[1], row[9].lstrip(".")]
        lines.append("\t".join(("C", country_ids[code], clean(row[4]), aliases(values))))
    missing_regions = set()
    for code in used_regions:
        row = regions.get(code)
        name = clean(row[1]) if row else ""
        # An absent administrative label stays absent: never invent a state name
        # from GeoNames' often numeric/FIPS administrative identifiers.
        values = [row[1], row[2], code, code.split(".", 1)[1]] if row else [code]
        if row is None:
            missing_regions.add(code)
        lines.append("\t".join(("R", region_ids[code], name, aliases(values))))
    for zone in sorted(zones):
        lines.append("\t".join(("T", zone_ids[zone], zone)))
    duplicate_count = 0
    for row in accepted:
        name = clean(row[1])
        display_region = display_regions.get(row[8] + "." + row[10], "")
        if duplicates[(name, row[8], display_region)] > 1:
            # Also distinguish duplicates when source administrative labels are
            # absent, without timezone deduplication or fabricated county names.
            name += f" (GeoNames {row[0]})"
            duplicate_count += 1
        values = [row[1], row[2], *row[3].split(",")]
        region = row[8] + "." + row[10]
        lines.append("\t".join(("P", name, region_ids[region], country_ids[row[8]], zone_ids[row[17]], aliases(values))))
    dataset = "\n".join(lines) + "\n"
    (output / "places.tsv").write_text(dataset, encoding="utf-8")
    details = [
        "GeoNames geographic search data attribution",
        "",
        "Contains modified GeoNames data: https://www.geonames.org/",
        "Licensed under Creative Commons Attribution 4.0 International (CC BY 4.0).",
        "License: https://creativecommons.org/licenses/by/4.0/",
        "GeoNames provides data as-is, without warranty of accuracy, timeliness or completeness.",
        f"Snapshot acquisition date (UTC): {snapshot_date}",
        f"Generator Unicode version: {unicodedata.unidata_version}",
        "",
        "Primary source inputs (original bytes, SHA-256):",
    ]
    for name in FILES:
        details.append(f"{BASE}{name}\n  {hashlib.sha256(contents[name]).hexdigest()}")
    details.extend([
        "",
        "Coverage: GeoNames cities5000 worldwide: population > 5,000 or PPLA",
        "(seat of a first-order administrative division), as defined by GeoNames.",
        f"Input city records: {source_count}; bundled city records: {len(accepted)}.",
        f"Countries/territories represented: {len(used_countries)}; regions: {len(used_regions)}; IANA timezones: {len(zones)}.",
        f"places.tsv: {len(dataset.encode('utf-8'))} bytes.",
        "Modifications: joined country/admin1 names and codes, retained all distinct",
        "normalized city alternate names (including ASCII transliterations),",
        "precomputed Unicode NFKD/lowercase accent-insensitive search keys, grouped",
        "shared metadata and sorted cities by population, normalized name and source ID.",
        f"Same-name/same-region records distinguished with GeoNames IDs: {duplicate_count}.",
        "Country search covers English source names, ISO2/ISO3 codes and country TLDs.",
        "Region search covers source names, ASCII names and GeoNames administrative codes;",
        "those codes are generally FIPS, not universally ISO state abbreviations.",
        f"Missing administrative names: {len(missing_regions)} unique country/admin1 codes; these show city and country only.",
        "Missing administrative codes: " + ", ".join(sorted(missing_regions)),
        "Skipped city records: " + (", ".join(f"{reason}: {count}" for reason, count in sorted(skipped.items())) or "none"),
        "Not every village or hamlet is covered. Alternate names may be historical.",
        "No online geocoding, user locations, or weather requests are used by city search.",
        "",
        "Reproduction: preserve the three input files with the hashes above; run",
        f"python3 resources/build-places.py --source-dir /path/to/snapshot --snapshot-date {snapshot_date}",
        "Python 3.9+ standard library and system IANA timezone data are required.",
        "Use the Unicode version above for byte-identical normalization output.",
        "Without --source-dir the generator downloads current public exports; these change daily.",
    ])
    (output / "places-source.txt").write_text("\n".join(details) + "\n", encoding="utf-8")
    print(f"Bundled {len(accepted)} cities, {len(used_countries)} countries/territories, {len(zones)} timezones; {len(dataset.encode('utf-8'))} TSV bytes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path)
    parser.add_argument("--snapshot-date", default=datetime.datetime.now(datetime.timezone.utc).date().isoformat())
    args = parser.parse_args()
    output = Path(__file__).resolve().parent
    if args.source_dir:
        generate(args.source_dir, output, args.snapshot_date)
    else:
        with tempfile.TemporaryDirectory(prefix="datetime-geonames-") as temporary:
            source = Path(temporary)
            for name in FILES:
                request = urllib.request.Request(BASE + name, headers={"User-Agent": "DatetimePlus-place-data-generator/1"})
                with urllib.request.urlopen(request, timeout=90) as response:
                    (source / name).write_bytes(response.read())
            generate(source, output, args.snapshot_date)


if __name__ == "__main__":
    main()
