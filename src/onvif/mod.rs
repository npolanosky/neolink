//! ONVIF PTZ bridge
//!
//! Serves a minimal ONVIF device (device, media and PTZ services) for one camera
//! on its own port, so NVRs that only speak ONVIF for PTZ (Frigate) can pan, tilt
//! and recall presets on Baichuan-only cameras.
//!
//! Enabled per camera with `onvif_port`. Supported: ContinuousMove (pan/tilt,
//! the dominant axis wins since the camera moves one direction at a time), Stop,
//! GetPresets, GotoPreset, GetStatus. No position feedback exists in the camera
//! protocol, so relative/absolute moves and autotracking are not offered.
//!
//! When `[[users]]` are configured the same credentials (and the camera's
//! `permitted_users`) are required as WS-Security UsernameToken.
mod soap;

use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, header, server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use log::*;
use neolink_core::{bc::xml::PtzPreset, bc_protocol::Direction as BcDirection};
use sha1::{Digest, Sha1};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tokio::{
    net::TcpListener,
    sync::{
        watch::{self, Receiver as WatchReceiver},
        Mutex,
    },
    task::JoinSet,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{
    common::NeoInstance,
    config::{CameraConfig, Config, StreamConfig, UserConfig},
    AnyResult,
};

/// Longest a single camera command may take before the client gets a fault
const CAMERA_TIMEOUT: Duration = Duration::from_secs(10);
/// A ContinuousMove without a Timeout stops after this long, in case the Stop is lost
const DEFAULT_MOVE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MOVE_TIMEOUT: Duration = Duration::from_secs(60);
/// Reolink PTZ speeds run up to 64; ONVIF velocities are -1..1
const MAX_SPEED: f32 = 64.0;
const MAX_BODY: usize = 64 * 1024;
/// How long GetPresets waits for the camera when it can fall back to the cache.
/// Frigate gives its whole ONVIF initialisation about 10s.
const CACHED_PRESETS_TIMEOUT: Duration = Duration::from_secs(3);
/// Applies to the request headers and to idle keep-alive connections
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Further connections are dropped; an NVR needs one or two
const MAX_CONNECTIONS: usize = 16;
const MAX_BIND_RETRY: Duration = Duration::from_secs(30);
/// A stale WS-Security `Created` stamp points at a replayed request
const MAX_CLOCK_SKEW_SECS: i64 = 5 * 60;

/// Preset id and name
type PresetList = Vec<(u8, String)>;
/// Result of one read of the presets, `None` while it is under way. The error is
/// a string so every waiting request can have a copy.
type PresetRead = Option<Result<PresetList, String>>;

struct Bridge {
    camera: NeoInstance,
    camera_config: WatchReceiver<CameraConfig>,
    config: WatchReceiver<Config>,
    motion: Arc<MotionState>,
    /// Last preset list read from the camera. Clients such as Frigate only ask for
    /// presets when they initialise, which may be while the camera is offline.
    presets: Mutex<Option<PresetList>>,
    /// The preset read in progress, shared by all requests that arrive meanwhile
    preset_read: Mutex<Option<WatchReceiver<PresetRead>>>,
    /// Cancelled when the server stops, ending background preset reads
    cancel: CancellationToken,
}

#[derive(Default)]
struct MotionState {
    /// Bumped by every move and preset recall, so a pending auto-stop can tell a
    /// newer command took over. Stops leave it alone: a redundant stop is harmless.
    generation: AtomicU64,
    /// Bumped by every stop, auto-stops included
    stops: AtomicU64,
    moving: AtomicBool,
}

/// What a move needs to check once its command has been sent
#[derive(Debug, Clone, Copy)]
struct MoveTicket {
    generation: u64,
    stops: u64,
}

impl MotionState {
    fn start_move(&self) -> MoveTicket {
        let stops = self.stops.load(Ordering::SeqCst);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.moving.store(true, Ordering::Relaxed);
        MoveTicket { generation, stops }
    }

    /// No newer move or preset recall has replaced this move
    fn is_current(&self, ticket: MoveTicket) -> bool {
        self.generation.load(Ordering::SeqCst) == ticket.generation
    }

    /// A stop was requested while this move was in flight, so it may have reached
    /// the camera first (e.g. both waited for a reconnect) and left it moving
    fn stopped_during(&self, ticket: MoveTicket) -> bool {
        self.stops.load(Ordering::SeqCst) != ticket.stops && self.is_current(ticket)
    }

    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.moving.store(false, Ordering::Relaxed);
    }

    /// The camera stops by itself at the preset, and an auto-stop would cut it short
    fn recall_preset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.moving.store(false, Ordering::Relaxed);
    }
}

