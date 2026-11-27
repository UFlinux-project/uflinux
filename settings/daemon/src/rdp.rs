//! Remote Desktop panel support.
//!
//! Talks to the RDS system-bus helper. The daemon deliberately
//! does not link the rds workspace, so the wire shapes are mirrored here.
//! The mirrors are permissive: unknown fields are ignored and every field has
//! a fallback, so a newer or older broker never breaks the UI.

use glib::variant::ToVariant;
use serde::Deserialize;
use serde_json::{Value, json};
use settings_core::{
    CertificateMode, CodecTier, MAX_RDP_SESSIONS, Rdp, RdpCodecs, RdpSession, RdpSystemAction,
    Result, bus,
};
use std::path::PathBuf;
#[cfg(test)]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

/// On-disk broker configuration; it is patched through the broker, never written here.
const CONFIG_PATH: &str = "/etc/uflinux/rds.json";
/// Presence of the package. Nothing is probed while the binary is missing.
const BROKER_BINARY: &str = "/usr/bin/uflinux-rdsd";
/// Privileged helper shipped by the rds package, not the Settings system service.
const RDS_SYSTEM_NAME: &str = "org.uflinux.Rds.System1";
const RDS_SYSTEM_PATH: &str = "/org/uflinux/Rds/System1";
/// The page must never wait on the broker: connect, write and read all stop here.
#[cfg(test)]
const TIMEOUT: Duration = Duration::from_millis(300);
#[cfg(test)]
const MAX_REPLY: usize = 1024 * 1024;

fn config_path() -> PathBuf {
    PathBuf::from(CONFIG_PATH)
}

fn broker_installed() -> bool {
    std::fs::symlink_metadata(BROKER_BINARY).is_ok()
}

// ---------------------------------------------------------------- state query

/// Reads the broker through the distro's system-bus helper.
pub async fn query(system: &gio::DBusConnection) -> Rdp {
    if !broker_installed() {
        return Rdp::default();
    }
    match helper(system, &json!({"operation": "broker-state"})).await {
        Ok(reply) => match serde_json::from_value::<BrokerState>(reply["state"].clone()) {
            Ok(state) => state_to_rdp(state),
            Err(_) => unavailable("The broker reported an unreadable state".into()),
        },
        Err(error) => unavailable(error),
    }
}

fn unavailable(error: String) -> Rdp {
    Rdp {
        installed: true,
        enabled: false,
        sessions: Vec::new(),
        error: Some(error),
        ..Default::default()
    }
}

/// One request, one reply, one line each way. Errors are user-facing strings.
#[cfg(test)]
fn exchange_at(path: &Path, request: &Value) -> Result<Value> {
    let stream = UnixStream::connect(path)
        .map_err(|_| format!("The RDS broker is not reachable at {}", path.display()))?;
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let mut encoded = request.to_string().into_bytes();
    encoded.push(b'\n');
    (&stream)
        .write_all(&encoded)
        .map_err(|_| "The RDS broker is not reachable".to_owned())?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|_| "The RDS broker did not answer".to_owned())?;
    if line.len() > MAX_REPLY {
        return Err("The broker reply is too large".into());
    }
    let reply: Value =
        serde_json::from_str(&line).map_err(|_| "The broker sent an unreadable reply")?;
    match reply.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(reply),
        _ => Err(reply
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("The broker refused the request")
            .to_owned()),
    }
}

/// Apply a Settings update or disconnect through the authorized helper.
pub async fn execute(system: &gio::DBusConnection, request: Value) -> Result<Value> {
    let operation = match request["op"].as_str() {
        Some("execute") => {
            json!({"operation": "broker-configure", "config": request["action"]["config"]})
        }
        Some("disconnect") => {
            json!({"operation": "broker-disconnect", "session": request["session"]})
        }
        _ => return Err("Unsupported broker operation".to_owned()),
    };
    helper(system, &operation).await
}

// ------------------------------------------------------------- privileged bus

/// Forwards an enable/disable/start/stop/apply-certificate verb to
/// `org.uflinux.Rds.System1`. `ApplyCertificate` needs the currently
/// configured mode, so the caller passes the published block.
pub async fn system(
    system: &gio::DBusConnection,
    operation: &RdpSystemAction,
    current: &Rdp,
) -> Result<Value> {
    helper(system, &helper_request(operation, current)).await
}

