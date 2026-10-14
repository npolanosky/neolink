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
use hyper_util::rt::TokioIo;
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
    sync::{watch::Receiver as WatchReceiver, Mutex},
    time::timeout,
};

use crate::{
    common::NeoInstance,
    config::{CameraConfig, Config, UserConfig},
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

struct Bridge {
    camera: NeoInstance,
    camera_config: WatchReceiver<CameraConfig>,
    config: WatchReceiver<Config>,
    motion: Arc<MotionState>,
    /// Last preset list read from the camera. Clients such as Frigate only ask for
    /// presets when they initialise, which may be while the camera is offline.
    presets: Arc<Mutex<Option<Vec<(u8, String)>>>>,
}

#[derive(Default)]
struct MotionState {
    /// Bumped by every move or stop, so a pending auto-stop can tell it is stale
    generation: AtomicU64,
    moving: AtomicBool,
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
    let listener = TcpListener::bind((bind_addr.as_str(), port))
        .await
        .with_context(|| format!("{name}: Could not bind ONVIF port {bind_addr}:{port}"))?;
    info!("{name}: ONVIF PTZ available at http://{bind_addr}:{port}/onvif/device_service");

    let bridge = Arc::new(Bridge {
        camera,
        camera_config,
        config,
        motion: Default::default(),
        presets: Default::default(),
    });

    // Fill the preset cache as soon as the camera is reachable
    let prefetch = bridge.clone();
    tokio::spawn(async move {
        match prefetch
            .camera
            .run_task(|cam| Box::pin(async move { AnyResult::Ok(cam.get_ptz_preset().await?) }))
            .await
        {
            Ok(presets) => {
                let presets = preset_list(presets);
                debug!("{name}: {} PTZ presets cached for ONVIF", presets.len());
                *prefetch.presets.lock().await = Some(presets);
            }
            Err(e) => debug!("{name}: Could not read PTZ presets: {e:?}"),
        }
    });

    loop {
        let (stream, peer) = listener.accept().await?;
        let bridge = bridge.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let bridge = bridge.clone();
                async move { Ok::<_, std::convert::Infallible>(bridge.handle(req, peer).await) }
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                debug!("ONVIF connection from {peer} ended: {e}");
            }
        });
    }
}

