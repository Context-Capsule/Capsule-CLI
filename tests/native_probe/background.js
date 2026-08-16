const snapshot = {
  schema_version: 1,
  browser: "firefox",
  extension_version: "zen-ci",
  captured_at_unix_ms: Date.now(),
  skipped_private_windows: 0,
  windows: [
    {
      key: "zen-ci-window",
      focused: true,
      state: "normal",
      left: 0,
      top: 0,
      width: 1000,
      height: 700,
      tabs: [
        {
          index: 0,
          url: "https://zen-native-host-ci.example/",
          title: "Zen native messaging CI",
          pinned: false,
          active: true,
          discarded: false,
          muted: false,
          restorable: true
        }
      ],
      groups: []
    }
  ]
};

function connect() {
  try {
    const port = browser.runtime.connectNative("com.contextcapsule.host");
    port.onDisconnect.addListener(() => setTimeout(connect, 1000));
    port.postMessage({
      protocol_version: 1,
      request_id: "zen-ci-state",
      type: "browser.state.update",
      snapshot
    });
  } catch (_) {
    setTimeout(connect, 1000);
  }
}

connect();
