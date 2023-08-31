use std::sync::{Arc, Weak};
use tokio::{
    sync::watch::{Receiver as WatchReceiver, Sender as WatchSender},
    time::{interval, sleep, Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::{config::CameraConfig, utils::connect_and_login, Result};
use neolink_core::bc_protocol::BcCamera;

pub(crate) struct NeoCamThread {
    config: WatchReceiver<CameraConfig>,
    cancel: CancellationToken,
    camera_watch: WatchSender<Weak<BcCamera>>,
}

impl NeoCamThread {
    pub(crate) async fn new(
        watch_config_rx: WatchReceiver<CameraConfig>,
        camera_watch_tx: WatchSender<Weak<BcCamera>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config: watch_config_rx,
            cancel,
            camera_watch: camera_watch_tx,
        }
    }
    async fn run_camera(&mut self, config: &CameraConfig) -> Result<()> {
        let camera = Arc::new(connect_and_login(config).await?);

        self.camera_watch.send_replace(Arc::downgrade(&camera));

        let cancel_check = self.cancel.clone();
        // Now we wait for a disconnect
        tokio::select! {
            _ = cancel_check.cancelled() => {
                Ok(())
            }
            v = camera.join() => v,
            v = async {
                let mut interval = interval(Duration::from_secs(5));
                loop {
                    interval.tick().await;
                    match camera.get_linktype().await {
                        Ok(_) => continue,
                        Err(neolink_core::Error::UnintelligibleReply { .. }) => {
                            // Camera does not support pings just wait forever
                            futures::pending!();
                        },
                        Err(e) => return Err(e),
                    }
                }
            } => v,
        }?;

        let _ = camera.logout().await;
        let _ = camera.shutdown().await;

        Ok(())
    }

    // Will run and attempt to maintain the connection
    //
    // A watch sender is used to send the new camera
    // whenever it changes
    pub(crate) async fn run(&mut self) -> Result<()> {
        const MAX_BACKOFF: Duration = Duration::from_secs(5);
        const MIN_BACKOFF: Duration = Duration::from_millis(50);

        let mut backoff = MIN_BACKOFF;

        loop {
            let mut config_rec = self.config.clone();

            let config = config_rec.borrow_and_update().clone();
            let now = Instant::now();

            let res = tokio::select! {
                Ok(_) = config_rec.changed() => {
                    None
                }
                v = self.run_camera(&config) => {
                    Some(v)
                }
            };
            self.camera_watch.send_replace(Weak::new());

            if res.is_none() {
                // If None go back and reload NOW
                //
                // This occurs if there was a config change
                continue;
            }

            // Else we see if the result actually was
            let result = res.unwrap();

            if Instant::now() - now > Duration::from_secs(60) {
                // Command ran long enough to be considered a success
                backoff = MIN_BACKOFF;
            }
            if backoff > MAX_BACKOFF {
                backoff = MAX_BACKOFF;
            }

            match result {
                Ok(()) => {
                    // Normal shutdown
                    log::debug!("Cancel:: NeoCamThread::NormalShutdown");
                    self.cancel.cancel();
                    return Ok(());
                }
                Err(e) => {
                    // An error
                    // Check if it is non-retry
                    let e_inner = e.downcast_ref::<neolink_core::Error>();
                    match e_inner {
                        Some(neolink_core::Error::CameraLoginFail) => {
                            // Fatal
                            log::error!("Login credentials were not accepted");
                            self.cancel.cancel();
                            return Err(e);
                        }
                        _ => {
                            // Non fatal
                            log::warn!("Connection Lost: {:?}", e);
                            log::info!("Attempt reconnect in {:?}", backoff);
                            sleep(backoff).await;
                            backoff *= 2;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for NeoCamThread {
    fn drop(&mut self) {
        log::debug!("Cancel:: NeoCamThread::drop");
        self.cancel.cancel();
    }
}