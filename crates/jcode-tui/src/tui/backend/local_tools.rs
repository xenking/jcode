//! Teardown of every local capability a session granted: screen context,
//! cloud authority and server-side tool configuration.
use super::*;

impl RemoteConnection {
    pub(super) fn revoke_cloud_authority(&mut self) {
        self.stop_screen_context();
        if let Some(live) = self.cloud_authority.take().and_then(|live| live.upgrade()) {
            live.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        for task in self.cloud_callbacks.drain(..) {
            task.abort();
        }
    }
    pub(super) async fn clear_local_tools(&mut self) -> Result<()> {
        self.stop_screen_context();
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.send_request(Request::ConfigureTools {
            id,
            tools: Default::default(),
        })
        .await?;
        self.local_tools_request = None;
        self.jarvis_tools_configured = false;
        Ok(())
    }
}