async fn helper(system: &gio::DBusConnection, request: &Value) -> Result<Value> {
    let request = serde_json::to_string(request).map_err(|_| "Cannot encode the rds operation")?;
    let reply = bus::call(
        system,
        RDS_SYSTEM_NAME,
        RDS_SYSTEM_PATH,
        RDS_SYSTEM_NAME,
        "Execute",
        Some(&(request,).to_variant()),
    )
    .await?;
    let raw = reply
        .get::<(String,)>()
        .ok_or("Invalid rds system response")?
        .0;
    serde_json::from_str(&raw).map_err(|_| "Invalid rds system response".into())
}

fn helper_request(operation: &RdpSystemAction, current: &Rdp) -> Value {
    match operation {
        RdpSystemAction::Enable {} => json!({"operation": "enable"}),
        RdpSystemAction::Disable {} => json!({"operation": "disable"}),
        RdpSystemAction::Start {} => json!({"operation": "start"}),
        RdpSystemAction::Stop {} => json!({"operation": "stop"}),
        RdpSystemAction::ApplyCertificate {} => match current.certificate_mode {
            CertificateMode::SelfSigned => {
                json!({"operation": "apply-certificate", "mode": "self-signed"})
            }
            CertificateMode::Custom => {
                let mut request = json!({"operation": "apply-certificate", "mode": "custom"});
                request["certificate_path"] = current
                    .custom_certificate_path
                    .as_deref()
                    .map_or(Value::Null, |path| json!(path));
                request["key_path"] = current
                    .custom_key_path
                    .as_deref()
                    .map_or(Value::Null, |path| json!(path));
                request
            }
        },
    }
}

// -------------------------------------------------------------- mutations
//
// The on-disk configuration is read as plain JSON so unknown keys survive,
// only the relevant fields are patched, and the broker is the only writer.

/// Current on-disk configuration as free-form JSON; defaults to an empty
/// object when the file is absent or unreadable so the patch still applies.
pub fn disk_config() -> Value {
    std::fs::read(config_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn execute_request(config: Value) -> Value {
    json!({"op": "execute", "action": {"kind": "apply_config", "config": config}})
}

fn section<'a>(config: &'a mut Value, key: &str) -> &'a mut Value {
    if !config[key].is_object() {
        config[key] = json!({});
    }
    &mut config[key]
}

fn flag(config: &Value, section: &str, key: &str, fallback: bool) -> bool {
    config[section][key].as_bool().unwrap_or(fallback)
}

fn check_port(port: u16) -> Result<()> {
    if port == 0 {
        Err("A port between 1 and 65535 is required".into())
    } else {
        Ok(())
    }
}

/// HTTPS makes the web gateway and the RDP listener share one port number
/// space, so they must differ while the secure listener is on.
fn check_port_pair(config: &Value, rdp_port: u16, web_port: u16) -> Result<()> {
    if flag(config, "web", "enabled", true)
        && flag(config, "web", "https", true)
        && rdp_port == web_port
    {
        return Err("The RDP and web ports must differ while HTTPS is on".into());
    }
    Ok(())
}

fn rdp_port_of(config: &Value) -> u16 {
    config["rdp_port"].as_u64().unwrap_or(3389) as u16
}

pub fn ports_request(config: Value, rdp_port: u16, web_port: u16) -> Result<Value> {
    check_port(rdp_port)?;
    check_port(web_port)?;
    let mut config = config;
    check_port_pair(&config, rdp_port, web_port)?;
    config["rdp_port"] = json!(rdp_port);
    section(&mut config, "web")["port"] = json!(web_port);
    Ok(execute_request(config))
}

pub fn web_request(config: Value, enabled: bool, port: u16) -> Result<Value> {
    check_port(port)?;
    let mut config = config;
    check_port_pair(&config, rdp_port_of(&config), port)?;
    let web = section(&mut config, "web");
    web["enabled"] = json!(enabled);
    web["port"] = json!(port);
    Ok(execute_request(config))
}

pub fn https_request(config: Value, enabled: bool) -> Result<Value> {
    let mut config = config;
    section(&mut config, "web")["https"] = json!(enabled);
    Ok(execute_request(config))
}

