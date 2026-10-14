//! Minimal SOAP handling for the ONVIF PTZ bridge
//!
//! Only the handful of operations that NVR clients (Frigate in particular) need
//! for pan/tilt and presets are understood. Requests are scanned for the few
//! values we care about instead of being fully deserialised.
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// Values extracted from an incoming SOAP request
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SoapRequest {
    /// Local name of the first element inside `Body`, e.g. `ContinuousMove`
    pub(crate) operation: String,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    /// True when the password is a WS-Security digest rather than plain text
    pub(crate) password_is_digest: bool,
    pub(crate) nonce: Option<String>,
    pub(crate) created: Option<String>,
    /// Velocity or translation pan/tilt, if any
    pub(crate) pan_tilt: Option<(f32, f32)>,
    pub(crate) zoom: Option<f32>,
    pub(crate) preset_token: Option<String>,
    pub(crate) timeout: Option<String>,
}

pub(crate) fn parse_request(body: &str) -> Result<SoapRequest, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut req = SoapRequest::default();
    let mut path: Vec<String> = vec![];
    let mut in_body = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("Invalid XML at {}: {e}", reader.error_position()))?;
        match event {
            Event::Start(e) => {
                let name = on_element(&mut req, &e, in_body);
                in_body |= name == "Body";
                path.push(name);
            }
            Event::Empty(e) => {
                on_element(&mut req, &e, in_body);
            }
            Event::End(_) => {
                path.pop();
            }
            Event::Text(t) => {
                let text = t.decode().map_err(|e| e.to_string())?;
                append_text(&mut req, path.last().map(|s| s.as_str()), &text);
            }
            Event::GeneralRef(r) => {
                let entity: &[u8] = r.as_ref();
                let text = match entity {
                    b"amp" => "&",
                    b"lt" => "<",
                    b"gt" => ">",
                    b"quot" => "\"",
                    b"apos" => "'",
                    _ => "",
                };
                append_text(&mut req, path.last().map(|s| s.as_str()), text);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if req.operation.is_empty() {
        return Err("No operation in SOAP body".to_string());
    }
    Ok(req)
}

/// Record what we need from an opening tag and return its local name
fn on_element(req: &mut SoapRequest, e: &BytesStart, in_body: bool) -> String {
    let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
    if in_body && req.operation.is_empty() {
        req.operation = name.clone();
    }
    match name.as_str() {
        "Password" => {
            for attr in e.attributes().flatten() {
                if attr.key.local_name().as_ref() == b"Type" {
                    let value = attr.unescape_value().unwrap_or_default();
                    req.password_is_digest = value.ends_with("#PasswordDigest");
                }
            }
        }
        "PanTilt" | "Zoom" => {
            let mut x = None;
            let mut y = None;
            for attr in e.attributes().flatten() {
                let value = attr
                    .unescape_value()
                    .ok()
                    .and_then(|v| v.trim().parse::<f32>().ok());
                match attr.key.local_name().as_ref() {
                    b"x" => x = value,
                    b"y" => y = value,
                    _ => {}
                }
            }
            if name == "PanTilt" {
                req.pan_tilt = Some((x.unwrap_or(0.0), y.unwrap_or(0.0)));
            } else {
                req.zoom = x;
            }
        }
        _ => {}
    }
    name
}

fn append_text(req: &mut SoapRequest, element: Option<&str>, text: &str) {
    let target = match element {
        Some("Username") => &mut req.username,
        Some("Password") => &mut req.password,
        Some("Nonce") => &mut req.nonce,
        Some("Created") => &mut req.created,
        Some("PresetToken") => &mut req.preset_token,
        Some("Timeout") => &mut req.timeout,
        _ => return,
    };
    target.get_or_insert_with(String::new).push_str(text);
}

/// Longest duration `parse_duration` returns; callers cap far lower anyway
const MAX_DURATION_SECS: f64 = 24.0 * 3600.0;

/// Parse the subset of xs:duration used for PTZ timeouts (`PT5S`, `PT1M30S`, `PT0.5S`)
pub(crate) fn parse_duration(value: &str) -> Option<std::time::Duration> {
    let rest = value.trim().strip_prefix("PT")?;
    let mut secs = 0f64;
    let mut num = String::new();
    for c in rest.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'H' | 'M' | 'S' => {
                let n: f64 = num.parse().ok()?;
                num.clear();
                secs += n * match c {
                    'H' => 3600.0,
                    'M' => 60.0,
                    _ => 1.0,
                };
            }
            _ => return None,
        }
    }
    if !num.is_empty() || !secs.is_finite() || secs < 0.0 {
        return None;
    }
    std::time::Duration::try_from_secs_f64(secs.min(MAX_DURATION_SECS)).ok()
}

