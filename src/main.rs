use anyhow::Result;

use yt_niusic::audio::AudioEngine;
use yt_niusic::config::AppPaths;
use yt_niusic::tui::run_tui;
use yt_niusic::ytm::YtmClient;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let paths = AppPaths::resolve()?;
    let settings = paths.load_settings();
    let ytm = YtmClient::new(&paths.data_dir)?;
    let audio = AudioEngine::spawn(
        tokio::runtime::Handle::current(),
        settings.volume,
        settings.normalize_loudness,
    )?;

    run_tui(paths, settings, ytm, audio).await
}