pub fn certificate_request(
    config: Value,
    mode: CertificateMode,
    certificate_path: Option<&str>,
    key_path: Option<&str>,
) -> Result<Value> {
    let certificate = match mode {
        CertificateMode::SelfSigned => json!({"source": "self_signed"}),
        CertificateMode::Custom => {
            let certificate = certificate_path
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or("A certificate file is required for a custom certificate")?;
            let key = key_path
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or("A private key file is required for a custom certificate")?;
            json!({"source": "custom", "certificate": certificate, "private_key": key})
        }
    };
    let mut config = config;
    section(&mut config, "web")["certificate"] = certificate;
    Ok(execute_request(config))
}

/// The four-way `h264` policy in `/etc/uflinux/rds.json` carries both the
/// "use H.264 at all" toggle and the hardware preference, so the two switches
/// on the page are merged here and split again by `accel_from_policy`.
pub fn codecs_request(
    config: Value,
    allow_h264: bool,
    allow_remotefx: bool,
    hardware_acceleration: settings_core::HardwareAccel,
) -> Result<Value> {
    use settings_core::HardwareAccel;
    let h264 = match (allow_h264, hardware_acceleration) {
        (false, _) => "off",
        (true, HardwareAccel::Always) => "force",
        (true, HardwareAccel::Off) => "software",
        (true, HardwareAccel::Auto) => "auto",
    };
    let mut config = config;
    let codec = section(&mut config, "codec");
    codec["h264"] = json!(h264);
    codec["allow_remotefx"] = json!(allow_remotefx);
    Ok(execute_request(config))
}

pub fn max_sessions_request(config: Value, max_sessions: u16) -> Result<Value> {
    if max_sessions == 0 || max_sessions > MAX_RDP_SESSIONS {
        return Err(format!(
            "The session limit must be between 1 and {MAX_RDP_SESSIONS}"
        ));
    }
    let mut config = config;
    config["max_sessions"] = json!(max_sessions);
    Ok(execute_request(config))
}

pub fn disconnect_request(session_id: &str) -> Result<Value> {
    let session: u32 = session_id
        .trim()
        .parse()
        .map_err(|_| "Unknown session identifier".to_owned())?;
    Ok(json!({"op": "disconnect", "session": session}))
}

// ------------------------------------------------------------ wire mirrors

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BrokerState {
    config: Option<Config>,
    effective: Option<Config>,
    enabled: bool,
    running: bool,
    sessions: Vec<SessionInfo>,
    fault: Option<String>,
}

/// The subset of `/etc/uflinux/rds.json` the panel displays. Defaults match
/// `uflinux_rds_common::Config`.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    rdp_port: u16,
    max_sessions: usize,
    codec: Codec,
    web: Web,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            rdp_port: 3389,
            max_sessions: 4,
            codec: Codec::default(),
            web: Web::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Codec {
    h264: String,
    allow_remotefx: bool,
    vaapi_device: Option<String>,
}

impl Default for Codec {
    fn default() -> Self {
        Self {
            h264: "auto".into(),
            allow_remotefx: true,
            vaapi_device: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Web {
    enabled: bool,
    port: u16,
    https: bool,
    certificate: Certificate,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 3390,
            https: true,
            certificate: Certificate::default(),
        }
    }
}

/// Internally tagged in the broker (`{"source":"custom", ...}`); read flat.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct Certificate {
    source: String,
    certificate: Option<String>,
    private_key: Option<String>,
}