/// Seconds since the Unix epoch of an xs:dateTime such as `2026-10-14T05:00:00Z` or
/// `2026-10-14T07:00:00.123+02:00` (no zone is taken as UTC)
pub(crate) fn parse_date_time(value: &str) -> Option<i64> {
    let (date, time) = value.trim().split_once(['T', 't'])?;
    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;

    let (clock, offset) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else if let Some(i) = time.rfind(['+', '-']) {
        let (clock, zone) = time.split_at(i);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (h, m) = zone[1..].split_once(':')?;
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        if h > 23 || m > 59 {
            return None;
        }
        (clock, sign * (h * 3600 + m * 60))
    } else {
        (time, 0)
    };
    let mut clock = clock.splitn(3, ':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second = clock.next()?;
    let second: i64 = second
        .split_once('.')
        .map_or(second, |(s, _)| s)
        .parse()
        .ok()?;

    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days-from-civil (Howard Hinnant), the inverse of civil_from_unix
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second - offset)
}

/// Escape for XML text or attributes, dropping characters XML 1.0 does not allow
/// even escaped (camera preset names may contain control characters)
pub(crate) fn escape(value: &str) -> String {
    let valid: String = value
        .chars()
        .filter(|&c| {
            matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
        })
        .collect();
    quick_xml::escape::escape(&valid).into_owned()
}

const ENVELOPE_START: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:ter="http://www.onvif.org/ver10/error"><SOAP-ENV:Body>"#;
const ENVELOPE_END: &str = "</SOAP-ENV:Body></SOAP-ENV:Envelope>";

pub(crate) fn envelope(body: &str) -> String {
    format!("{ENVELOPE_START}{body}{ENVELOPE_END}")
}

/// A SOAP 1.2 fault. `subcode` is an ONVIF `ter:` code such as `ActionNotSupported`
pub(crate) fn fault(code: &str, subcode: &str, reason: &str) -> String {
    envelope(&format!(
        "<SOAP-ENV:Fault><SOAP-ENV:Code><SOAP-ENV:Value>SOAP-ENV:{code}</SOAP-ENV:Value>\
         <SOAP-ENV:Subcode><SOAP-ENV:Value>ter:{subcode}</SOAP-ENV:Value></SOAP-ENV:Subcode></SOAP-ENV:Code>\
         <SOAP-ENV:Reason><SOAP-ENV:Text xml:lang=\"en\">{}</SOAP-ENV:Text></SOAP-ENV:Reason></SOAP-ENV:Fault>",
        escape(reason)
    ))
}

pub(crate) const PROFILE_TOKEN: &str = "MainProfile";
pub(crate) const VIDEO_SOURCE_TOKEN: &str = "VideoSource";
pub(crate) const PTZ_CONFIG_TOKEN: &str = "PtzConfig";
const PTZ_NODE_TOKEN: &str = "PtzNode";
const PAN_TILT_VELOCITY_SPACE: &str =
    "http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace";

/// Fields that depend on the request and the camera
pub(crate) struct Context<'a> {
    /// Base URL the client used to reach us, e.g. `http://192.168.1.20:8001`
    pub(crate) base_url: &'a str,
    pub(crate) camera_name: &'a str,
    /// RTSP URL of the camera's main stream, for GetStreamUri
    pub(crate) stream_uri: &'a str,
}

pub(crate) fn get_capabilities(ctx: &Context) -> String {
    let base = escape(ctx.base_url);
    envelope(&format!(
        "<tds:GetCapabilitiesResponse><tds:Capabilities>\
         <tt:Device><tt:XAddr>{base}/onvif/device_service</tt:XAddr></tt:Device>\
         <tt:Media><tt:XAddr>{base}/onvif/media_service</tt:XAddr>\
         <tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media>\
         <tt:PTZ><tt:XAddr>{base}/onvif/ptz_service</tt:XAddr></tt:PTZ>\
         </tds:Capabilities></tds:GetCapabilitiesResponse>"
    ))
}

pub(crate) fn get_services(ctx: &Context) -> String {
    let base = escape(ctx.base_url);
    let service = |ns: &str, path: &str| {
        format!(
            "<tds:Service><tds:Namespace>{ns}</tds:Namespace><tds:XAddr>{base}/onvif/{path}</tds:XAddr>\
             <tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service>"
        )
    };
    envelope(&format!(
        "<tds:GetServicesResponse>{}{}{}</tds:GetServicesResponse>",
        service("http://www.onvif.org/ver10/device/wsdl", "device_service"),
        service("http://www.onvif.org/ver10/media/wsdl", "media_service"),
        service("http://www.onvif.org/ver20/ptz/wsdl", "ptz_service"),
    ))
}

