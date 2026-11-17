mod ai;
mod tls;
mod dualserve;
mod users_http;
mod auth;
mod twostep;
mod account_http;
mod devices;
mod display;
mod backup;
mod lockdown;
mod macnet;
mod offsite;
mod timesync;
mod rtmp;
mod restream;
mod helpers;
mod links;
mod remote;
mod ai_setup;
mod ai_review;
mod faces;
mod faces_http;
mod lpr;
mod lpr_http;
mod digest;
mod digest_http;
mod smart;
mod follow;
mod rules_http;
mod bookmarks_http;
mod alert_rules;
mod sound;
mod gpu;
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
mod gpu_addon;
mod capture;
mod child;
mod ffmpeg_bin;
mod conns;
mod cli;
mod config;
mod db;
mod email;
mod engine;
mod live_http;
mod transcode;
mod local_source;
mod logbuf;
mod missing;
mod ingest;
mod hwdecode;
mod motion;
mod tracker;
mod tamper;
mod unusual;
mod mqtt;
mod onvif;
mod reolink;
mod cardfill;
mod camreboot;
mod dahua_login;
mod camreport;
mod proclimit;
mod onvif_ptz;
mod netinfo;
mod service;
mod setup;
mod stats;
mod telemetry;
mod talk;
mod ui_http;
mod update;
mod push;
mod push_http;
mod presence_http;
mod snooze;
mod web;
mod picture_text;
#[cfg(test)]
mod test_ports;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    service::init_logging();
    cli::run().await
}
#[cfg(test)]
mod device_list_tests;
#[cfg(test)]
mod display_tests;
