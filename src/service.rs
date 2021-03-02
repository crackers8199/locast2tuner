use crate::{
    config::Config, credentials::LocastCredentials, fcc_facilities::FCCFacilities, utils::get,
};
use chrono::Utc;
use regex::Regex;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    convert::{From, TryFrom},
    fmt,
    str::FromStr,
    sync::{Arc, Mutex},
    thread, time,
};

static DMA_URL: &str = "https://api.locastnet.org/api/watch/dma";
static IP_URL: &str = "https://api.locastnet.org/api/watch/dma/ip";
static STATIONS_URL: &str = "https://api.locastnet.org/api/watch/epg";
static WATCH_URL: &str = "https://api.locastnet.org/api/watch/station";

#[derive(Debug)]
pub struct LocastService {
    config: Arc<Config>,
    credentials: Arc<LocastCredentials>,
    fcc_facilities: Arc<FCCFacilities>,
    zipcode: Option<String>,
    pub geo: Arc<Geo>,
    pub uuid: String,
    stations: Stations,
}

impl LocastService {
    pub fn new(
        config: Arc<Config>,
        credentials: Arc<LocastCredentials>,
        fcc_facilities: Arc<FCCFacilities>,
        zipcode: Option<String>,
    ) -> LocastServiceArc {
        let geo = Arc::new(Geo::from(&zipcode));
        if !geo.active {
            panic!(format!("{} not active", geo.name))
        }

        let uuid = uuid::Uuid::new_v5(
            &uuid::Uuid::from_str(&config.uuid).unwrap(),
            geo.DMA.as_bytes(),
        )
        .to_string();

        let stations = Arc::new(Mutex::new(build_stations(
            locast_stations(&geo.DMA, config.days, &credentials.token()),
            &geo,
            &config,
            &fcc_facilities,
        )));

        start_updater_thread(&config, &stations, &geo, &credentials, &fcc_facilities);

        let service = Arc::new(LocastService {
            config,
            credentials,
            fcc_facilities,
            zipcode,
            geo,
            uuid,
            stations,
        });

        service
    }

    // Convenience method for building stations based on &self
    fn build_stations(&self) -> Vec<Station> {
        let locast_stations =
            locast_stations(&self.geo.DMA, self.config.days, &self.credentials.token());
        build_stations(
            locast_stations,
            &self.geo,
            &self.config,
            &self.fcc_facilities,
        )
    }
}

pub type LocastServiceArc = Arc<LocastService>;

impl StationProvider for LocastServiceArc {
    fn stations(&self) -> Stations {
        if self.config.disable_station_cache {
            Arc::new(Mutex::new(self.build_stations()))
        } else {
            self.stations.clone()
        }
    }

    fn station_stream_uri(&self, id: &str) -> String {
        let url = format!(
            "{}/{}/{}/{}",
            WATCH_URL, id, self.geo.latitude, self.geo.longitude
        );

        let response: WatchResponse = get(&url, Some(&self.credentials.token())).json().unwrap();
        println!("{:?}", response);
        let m3u_data = get(&response.streamUrl, None).text().unwrap();
        let master_playlist = hls_m3u8::MasterPlaylist::try_from(m3u_data.as_str());

        // Make this nicer with a match
        if master_playlist.is_err() {
            response.streamUrl
        } else {
            let mut vs = master_playlist.unwrap().variant_streams;
            vs.sort_by(|a, b| {
                let ea = match a {
                    hls_m3u8::tags::VariantStream::ExtXStreamInf { stream_data, .. } => {
                        stream_data.bandwidth()
                    }
                    _ => 0,
                };
                let eb = match b {
                    hls_m3u8::tags::VariantStream::ExtXStreamInf { stream_data, .. } => {
                        stream_data.bandwidth()
                    }
                    _ => 0,
                };
                ea.cmp(&eb)
            });
            let variant = vs.pop().unwrap();

            let stream_url = match variant {
                hls_m3u8::tags::VariantStream::ExtXStreamInf { uri, .. } => uri,
                _ => Cow::Borrowed(""),
            };

            let full_url = Url::parse(&response.streamUrl)
                .unwrap()
                .join(&stream_url.to_string())
                .unwrap()
                .to_string();

            full_url.to_string()
        }
    }

    fn geo(&self) -> Arc<Geo> {
        self.geo.clone()
    }

    fn uuid(&self) -> String {
        self.uuid.to_owned()
    }
}

pub trait StationProvider {
    fn station_stream_uri(&self, id: &str) -> String;
    fn stations(&self) -> Stations;
    fn geo(&self) -> Arc<Geo>;
    fn uuid(&self) -> String;
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug)]
struct WatchResponse {
    streamUrl: String,
}

impl fmt::Display for LocastService {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "LocastService{{ zipcode: {:?}, uuid: {:?}, geo: {:?} }}",
            self.zipcode, self.uuid, self.geo
        )
    }
}