pub(crate) fn get_system_date_and_time(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix(secs);
    envelope(&format!(
        "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>\
         <tt:DateTimeType>NTP</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings>\
         <tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone>\
         <tt:UTCDateTime><tt:Time><tt:Hour>{hour}</tt:Hour><tt:Minute>{minute}</tt:Minute><tt:Second>{second}</tt:Second></tt:Time>\
         <tt:Date><tt:Year>{year}</tt:Year><tt:Month>{month}</tt:Month><tt:Day>{day}</tt:Day></tt:Date></tt:UTCDateTime>\
         </tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>"
    ))
}

/// Days-from-civil inverse (Howard Hinnant), avoids pulling in a date crate
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (
        year,
        month,
        day,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

pub(crate) fn get_device_information(ctx: &Context) -> String {
    envelope(&format!(
        "<tds:GetDeviceInformationResponse><tds:Manufacturer>Reolink</tds:Manufacturer>\
         <tds:Model>Neolink ONVIF bridge</tds:Model><tds:FirmwareVersion>{}</tds:FirmwareVersion>\
         <tds:SerialNumber>{}</tds:SerialNumber><tds:HardwareId>neolink</tds:HardwareId>\
         </tds:GetDeviceInformationResponse>",
        escape(env!("NEOLINK_VERSION")),
        escape(ctx.camera_name),
    ))
}

fn ptz_configuration() -> String {
    format!(
        "<tt:PTZConfiguration token=\"{PTZ_CONFIG_TOKEN}\"><tt:Name>PTZ</tt:Name><tt:UseCount>1</tt:UseCount>\
         <tt:NodeToken>{PTZ_NODE_TOKEN}</tt:NodeToken>\
         <tt:DefaultContinuousPanTiltVelocitySpace>{PAN_TILT_VELOCITY_SPACE}</tt:DefaultContinuousPanTiltVelocitySpace>\
         <tt:DefaultPTZSpeed><tt:PanTilt x=\"0.5\" y=\"0.5\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/GenericSpeedSpace\"/></tt:DefaultPTZSpeed>\
         <tt:DefaultPTZTimeout>PT10S</tt:DefaultPTZTimeout>\
         </tt:PTZConfiguration>"
    )
}

fn video_source_configuration() -> String {
    format!(
        "<tt:VideoSourceConfiguration token=\"VideoSourceConfig\"><tt:Name>VideoSource</tt:Name><tt:UseCount>1</tt:UseCount>\
         <tt:SourceToken>{VIDEO_SOURCE_TOKEN}</tt:SourceToken><tt:Bounds x=\"0\" y=\"0\" width=\"2560\" height=\"1440\"/>\
         </tt:VideoSourceConfiguration>"
    )
}

fn video_encoder_configuration() -> &'static str {
    "<tt:VideoEncoderConfiguration token=\"VideoEncoderConfig\"><tt:Name>Main</tt:Name><tt:UseCount>1</tt:UseCount>\
     <tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>2560</tt:Width><tt:Height>1440</tt:Height></tt:Resolution>\
     <tt:Quality>5</tt:Quality>\
     <tt:RateControl><tt:FrameRateLimit>15</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>2048</tt:BitrateLimit></tt:RateControl>\
     <tt:SessionTimeout>PT60S</tt:SessionTimeout></tt:VideoEncoderConfiguration>"
}

fn profile() -> String {
    format!(
        "<trt:Profiles token=\"{PROFILE_TOKEN}\" fixed=\"true\"><tt:Name>{PROFILE_TOKEN}</tt:Name>{}{}{}</trt:Profiles>",
        video_source_configuration(),
        video_encoder_configuration(),
        ptz_configuration()
    )
}

pub(crate) fn get_profiles() -> String {
    envelope(&format!(
        "<trt:GetProfilesResponse>{}</trt:GetProfilesResponse>",
        profile()
    ))
}

pub(crate) fn get_profile() -> String {
    envelope(&format!(
        "<trt:GetProfileResponse>{}</trt:GetProfileResponse>",
        profile().replace("trt:Profiles", "trt:Profile")
    ))
}