impl Bridge {
    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Full<Bytes>> {
        let name = self.camera_config.borrow().name.clone();
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let body = match Limited::new(req.into_body(), MAX_BODY).collect().await {
            Ok(body) => body.to_bytes(),
            Err(e) => {
                return reply(400, soap::fault("Sender", "InvalidArgVal", &e.to_string()));
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
            Ok(xml) => reply(200, xml),
            Err(e) => {
                warn!("{name}: ONVIF {} failed: {e:?}", request.operation);
                reply(500, soap::fault("Receiver", "Action", &format!("{e:#}")))
            }
        }
    }

    async fn dispatch(&self, req: &soap::SoapRequest, host: Option<&str>) -> AnyResult<String> {
        let (camera_name, onvif_port) = {
            let cam = self.camera_config.borrow();
            (cam.name.clone(), cam.onvif_port.unwrap_or_default())
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
        let stream_uri = format!("rtsp://{host_only}:{rtsp_port}/{camera_name}/main");
        let ctx = soap::Context {
            base_url: &base_url,
            camera_name: &camera_name,
            stream_uri: &stream_uri,
        };

        Ok(match req.operation.as_str() {
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
                self.continuous_move(x, y, move_timeout).await?;
                soap::empty("tptz:ContinuousMoveResponse")
            }
            "Stop" => {
                self.stop().await?;
                soap::empty("tptz:StopResponse")
            }
            other => {
                debug!("{camera_name}: Unsupported ONVIF operation {other}");
                return Ok(soap::fault(
                    "Receiver",
                    "ActionNotSupported",
                    &format!("{other} is not supported by the neolink ONVIF bridge"),
                ));
            }
        })
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

    /// Fresh from the camera when it answers, otherwise the cached list
    async fn presets(&self) -> AnyResult<Vec<(u8, String)>> {
        let fresh = timeout(
            CAMERA_TIMEOUT,
            self.camera.run_task(|cam| {
                Box::pin(async move { AnyResult::Ok(cam.get_ptz_preset().await?) })
            }),
        )
        .await
        .map_err(|_| anyhow!("Camera did not answer in time"))
        .and_then(|r| r);
        let mut cache = self.presets.lock().await;
        match fresh {
            Ok(presets) => {
                let presets = preset_list(presets);
                *cache = Some(presets.clone());
                Ok(presets)
            }
            Err(e) => cache.clone().ok_or(e),
        }
    }

    async fn goto_preset(&self, id: u8) -> AnyResult<()> {
        self.motion.generation.fetch_add(1, Ordering::SeqCst);
        timeout(
            CAMERA_TIMEOUT,
            self.camera.run_task(|cam| {
                Box::pin(async move { AnyResult::Ok(cam.moveto_ptz_preset(id).await?) })
            }),
        )
        .await
        .map_err(|_| anyhow!("Camera did not answer in time"))?
    }

    async fn continuous_move(&self, x: f32, y: f32, move_timeout: Duration) -> AnyResult<()> {
        let Some((direction, speed)) = velocity_to_command(x, y) else {
            return self.stop().await;
        };
        let generation = self.motion.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.send_ptz(direction, speed).await?;
        self.motion.moving.store(true, Ordering::Relaxed);

        // Safety stop: the camera keeps moving until told otherwise
        let camera = self.camera.clone();
        let motion = self.motion.clone();
        tokio::spawn(async move {
            tokio::time::sleep(move_timeout).await;
            if motion.generation.load(Ordering::SeqCst) == generation {
                motion.moving.store(false, Ordering::Relaxed);
                let _ = timeout(
                    CAMERA_TIMEOUT,
                    camera.run_task(|cam| {
                        Box::pin(async move {
                            AnyResult::Ok(cam.send_ptz(BcDirection::Stop, 0.0).await?)
                        })
                    }),
                )
                .await;
            }
        });
        Ok(())
    }

    async fn stop(&self) -> AnyResult<()> {
        self.motion.generation.fetch_add(1, Ordering::SeqCst);
        self.motion.moving.store(false, Ordering::Relaxed);
        self.send_ptz(BcDirection::Stop, 0.0).await
    }

    async fn send_ptz(&self, direction: BcDirection, speed: f32) -> AnyResult<()> {
        timeout(
            CAMERA_TIMEOUT,
            self.camera.run_task(|cam| {
                Box::pin(async move { AnyResult::Ok(cam.send_ptz(direction, speed).await?) })
            }),
        )
        .await
        .map_err(|_| anyhow!("Camera did not answer in time"))?
    }
}

fn preset_list(presets: PtzPreset) -> Vec<(u8, String)> {
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

fn reply(status: u16, xml: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")
        .body(Full::new(Bytes::from(xml)))
        .expect("static response parts are valid")
}

/// Pick the dominant axis: the camera only moves in one direction at a time
fn velocity_to_command(x: f32, y: f32) -> Option<(BcDirection, f32)> {
    const DEAD_ZONE: f32 = 0.01;
    let (direction, magnitude) = if x.abs() >= y.abs() {
        (
            if x < 0.0 {
                BcDirection::Left
            } else {
                BcDirection::Right
            },
            x.abs(),
        )
    } else {
        (
            if y < 0.0 {
                BcDirection::Down
            } else {
                BcDirection::Up
            },
            y.abs(),
        )
    };
    if magnitude < DEAD_ZONE {
        return None;
    }
    Some((direction, (magnitude.min(1.0) * MAX_SPEED).max(1.0)))
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
        let (d, s) = velocity_to_command(-0.5, 0.0).unwrap();
        assert!(d == BcDirection::Left && s == 32.0);
        let (d, _) = velocity_to_command(0.2, 0.6).unwrap();
        assert!(d == BcDirection::Up);
        let (d, s) = velocity_to_command(0.0, -1.5).unwrap();
        assert!(d == BcDirection::Down && s == 64.0);
        assert!(velocity_to_command(0.0, 0.0).is_none());
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
}