fn start_updater_thread(
    config: &Arc<Config>,
    stations: &Stations,
    geo: &Arc<Geo>,
    credentials: &Arc<LocastCredentials>,
    fcc_facilities: &Arc<FCCFacilities>,
) {
    // Can this be done nicer?
    let thread_stations = stations.clone();
    let thread_config = config.clone();
    let thread_geo = geo.clone();
    let thread_credentials = credentials.clone();
    let thread_facilities = fcc_facilities.clone();
    let thread_timeout = config.cache_timeout.clone();

    thread::spawn(move || loop {
        thread::sleep(time::Duration::from_secs(thread_timeout));
        let ls = locast_stations(
            &thread_geo.DMA,
            thread_config.days,
            &thread_credentials.token(),
        );
        let new_stations = build_stations(ls, &thread_geo, &thread_config, &thread_facilities);
        let mut stations = thread_stations.lock().unwrap();
        *stations = new_stations;
    });
}

// Build stations
fn build_stations(
    locast_stations: Vec<Station>,
    geo: &Geo,
    config: &Arc<Config>,
    fcc_facilities: &Arc<FCCFacilities>,
) -> Vec<Station> {
    println!(
        "Loading stations for {} (cache: {}, cache timeout: {}, days: {}",
        geo.name, config.disable_station_cache, config.cache_timeout, config.days
    );

    let mut stations: Vec<Station> = Vec::new();

    for mut station in locast_stations.into_iter() {
        station.timezone = geo.timezone.to_owned();
        station.city = Some(geo.name.to_owned());

        let channel_from_call_sign = match Regex::new(r"(\d+\.\d+) .+")
            .unwrap()
            .captures(&station.callSign)
        {
            Some(c) => Some(c.get(1).map_or("", |m| m.as_str())),
            None => None,
        };

        let c = if let Some(channel) = channel_from_call_sign {
            Some(channel.to_string())
        } else if let Some((call_sign, sub_channel)) =
            detect_callsign(&station.name).or(detect_callsign(&station.callSign))
        {
            let dma = geo.DMA.parse::<i64>().unwrap();
            let channel = fcc_facilities.lookup(dma, call_sign, sub_channel);

            Some(channel)
        } else {
            panic!(format!(
                "Channel {}, call sign: {} not found!",
                &station.name, &station.callSign
            ));
        };
        station.channel = c;
        stations.push(station);
    }
    stations
}

fn locast_stations(dma: &str, days: u8, token: &str) -> Vec<Station> {
    let start_time = Utc::now().format("%Y-%m-%dT00:00:00-00:00").to_string();
    let uri = format!(
        "{}/{}?startTime={}&hours={}",
        STATIONS_URL,
        dma,
        start_time,
        days * 24
    );
    crate::utils::get(&uri, Some(token))
        .json::<Vec<Station>>()
        .unwrap()
}

fn detect_callsign(input: &str) -> Option<(&str, &str)> {
    let re = Regex::new(r"^([KW][A-Z]{2,3})[A-Z]{0,2}(\d{0,2})$").unwrap();
    let caps = re.captures(input)?;
    let call_sign = caps.get(1).map_or("default", |m| m.as_str());
    let sub_channel = caps.get(2).map_or("default", |m| m.as_str());
    Some((call_sign, sub_channel))
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug)]
pub struct Geo {
    pub latitude: f64,
    pub longitude: f64,
    pub DMA: String,
    pub name: String,
    pub active: bool,
    pub timezone: Option<String>,
}

impl From<&Option<String>> for Geo {
    fn from(zipcode: &Option<String>) -> Self {
        let uri = match zipcode {
            Some(z) => format!("{}/zip/{}", DMA_URL, z),
            None => String::from(IP_URL),
        };

        let mut geo = crate::utils::get(&uri, None).json::<Geo>().unwrap();
        geo.timezone = Some(tz_search::lookup(geo.latitude, geo.longitude).unwrap());
        geo
    }
}

#[allow(non_snake_case)]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Station {
    pub active: bool,
    pub callSign: String,
    pub channel: Option<String>,
    pub city: Option<String>,
    pub dma: i64,
    pub id: i64,
    pub listings: Vec<Listing>,
    pub logo226Url: String,
    pub logoUrl: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    pub stationId: String,
    pub timezone: Option<String>,
    pub tivoId: Option<i64>,
    pub transcodeId: i64,
}
pub type Stations = Arc<Mutex<Vec<Station>>>;

#[allow(non_snake_case)]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Listing {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airdate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audioProperties: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directors: Option<String>,
    pub duration: i64,
    pub entityType: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episodeNumber: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episodeTitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genres: Option<String>,
    pub hasImageArtwork: bool,
    pub hasSeriesArtwork: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isNew: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferredImage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferredImageHeight: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferredImageWidth: Option<i16>,
    pub programId: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub releaseDate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub releaseYear: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seasonNumber: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seriesId: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortDescription: Option<String>,
    pub showType: String,
    pub startTime: i64,
    pub stationId: i64,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topCast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub videoProperties: Option<String>,
}