pub(crate) fn get_video_sources() -> String {
    envelope(&format!(
        "<trt:GetVideoSourcesResponse><trt:VideoSources token=\"{VIDEO_SOURCE_TOKEN}\">\
         <tt:Framerate>15</tt:Framerate><tt:Resolution><tt:Width>2560</tt:Width><tt:Height>1440</tt:Height></tt:Resolution>\
         </trt:VideoSources></trt:GetVideoSourcesResponse>"
    ))
}

pub(crate) fn get_stream_uri(ctx: &Context) -> String {
    envelope(&format!(
        "<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>{}</tt:Uri>\
         <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>\
         <tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetStreamUriResponse>",
        escape(ctx.stream_uri)
    ))
}

pub(crate) fn get_ptz_configurations() -> String {
    envelope(&format!(
        "<tptz:GetConfigurationsResponse>{}</tptz:GetConfigurationsResponse>",
        ptz_configuration().replace("tt:PTZConfiguration", "tptz:PTZConfiguration")
    ))
}

pub(crate) fn get_configuration_options() -> String {
    envelope(&format!(
        "<tptz:GetConfigurationOptionsResponse><tptz:PTZConfigurationOptions><tt:Spaces>\
         <tt:ContinuousPanTiltVelocitySpace><tt:URI>{PAN_TILT_VELOCITY_SPACE}</tt:URI>\
         <tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange><tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange>\
         </tt:ContinuousPanTiltVelocitySpace></tt:Spaces>\
         <tt:PTZTimeout><tt:Min>PT1S</tt:Min><tt:Max>PT60S</tt:Max></tt:PTZTimeout>\
         </tptz:PTZConfigurationOptions></tptz:GetConfigurationOptionsResponse>"
    ))
}

pub(crate) fn get_nodes() -> String {
    envelope(&format!(
        "<tptz:GetNodesResponse><tptz:PTZNode token=\"{PTZ_NODE_TOKEN}\" FixedHomePosition=\"false\"><tt:Name>PTZ</tt:Name>\
         <tt:SupportedPTZSpaces><tt:ContinuousPanTiltVelocitySpace><tt:URI>{PAN_TILT_VELOCITY_SPACE}</tt:URI>\
         <tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange><tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange>\
         </tt:ContinuousPanTiltVelocitySpace></tt:SupportedPTZSpaces>\
         <tt:MaximumNumberOfPresets>64</tt:MaximumNumberOfPresets><tt:HomeSupported>false</tt:HomeSupported>\
         </tptz:PTZNode></tptz:GetNodesResponse>"
    ))
}

pub(crate) fn get_ptz_service_capabilities() -> String {
    envelope(
        // MoveStatus/StatusPosition are left out on purpose: older PTZ schemas (the one
        // Frigate's client ships) surface them as raw strings, and "false" is truthy
        "<tptz:GetServiceCapabilitiesResponse><tptz:Capabilities EFlip=\"false\" Reverse=\"false\" \
         GetCompatibleConfigurations=\"false\"/></tptz:GetServiceCapabilitiesResponse>",
    )
}