/// Run the ONVIF server for one camera until the camera task is cancelled
///
/// Never returns an error so that a port clash does not take the camera's
/// RTSP stream down with it.
pub(crate) async fn serve(camera: NeoInstance, config: WatchReceiver<Config>) -> AnyResult<()> {
    if let Err(e) = serve_inner(camera, config).await {
        error!("ONVIF server stopped: {e:?}");
    }
    futures::future::pending::<()>().await;
    Ok(())
}

async fn serve_inner(camera: NeoInstance, config: WatchReceiver<Config>) -> AnyResult<()> {
    let camera_config = camera.config().await?;
    let (name, port) = {
        let cam = camera_config.borrow();
        (cam.name.clone(), cam.onvif_port)
    };
    let Some(port) = port else {
        return futures::future::pending().await;
    };
    let bind_addr = config.borrow().bind_addr.clone();
    let listener = bind(&name, &bind_addr, port).await;
    info!("{name}: ONVIF PTZ available at http://{bind_addr}:{port}/onvif/device_service");

    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let bridge = Arc::new(Bridge {
        camera,
        camera_config,
        config,
        motion: Default::default(),
        presets: Default::default(),
        preset_read: Default::default(),
        cancel,
    });

    // Fill the preset cache as soon as the camera is reachable
    bridge.read_presets().await;

    // Owned here so that open connections close when the camera task stops
    let mut connections = JoinSet::new();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // Usually out of file descriptors, which clears up by itself
                debug!("{name}: ONVIF accept failed: {e}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        while connections.try_join_next().is_some() {}
        if connections.len() >= MAX_CONNECTIONS {
            debug!("{name}: Dropping ONVIF connection from {peer}: {MAX_CONNECTIONS} already open");
            continue;
        }
        let bridge = bridge.clone();
        connections.spawn(async move {
            let service = service_fn(move |req| {
                let bridge = bridge.clone();
                async move { Ok::<_, std::convert::Infallible>(bridge.handle(req, peer).await) }
            });
            if let Err(e) = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                debug!("ONVIF connection from {peer} ended: {e}");
            }
        });
    }
}

/// Bind the ONVIF port, retrying while it is taken (e.g. by a previous instance)
async fn bind(name: &str, bind_addr: &str, port: u16) -> TcpListener {
    let mut delay = Duration::from_secs(2);
    let mut last_error = None;
    loop {
        match TcpListener::bind((bind_addr, port)).await {
            Ok(listener) => return listener,
            Err(e) => {
                let e = e.to_string();
                if last_error.as_ref() != Some(&e) {
                    error!(
                        "{name}: Could not bind ONVIF port {bind_addr}:{port}: {e}. Retrying in the background"
                    );
                } else {
                    debug!("{name}: Could not bind ONVIF port {bind_addr}:{port}: {e}");
                }
                last_error = Some(e);
            }
        }
        sleep(delay).await;
        delay = (delay * 2).min(MAX_BIND_RETRY);
    }
}