impl Default for Certificate {
    fn default() -> Self {
        Self {
            source: "self_signed".into(),
            certificate: None,
            private_key: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SessionInfo {
    id: u32,
    username: String,
    transport: String,
    codec: String,
    codec_device: Option<String>,
    connected_seconds: u64,
}

// ----------------------------------------------------------------- mapping

/// Prefers `effective` (defaults and environment overrides already applied)
/// and falls back to the stored `config`.
fn state_to_rdp(state: BrokerState) -> Rdp {
    let config = state.effective.or(state.config).unwrap_or_default();
    let custom = config.web.certificate.source == "custom";
    Rdp {
        installed: true,
        enabled: state.enabled || state.running,
        rdp_port: config.rdp_port,
        web_enabled: config.web.enabled,
        web_port: config.web.port,
        https: config.web.https,
        certificate_mode: if custom {
            CertificateMode::Custom
        } else {
            CertificateMode::SelfSigned
        },
        custom_certificate_path: custom
            .then_some(config.web.certificate.certificate)
            .flatten(),
        custom_key_path: custom
            .then_some(config.web.certificate.private_key)
            .flatten(),
        max_sessions: config.max_sessions.min(u16::MAX as usize) as u16,
        codecs: RdpCodecs {
            allow_h264: config.codec.h264 != "off",
            allow_remotefx: config.codec.allow_remotefx,
            hardware_acceleration: accel_from_policy(&config.codec.h264),
            detected_device: config.codec.vaapi_device,
        },
        sessions: state.sessions.into_iter().map(session_from_info).collect(),
        error: state.fault,
    }
}

fn session_from_info(info: SessionInfo) -> RdpSession {
    RdpSession {
        id: info.id.to_string(),
        username: info.username,
        client_address: String::new(),
        connected_since_secs: info.connected_seconds,
        codec: codec_tier(&info.codec),
        codec_detail: info.codec_device,
        web: info.transport == "web",
    }
}

fn codec_tier(tag: &str) -> CodecTier {
    match tag
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-")
        .replace("remotefx", "remote-fx")
        .as_str()
    {
        "h264-vaapi" | "vaapi" => CodecTier::H264Vaapi,
        "h264" | "h264-software" | "openh264" => CodecTier::H264Software,
        "rfx" | "remote-fx" => CodecTier::RemoteFx,
        "qoi-z" | "qoiz" => CodecTier::QoiZ,
        _ => CodecTier::Qoi,
    }
}

/// Mirrors `uflinux_rds_common::H264Policy::parse`.
fn accel_from_policy(policy: &str) -> settings_core::HardwareAccel {
    use settings_core::HardwareAccel;
    match policy.trim().to_ascii_lowercase().as_str() {
        "force" | "on" | "1" | "true" => HardwareAccel::Always,
        "software" | "soft" | "off" | "0" | "false" => HardwareAccel::Off,
        _ => HardwareAccel::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = r#"{
        "config": {"enabled": true, "rdp_port": 3391, "max_sessions": 8,
                   "codec": {"h264": "force", "allow_remotefx": false, "vaapi_device": "/dev/dri/renderD128"},
                   "web": {"enabled": true, "port": 3392, "https": false,
                           "certificate": {"source": "custom", "certificate": "/etc/tls/c.pem", "private_key": "/etc/tls/k.pem"}}},
        "effective": {"enabled": true, "rdp_port": 3391, "max_sessions": 8,
                      "codec": {"h264": "force", "allow_remotefx": false, "vaapi_device": "/dev/dri/renderD128"},
                      "web": {"enabled": true, "port": 3392, "https": false,
                              "certificate": {"source": "custom", "certificate": "/etc/tls/c.pem", "private_key": "/etc/tls/k.pem"}}},
        "enabled": true, "running": true, "rdp_listening": true, "web_listening": true,
        "sessions": [
            {"id": 7, "username": "root", "transport": "web", "codec": "qoi-z", "width": 800, "height": 600, "connected_seconds": 42, "idle_seconds": 3},
            {"id": 9, "username": "user", "transport": "rdp", "codec": "h264-vaapi", "codec_device": "Intel iHD", "width": 1920, "height": 1080, "connected_seconds": 5, "idle_seconds": 0, "future_field": 1}
        ],
        "fault": null, "brand_new_field": {"ignored": true}
    }"#;

    fn state() -> Value {
        serde_json::from_str(STATE).unwrap()
    }

    fn mapped() -> Rdp {
        state_to_rdp(serde_json::from_value(state()).unwrap())
    }

    #[test]
    fn a_broker_state_maps_into_the_published_block() {
        let rdp = mapped();
        assert!(rdp.installed);
        assert!(rdp.enabled);
        assert_eq!(rdp.rdp_port, 3391);
        assert!(rdp.web_enabled);
        assert_eq!(rdp.web_port, 3392);
        assert!(!rdp.https);
        assert_eq!(rdp.certificate_mode, CertificateMode::Custom);
        assert_eq!(
            rdp.custom_certificate_path.as_deref(),
            Some("/etc/tls/c.pem")
        );
        assert_eq!(rdp.max_sessions, 8);
        assert!(rdp.codecs.allow_h264);
        assert!(!rdp.codecs.allow_remotefx);
        assert_eq!(
            rdp.codecs.hardware_acceleration,
            settings_core::HardwareAccel::Always
        );
        assert_eq!(
            rdp.codecs.detected_device.as_deref(),
            Some("/dev/dri/renderD128")
        );
        assert_eq!(rdp.sessions.len(), 2);
        assert_eq!(rdp.sessions[0].id, "7");
        assert!(rdp.sessions[0].web);
        assert_eq!(rdp.sessions[0].codec, CodecTier::QoiZ);
        assert_eq!(rdp.sessions[0].connected_since_secs, 42);
        assert!(!rdp.sessions[1].web);
        assert_eq!(rdp.sessions[1].codec, CodecTier::H264Vaapi);
        assert_eq!(rdp.sessions[1].codec_detail.as_deref(), Some("Intel iHD"));
        assert!(rdp.error.is_none());
    }

    #[test]
    fn unknown_and_missing_fields_fall_back_to_defaults() {
        let state: BrokerState =
            serde_json::from_str(r#"{"running": true, "nope": [1,2]}"#).unwrap();
        let rdp = state_to_rdp(state);
        assert!(rdp.installed);
        assert!(rdp.enabled, "running alone counts as enabled");
        assert_eq!(rdp.rdp_port, 3389);
        assert_eq!(rdp.web_port, 3390);
        assert!(rdp.web_enabled, "the broker default serves the web client");
        assert!(rdp.https);
        assert_eq!(rdp.certificate_mode, CertificateMode::SelfSigned);
        assert_eq!(rdp.max_sessions, 4);
        assert!(rdp.codecs.allow_h264);
        assert_eq!(
            rdp.codecs.hardware_acceleration,
            settings_core::HardwareAccel::Auto
        );
        assert!(rdp.sessions.is_empty());
        // A state without `effective` still reads the stored config.
        let state: BrokerState = serde_json::from_str(r#"{"config": {"rdp_port": 3399}}"#).unwrap();
        assert_eq!(state_to_rdp(state).rdp_port, 3399);
    }

    #[test]
    fn codec_tags_map_onto_the_documented_tiers() {
        assert_eq!(codec_tier("h264-vaapi"), CodecTier::H264Vaapi);
        assert_eq!(codec_tier("H264"), CodecTier::H264Software);
        assert_eq!(codec_tier("h264_software"), CodecTier::H264Software);
        assert_eq!(codec_tier("rfx"), CodecTier::RemoteFx);
        assert_eq!(codec_tier("RemoteFX"), CodecTier::RemoteFx);
        assert_eq!(codec_tier("qoiz"), CodecTier::QoiZ);
        assert_eq!(codec_tier("qoi"), CodecTier::Qoi);
        assert_eq!(codec_tier("anything-new"), CodecTier::Qoi);
    }

    #[test]
    fn a_missing_socket_is_reported_without_panicking() {
        let error = exchange_at(Path::new("/nonexistent/control.sock"), &state())
            .err()
            .unwrap();
        assert!(error.contains("not reachable"), "{error}");
    }

    #[test]
    fn each_mutation_builds_the_documented_request() {
        // A config that already carries an unknown key, to prove the patch keeps it.
        let base = json!({"future_key": {"kept": true}, "rdp_port": 3389});

        let request = ports_request(base.clone(), 3391, 3392).unwrap();
        assert_eq!(request["op"], "execute");
        assert_eq!(request["action"]["kind"], "apply_config");
        assert_eq!(request["action"]["config"]["rdp_port"], 3391);
        assert_eq!(request["action"]["config"]["web"]["port"], 3392);
        assert_eq!(request["action"]["config"]["future_key"]["kept"], true);

        let request = web_request(base.clone(), false, 3390).unwrap();
        assert_eq!(request["action"]["config"]["web"]["enabled"], false);
        assert_eq!(request["action"]["config"]["web"]["port"], 3390);

        let request = https_request(base.clone(), false).unwrap();
        assert_eq!(request["action"]["config"]["web"]["https"], false);

        let request = certificate_request(
            base.clone(),
            CertificateMode::Custom,
            Some("/c.pem"),
            Some("/k.pem"),
        )
        .unwrap();
        assert_eq!(
            request["action"]["config"]["web"]["certificate"]["source"],
            "custom"
        );
        assert_eq!(
            request["action"]["config"]["web"]["certificate"]["certificate"],
            "/c.pem"
        );
        let request =
            certificate_request(base.clone(), CertificateMode::SelfSigned, None, None).unwrap();
        assert_eq!(
            request["action"]["config"]["web"]["certificate"]["source"],
            "self_signed"
        );

        let request = codecs_request(
            base.clone(),
            true,
            false,
            settings_core::HardwareAccel::Always,
        )
        .unwrap();
        assert_eq!(request["action"]["config"]["codec"]["h264"], "force");
        assert_eq!(
            request["action"]["config"]["codec"]["allow_remotefx"],
            false
        );
        let request = codecs_request(
            base.clone(),
            false,
            true,
            settings_core::HardwareAccel::Auto,
        )
        .unwrap();
        assert_eq!(request["action"]["config"]["codec"]["h264"], "off");

        // "Software only" must survive a write/read round trip, not collapse
        // into "auto".
        let request =
            codecs_request(base.clone(), true, true, settings_core::HardwareAccel::Off).unwrap();
        assert_eq!(request["action"]["config"]["codec"]["h264"], "software");
        let written: Config = serde_json::from_value(request["action"]["config"].clone()).unwrap();
        let round_trip = state_to_rdp(BrokerState {
            effective: Some(written),
            ..BrokerState::default()
        });
        assert!(round_trip.codecs.allow_h264);
        assert_eq!(
            round_trip.codecs.hardware_acceleration,
            settings_core::HardwareAccel::Off
        );

        let request = max_sessions_request(base.clone(), 8).unwrap();
        assert_eq!(request["action"]["config"]["max_sessions"], 8);

        let request = disconnect_request("7").unwrap();
        assert_eq!(request, json!({"op": "disconnect", "session": 7}));

        assert!(ports_request(base.clone(), 0, 3390).is_err());
        assert!(
            ports_request(base.clone(), 3390, 3390).is_err(),
            "https needs distinct ports"
        );
        assert!(max_sessions_request(base.clone(), 0).is_err());
        assert!(max_sessions_request(base.clone(), MAX_RDP_SESSIONS + 1).is_err());
        assert!(disconnect_request("seven").is_err());
        assert!(
            certificate_request(base, CertificateMode::Custom, Some("/c.pem"), None).is_err(),
            "a custom certificate needs both files"
        );
    }

    #[test]
    fn an_absent_config_file_reads_as_an_empty_object() {
        // On a dev host /etc/uflinux/rds.json is usually missing; the patch
        // must still build a valid request from defaults.
        let config = disk_config();
        assert!(config.is_object());
        let request = max_sessions_request(config, 4).unwrap();
        assert_eq!(request["action"]["config"]["max_sessions"], 4);
    }

    #[test]
    fn the_helper_request_carries_the_configured_certificate() {
        let current = Rdp {
            certificate_mode: CertificateMode::Custom,
            custom_certificate_path: Some("/c.pem".into()),
            custom_key_path: Some("/k.pem".into()),
            ..Default::default()
        };
        let request = helper_request(&RdpSystemAction::ApplyCertificate {}, &current);
        assert_eq!(request["operation"], "apply-certificate");
        assert_eq!(request["mode"], "custom");
        assert_eq!(request["certificate_path"], "/c.pem");
        assert_eq!(
            helper_request(&RdpSystemAction::Enable {}, &current),
            json!({"operation": "enable"})
        );
        let request = helper_request(&RdpSystemAction::ApplyCertificate {}, &Rdp::default());
        assert_eq!(request["mode"], "self-signed");
        assert!(request.get("certificate_path").is_none());
    }

    /// End to end over a real socket: a fake broker answers one line each way.
    #[test]
    fn a_live_socket_round_trips_one_line_each_way() {
        let dir = std::env::temp_dir().join(format!("uflinux-rds-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("control.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let expected = json!({"op": "get_state"}).to_string();
        let reply = format!("{}\n", json!({"status": "ok", "state": state()}));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            assert_eq!(line.trim(), expected);
            (&stream).write_all(reply.as_bytes()).unwrap();
        });
        let reply = exchange_at(&socket, &json!({"op": "get_state"})).unwrap();
        let rdp = state_to_rdp(serde_json::from_value(reply["state"].clone()).unwrap());
        assert_eq!(rdp.rdp_port, 3391);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