pub(crate) fn get_status(moving: bool, now: std::time::SystemTime) -> String {
    let state = if moving { "MOVING" } else { "IDLE" };
    let (year, month, day, hour, minute, second) = civil_from_unix(
        now.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    envelope(&format!(
        "<tptz:GetStatusResponse><tptz:PTZStatus>\
         <tt:MoveStatus><tt:PanTilt>{state}</tt:PanTilt></tt:MoveStatus>\
         <tt:UtcTime>{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z</tt:UtcTime>\
         </tptz:PTZStatus></tptz:GetStatusResponse>"
    ))
}

pub(crate) fn get_presets(presets: &[(u8, String)]) -> String {
    let body: String = presets
        .iter()
        .map(|(id, name)| {
            format!(
                "<tptz:Preset token=\"{id}\"><tt:Name>{}</tt:Name></tptz:Preset>",
                escape(name)
            )
        })
        .collect();
    envelope(&format!(
        "<tptz:GetPresetsResponse>{body}</tptz:GetPresetsResponse>"
    ))
}

/// Response with an empty body element, e.g. `tptz:StopResponse`
pub(crate) fn empty(element: &str) -> String {
    envelope(&format!("<{element}/>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTINUOUS_MOVE: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<soap-env:Envelope xmlns:soap-env="http://www.w3.org/2003/05/soap-envelope">
  <soap-env:Header>
    <wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">
      <wsse:UsernameToken>
        <wsse:Username>frigate</wsse:Username>
        <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">pVsgWl3Z0G1kR8cXhC8YSm+0kxk=</wsse:Password>
        <wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">a2V5&amp;</wsse:Nonce>
        <wsu:Created xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">2026-10-14T05:00:00+00:00</wsu:Created>
      </wsse:UsernameToken>
    </wsse:Security>
  </soap-env:Header>
  <soap-env:Body>
    <ns0:ContinuousMove xmlns:ns0="http://www.onvif.org/ver20/ptz/wsdl">
      <ns0:ProfileToken>MainProfile</ns0:ProfileToken>
      <ns0:Velocity>
        <ns1:PanTilt xmlns:ns1="http://www.onvif.org/ver10/schema" x="-0.5" y="0"/>
      </ns0:Velocity>
      <ns0:Timeout>PT5S</ns0:Timeout>
    </ns0:ContinuousMove>
  </soap-env:Body>
</soap-env:Envelope>"#;

    #[test]
    fn parses_continuous_move() {
        let req = parse_request(CONTINUOUS_MOVE).unwrap();
        assert_eq!(req.operation, "ContinuousMove");
        assert_eq!(req.username.as_deref(), Some("frigate"));
        assert!(req.password_is_digest);
        assert_eq!(req.nonce.as_deref(), Some("a2V5&"));
        assert_eq!(req.created.as_deref(), Some("2026-10-14T05:00:00+00:00"));
        assert_eq!(req.pan_tilt, Some((-0.5, 0.0)));
        assert_eq!(req.timeout.as_deref(), Some("PT5S"));
        assert_eq!(req.preset_token, None);
    }

    #[test]
    fn parses_goto_preset_without_header() {
        let req = parse_request(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body>
               <GotoPreset xmlns="http://www.onvif.org/ver20/ptz/wsdl"><ProfileToken>MainProfile</ProfileToken>
               <PresetToken>3</PresetToken></GotoPreset></s:Body></s:Envelope>"#,
        )
        .unwrap();
        assert_eq!(req.operation, "GotoPreset");
        assert_eq!(req.preset_token.as_deref(), Some("3"));
        assert_eq!(req.username, None);
    }

    #[test]
    fn empty_operation_element() {
        let req = parse_request(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><GetProfiles xmlns="http://www.onvif.org/ver10/media/wsdl"/></s:Body></s:Envelope>"#,
        )
        .unwrap();
        assert_eq!(req.operation, "GetProfiles");
    }

    #[test]
    fn durations() {
        assert_eq!(
            parse_duration("PT5S"),
            Some(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            parse_duration("PT1M30S"),
            Some(std::time::Duration::from_secs(90))
        );
        assert_eq!(
            parse_duration("PT0.5S"),
            Some(std::time::Duration::from_millis(500))
        );
        assert_eq!(parse_duration("5"), None);
        // Absurd timeouts are capped rather than overflowing
        assert_eq!(
            parse_duration("PT99999999999999999999999S"),
            Some(std::time::Duration::from_secs(86400))
        );
        assert_eq!(parse_duration(&format!("PT{}H", "9".repeat(400))), None);
        assert_eq!(parse_duration("PT.S"), None);
    }

    #[test]
    fn date_times() {
        assert_eq!(parse_date_time("2026-10-14T05:06:07Z"), Some(1791954367));
        assert_eq!(
            parse_date_time("2026-10-14T07:06:07.123+02:00"),
            Some(1791954367)
        );
        assert_eq!(
            parse_date_time("2026-10-14T00:06:07-05:00"),
            Some(1791954367)
        );
        assert_eq!(parse_date_time("2026-10-14T05:06:07"), Some(1791954367));
        assert_eq!(parse_date_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_date_time("2024-02-29T00:00:00Z"), Some(1709164800));
        assert_eq!(parse_date_time("yesterday"), None);
        assert_eq!(parse_date_time("2026-13-01T00:00:00Z"), None);
        // Out-of-range parts are rejected rather than overflowing
        assert_eq!(parse_date_time("9223372036854775807-01-01T00:00:00Z"), None);
        assert_eq!(
            parse_date_time("2026-10-14T05:06:07+9223372036854775807:00"),
            None
        );
    }

    #[test]
    fn escape_drops_invalid_characters() {
        assert_eq!(
            escape("Gate\u{1}\u{8} <1>\tA&B\u{FFFF}"),
            "Gate &lt;1&gt;\tA&amp;B"
        );
        let xml = get_presets(&[(1, "Drive\u{0}way".to_string())]);
        assert!(xml.contains("<tt:Name>Driveway</tt:Name>"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-10-14T05:06:07Z
        assert_eq!(civil_from_unix(1791954367), (2026, 10, 14, 5, 6, 7));
    }
}
