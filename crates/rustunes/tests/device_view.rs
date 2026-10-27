//! Device selection is a typed configuration value; construction performs no discovery.
use rustunes::engine::DeviceConfig;

#[test]
fn alternate_frontend_supplies_validated_device_and_local_socket_explicitly() {
    assert!(
        DeviceConfig::new(
            Some("00008140-EXAMPLE".into()),
            Some("/tmp/explicit-usbmuxd.sock".into())
        )
        .is_ok()
    );
    for value in ["", "device/name", "device\nname"] {
        assert!(DeviceConfig::new(Some(value.into()), None).is_err());
    }
    // Offline frontends retain the value; transport validates it only before connection.
    assert!(DeviceConfig::new(None, Some("relative.sock".into())).is_ok());
}