impl Bridge {
    async fn handle(
        self: &Arc<Self>,
        req: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<Full<Bytes>> {
        let name = self.camera_config.borrow().name.clone();
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let body = match timeout(
            BODY_READ_TIMEOUT,
            Limited::new(req.into_body(), MAX_BODY).collect(),
        )
        .await
        {
            Ok(Ok(body)) => body.to_bytes(),
            Ok(Err(e)) => {
                return reply(400, soap::fault("Sender", "InvalidArgVal", &e.to_string()));
            }
            Err(_) => {
                return reply(
                    400,
                    soap::fault(
                        "Sender",
                        "InvalidArgVal",
                        "Request body not received in time",
                    ),
                );
            }
        };
        let body = String::from_utf8_lossy(&body);
        let request = match soap::parse_request(&body) {
            Ok(r) => r,
            Err(e) => {
                debug!("{name}: Bad ONVIF request from {peer}: {e}");
                return reply(400, soap::fault("Sender", "WellFormed", &e));
            }
        };
        debug!("{name}: ONVIF {} from {peer}", request.operation);

        // Clients may fetch the clock before authenticating, to work out time offsets
        if request.operation != "GetSystemDateAndTime" && !self.authorised(&request) {
            warn!(
                "{name}: ONVIF {} from {peer} rejected: not authorised",
                request.operation
            );
            return reply(
                400,
                soap::fault("Sender", "NotAuthorized", "Sender not authorized"),
            );
        }

        match self.dispatch(&request, host.as_deref()).await {
            Ok((status, xml)) => reply(status, xml),
            Err(e) => {
                warn!("{name}: ONVIF {} failed: {e:?}", request.operation);
                reply(500, soap::fault("Receiver", "Action", &format!("{e:#}")))
            }
        }
    }

    /// The HTTP status and SOAP response for a request
    async fn dispatch(
        self: &Arc<Self>,
        req: &soap::SoapRequest,
        host: Option<&str>,
    ) -> AnyResult<(u16, String)> {
        let (camera_name, onvif_port, stream) = {
            let cam = self.camera_config.borrow();
            (
                cam.name.clone(),
                cam.onvif_port.unwrap_or_default(),
                cam.stream,
            )
        };
        let (bind_addr, rtsp_port) = {
            let config = self.config.borrow();
            (config.bind_addr.clone(), config.bind_port)
        };
        // Answer with the host the client used, since bind may be 0.0.0.0
        let host = host
            .map(str::to_string)
            .unwrap_or_else(|| format!("{bind_addr}:{onvif_port}"));
        let base_url = format!("http://{host}");
        let host_only = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(&host);
        let stream_uri = format!(
            "rtsp://{host_only}:{rtsp_port}/{}/{}",
            percent_encode(&camera_name),
            stream_path(stream)
        );
        let ctx = soap::Context {
            base_url: &base_url,
            camera_name: &camera_name,
            stream_uri: &stream_uri,
        };

        let xml = match req.operation.as_str() {
            // Device service
            "GetCapabilities" => soap::get_capabilities(&ctx),
            "GetServices" => soap::get_services(&ctx),
            "GetSystemDateAndTime" => soap::get_system_date_and_time(SystemTime::now()),
            "GetDeviceInformation" => soap::get_device_information(&ctx),
            // Media service
            "GetProfiles" => soap::get_profiles(),
            "GetProfile" => soap::get_profile(),
            "GetVideoSources" => soap::get_video_sources(),
            "GetStreamUri" => soap::get_stream_uri(&ctx),
            // PTZ service
            "GetServiceCapabilities" => soap::get_ptz_service_capabilities(),
            "GetConfigurations" => soap::get_ptz_configurations(),
            "GetConfigurationOptions" => soap::get_configuration_options(),
            "GetNodes" => soap::get_nodes(),
            "GetStatus" => soap::get_status(
                self.motion.moving.load(Ordering::Relaxed),
                SystemTime::now(),
            ),
            "GetPresets" => soap::get_presets(&self.presets().await?),
            "GotoPreset" => {
                let token = req
                    .preset_token
                    .as_deref()
                    .ok_or_else(|| anyhow!("GotoPreset without PresetToken"))?;
                let id: u8 = token
                    .trim()
                    .parse()
                    .with_context(|| format!("Unknown preset token {token:?}"))?;
                self.goto_preset(id).await?;
                soap::empty("tptz:GotoPresetResponse")
            }
            "ContinuousMove" => {
                let move_timeout = req
                    .timeout
                    .as_deref()
                    .and_then(soap::parse_duration)
                    .unwrap_or(DEFAULT_MOVE_TIMEOUT)
                    .min(MAX_MOVE_TIMEOUT);
                let (x, y) = req.pan_tilt.unwrap_or((0.0, 0.0));
                let (pan_speed, tilt_speed, nudge) = {
                    let cam = self.camera_config.borrow();
                    let pan = cam.ptz_speed;
                    (pan, cam.ptz_tilt_speed.unwrap_or(pan), cam.ptz_nudge_ms)
                };
                // Nudge mode: each press is a short fixed move, regardless of how
                // late the Stop arrives (it can lag a lot over a relayed connection)
                let move_timeout = match nudge {
                    Some(ms) => move_timeout.min(Duration::from_millis(ms)),
                    None => move_timeout,
                };
                let Some((direction, speed)) = velocity_to_command(x, y, pan_speed, tilt_speed)
                else {
                    self.stop().await?;
                    return Ok((200, soap::empty("tptz:ContinuousMoveResponse")));
                };
                self.continuous_move(direction, speed, move_timeout).await?;
                soap::empty("tptz:ContinuousMoveResponse")
            }
            "Stop" => {
                self.stop().await?;
                soap::empty("tptz:StopResponse")
            }
            other => {
                debug!("{camera_name}: Unsupported ONVIF operation {other}");
                return Ok((
                    500,
                    soap::fault(
                        "Receiver",
                        "ActionNotSupported",
                        &format!("{other} is not supported by the neolink ONVIF bridge"),
                    ),
                ));
            }
        };
        Ok((200, xml))
    }

    /// No users configured means open access, matching the RTSP server
    fn authorised(&self, req: &soap::SoapRequest) -> bool {
        let users = self.config.borrow().users.clone();
        if users.is_empty() {
            return true;
        }
        let permitted = self.camera_config.borrow().permitted_users.clone();
        let (Some(username), Some(password)) = (&req.username, &req.password) else {
            return false;
        };
        if req.password_is_digest && !created_is_recent(req.created.as_deref(), SystemTime::now()) {
            debug!("ONVIF UsernameToken from {username} is more than 5 minutes old or ahead");
            return false;
        }
        users
            .iter()
            .filter(|user| &user.name == username)
            .filter(|user| {
                permitted
                    .as_ref()
                    .map(|p| p.iter().any(|name| name == "anyone" || name == &user.name))
                    .unwrap_or(true)
            })
            .any(|user| password_matches(user, req, password))
    }

    /// Fresh from the camera when it answers in time, otherwise the cached list
    async fn presets(self: &Arc<Self>) -> AnyResult<PresetList> {
        let cached = self.presets.lock().await.clone();
        let mut read = self.read_presets().await;
        // Waiting less with a cache keeps clients within their init budget; the
        // read carries on in the background and updates the cache
        let wait = if cached.is_some() {
            CACHED_PRESETS_TIMEOUT
        } else {
            CAMERA_TIMEOUT
        };
        let fresh = match timeout(wait, read.wait_for(Option::is_some)).await {
            Ok(Ok(result)) => result
                .clone()
                .unwrap_or_else(|| Err("No result".to_string())),
            Ok(Err(_)) => Err("Preset read was cancelled".to_string()),
            Err(_) => Err("Camera did not answer in time".to_string()),
        };
        match (fresh, cached) {
            (Ok(presets), _) => Ok(presets),
            (Err(e), Some(cached)) => {
                debug!("Using cached PTZ presets: {e}");
                Ok(cached)
            }
            (Err(e), None) => Err(anyhow!(e)),
        }
    }

    /// Start reading the presets into the cache, unless a read is already under way
    async fn read_presets(self: &Arc<Self>) -> WatchReceiver<PresetRead> {
        let mut current = self.preset_read.lock().await;
        if let Some(read) = current.as_ref() {
            // A read whose task went away without an answer is not under way
            if read.borrow().is_none() && read.has_changed().is_ok() {
                return read.clone();
            }
        }
        let (tx, rx) = watch::channel(None);
        *current = Some(rx.clone());
        let bridge = self.clone();
        tokio::spawn(async move {
            let name = bridge.camera_config.borrow().name.clone();
            let result = tokio::select! {
                _ = bridge.cancel.cancelled() => return,
                v = bridge.camera.run_task(|cam| {
                    Box::pin(async move { AnyResult::Ok(cam.get_ptz_preset().await?) })
                }) => v,
            };
            let result = match result {
                Ok(presets) => {
                    let presets = preset_list(presets);
                    debug!("{name}: {} PTZ presets cached for ONVIF", presets.len());
                    *bridge.presets.lock().await = Some(presets.clone());
                    Ok(presets)
                }
                Err(e) => {
                    debug!("{name}: Could not read PTZ presets: {e:?}");
                    Err(format!("{e:#}"))
                }
            };
            let _ = tx.send(Some(result));
        });
        rx
    }

    async fn goto_preset(&self, id: u8) -> AnyResult<()> {
        self.motion.recall_preset();
        timeout(
            CAMERA_TIMEOUT,
            self.camera.run_task(|cam| {
                Box::pin(async move { AnyResult::Ok(cam.moveto_ptz_preset(id).await?) })
            }),
        )
        .await
        .map_err(|_| anyhow!("Camera did not answer in time"))?
    }

    async fn continuous_move(
        &self,
        direction: BcDirection,
        speed: f32,
        move_timeout: Duration,
    ) -> AnyResult<()> {
        let ticket = self.motion.start_move();
        // Armed before sending: the camera may act on the move even if its reply is
        // lost or late. Timing from the send also keeps nudges the same length
        // whatever the round trip.
        self.arm_auto_stop(ticket, move_timeout);
        let sent = send_ptz(&self.camera, direction, speed).await;
        if self.motion.stopped_during(ticket) {
            // In the background so the client's answer is not held up
            debug!("PTZ stop arrived while the move was in flight, stopping again");
            let camera = self.camera.clone();
            tokio::spawn(async move {
                if let Err(e) = send_ptz(&camera, BcDirection::Stop, 0.0).await {
                    debug!("PTZ stop after move failed: {e:?}");
                }
            });
        }
        sent
    }

    /// Safety stop: the camera keeps moving until told otherwise
    fn arm_auto_stop(&self, ticket: MoveTicket, after: Duration) {
        let camera = self.camera.clone();
        let motion = self.motion.clone();
        tokio::spawn(async move {
            sleep(after).await;
            if motion.is_current(ticket) {
                motion.stop();
                if let Err(e) = send_ptz(&camera, BcDirection::Stop, 0.0).await {
                    debug!("PTZ auto-stop failed: {e:?}");
                }
            }
        });
    }

    async fn stop(&self) -> AnyResult<()> {
        self.motion.stop();
        send_ptz(&self.camera, BcDirection::Stop, 0.0).await
    }
}

async fn send_ptz(camera: &NeoInstance, direction: BcDirection, speed: f32) -> AnyResult<()> {
    timeout(
        CAMERA_TIMEOUT,
        camera.run_task(|cam| {
            Box::pin(async move { AnyResult::Ok(cam.send_ptz(direction, speed).await?) })
        }),
    )
    .await
    .map_err(|_| anyhow!("Camera did not answer in time"))?
}

fn preset_list(presets: PtzPreset) -> PresetList {
    presets
        .preset_list
        .preset
        .into_iter()
        .map(|p| {
            let id = p.id;
            (id, p.name.unwrap_or_else(|| format!("Preset {id}")))
        })
        .collect()
}

/// RTSP path of the stream GetStreamUri offers: main unless only sub or extern is served
fn stream_path(stream: StreamConfig) -> &'static str {
    match stream {
        StreamConfig::Sub => "sub",
        StreamConfig::Extern => "extern",
        StreamConfig::All | StreamConfig::Both | StreamConfig::Main | StreamConfig::None => "main",
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters, for a URI path segment
fn percent_encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn reply(status: u16, xml: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")
        .body(Full::new(Bytes::from(xml)))
        .expect("static response parts are valid")
}

/// Pick the dominant axis: the camera only moves in one direction at a time
///
/// `pan_speed`/`tilt_speed` are the Reolink speeds (1-64) for a velocity of 0.5,
/// which is what Frigate's arrow buttons send; other velocities scale linearly.
fn velocity_to_command(
    x: f32,
    y: f32,
    pan_speed: u8,
    tilt_speed: u8,
) -> Option<(BcDirection, f32)> {
    const DEAD_ZONE: f32 = 0.01;
    let (direction, magnitude, base) = if x.abs() >= y.abs() {
        (
            if x < 0.0 {
                BcDirection::Left
            } else {
                BcDirection::Right
            },
            x.abs(),
            pan_speed,
        )
    } else {
        (
            if y < 0.0 {
                BcDirection::Down
            } else {
                BcDirection::Up
            },
            y.abs(),
            tilt_speed,
        )
    };
    if magnitude < DEAD_ZONE {
        return None;
    }
    Some((
        direction,
        (magnitude.min(1.0) * 2.0 * base as f32).clamp(1.0, MAX_SPEED),
    ))
}

/// Whether a WS-Security `Created` stamp is within a few minutes of `now`. Stamps
/// that are missing or cannot be parsed are let through rather than locking out
/// a client over a format we do not know.
fn created_is_recent(created: Option<&str>, now: SystemTime) -> bool {
    let Some(created) = created.and_then(soap::parse_date_time) else {
        return true;
    };
    let now = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    (created - now).abs() <= MAX_CLOCK_SKEW_SECS
}

/// WS-Security UsernameToken: digest is Base64(SHA1(nonce + created + password))
fn password_matches(user: &UserConfig, req: &soap::SoapRequest, given: &str) -> bool {
    let Some(expected) = user.pass.as_deref() else {
        return false;
    };
    if !req.password_is_digest {
        return given == expected;
    }
    let (Some(nonce), Some(created)) = (&req.nonce, &req.created) else {
        return false;
    };
    let Ok(nonce) = BASE64.decode(nonce.trim()) else {
        return false;
    };
    let mut hasher = Sha1::new();
    hasher.update(&nonce);
    hasher.update(created.as_bytes());
    hasher.update(expected.as_bytes());
    BASE64.encode(hasher.finalize()) == given.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominant_axis() {
        let (d, s) = velocity_to_command(-0.5, 0.0, 32, 32).unwrap();
        assert!(d == BcDirection::Left && s == 32.0);
        let (d, _) = velocity_to_command(0.2, 0.6, 32, 32).unwrap();
        assert!(d == BcDirection::Up);
        let (d, s) = velocity_to_command(0.0, -1.5, 32, 32).unwrap();
        assert!(d == BcDirection::Down && s == 64.0);
        assert!(velocity_to_command(0.0, 0.0, 32, 32).is_none());
    }

    #[test]
    fn per_axis_speed() {
        // Frigate's buttons send 0.5, which maps to the configured speed
        let (_, s) = velocity_to_command(0.5, 0.0, 8, 4).unwrap();
        assert_eq!(s, 8.0);
        let (_, s) = velocity_to_command(0.0, 0.5, 8, 4).unwrap();
        assert_eq!(s, 4.0);
        // Never below the camera's minimum speed
        let (_, s) = velocity_to_command(0.05, 0.0, 1, 1).unwrap();
        assert_eq!(s, 1.0);
    }

    #[test]
    fn digest_auth() {
        let user = UserConfig {
            name: "frigate".to_string(),
            pass: Some("secret".to_string()),
        };
        let nonce = b"0123456789abcdef";
        let created = "2026-10-14T05:00:00Z";
        let mut hasher = Sha1::new();
        hasher.update(nonce);
        hasher.update(created.as_bytes());
        hasher.update(b"secret");
        let digest = BASE64.encode(hasher.finalize());
        let req = soap::SoapRequest {
            password_is_digest: true,
            nonce: Some(BASE64.encode(nonce)),
            created: Some(created.to_string()),
            ..Default::default()
        };
        assert!(password_matches(&user, &req, &digest));
        assert!(!password_matches(&user, &req, "wrong"));
        let plain = soap::SoapRequest::default();
        assert!(password_matches(&user, &plain, "secret"));
    }

    #[test]
    fn created_window() {
        // 2026-10-14T05:00:00Z
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1791954000);
        assert!(created_is_recent(Some("2026-10-14T05:04:00Z"), now));
        assert!(created_is_recent(Some("2026-10-14T04:56:00+00:00"), now));
        assert!(!created_is_recent(Some("2026-10-14T04:50:00Z"), now));
        assert!(!created_is_recent(Some("2026-10-14T05:06:00Z"), now));
        // Unknown formats are not held against the client
        assert!(created_is_recent(Some("garbage"), now));
        assert!(created_is_recent(None, now));
    }

    #[test]
    fn stop_overtaking_move() {
        let motion = MotionState::default();
        let ticket = motion.start_move();
        assert!(motion.moving.load(Ordering::Relaxed));
        assert!(!motion.stopped_during(ticket));
        // A stop while the move is in flight: send another once the move lands
        motion.stop();
        assert!(motion.stopped_during(ticket));
        // ...and it leaves the auto-stop armed
        assert!(motion.is_current(ticket));
    }

    #[test]
    fn newer_commands_supersede_move() {
        let motion = MotionState::default();
        let first = motion.start_move();
        motion.stop();
        let second = motion.start_move();
        // The first move's auto-stop and re-stop must not cut the second short
        assert!(!motion.is_current(first));
        assert!(!motion.stopped_during(first));
        assert!(motion.is_current(second));
        assert!(!motion.stopped_during(second));
        motion.recall_preset();
        assert!(!motion.is_current(second));
        assert!(!motion.moving.load(Ordering::Relaxed));
    }

    #[test]
    fn stream_uri_parts() {
        assert_eq!(stream_path(StreamConfig::All), "main");
        assert_eq!(stream_path(StreamConfig::Main), "main");
        assert_eq!(stream_path(StreamConfig::Sub), "sub");
        assert_eq!(percent_encode("Front Door/1é"), "Front%20Door%2F1%C3%A9");
        assert_eq!(percent_encode("cam_1-a.b~"), "cam_1-a.b~");
    }
}
