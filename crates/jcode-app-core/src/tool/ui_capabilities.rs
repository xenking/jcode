use super::*;

impl Registry {
    #[cfg(test)]
    pub(crate) async fn set_desktop_ui_enabled(&self, enabled: bool) {
        self.set_ui_capabilities(enabled, enabled).await;
    }

    pub(crate) async fn set_ui_capabilities(&self, user_response: bool, applets: bool) {
        if user_response {
            self.register(
                "user_response".into(),
                Arc::new(user_response::UserResponseTool),
            )
            .await;
        } else {
            self.unregister("user_response").await;
        }
        if applets {
            self.register("applet".into(), Arc::new(applet::AppletTool::new()))
                .await;
        } else {
            self.unregister("applet").await;
        }
    }
}
