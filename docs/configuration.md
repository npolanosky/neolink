# Configuration Reference

Neolink is configured with a TOML file. See
[sample_config.toml](https://github.com/MutuallyAssuredDeployment/neolink/blob/master/sample_config.toml)
for a complete example.

## Global Settings

```toml
bind = "0.0.0.0"           # listen address (0.0.0.0 = all interfaces)
bind_port = 8554            # RTSP port (default: 8554)
```

### TLS

```toml
certificate = "/path/to/cert-and-key.pem"   # enables rtsps://
tls_client_auth = "none"                    # none | requested | required
```

### Users

Password-protect RTSP streams:

```toml
[[users]]
name = "me"
pass = "mepass"
```

Access with `rtsp://me:mepass@HOST:8554/CameraName`. If no `[[users]]` are
defined, anyone can connect without authentication.

## Camera Settings

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"

# Connection: address, uid, or both (address tried first)
address = "192.168.1.10:9000"    # direct IP connection
uid = "ABCDEF0123456789"        # UID-based discovery

# Stream selection
stream = "all"                   # "all" (default: main, sub and extern where the camera has
                                 # one), "both" (main and sub), "mainStream", "subStream",
                                 # "externStream" or "none"
permitted_users = ["me"]         # restrict to specific users (default: all)
enabled = true                   # disable without removing from config

# Audio & latency
enable_audio = true              # set false to save CPU (default: true)
enable_low_latency = false       # reduce stream delay (default: false)

# Diagnostics
debug = false                    # dump raw XML from camera
print_format = "None"            # "None", "Human", or "Xml" for status messages
update_time = false              # force camera clock sync on connect

# NVR channel (for multi-camera NVRs)
channel_id = 0                   # 0-indexed camera on NVR

# Offline cameras (see below)
use_splash = true                # default: true
splash_pattern = "snow"          # "snow" (default), "smpte", "black", "checkers-8", ...
```

### Offline cameras and `use_splash`

With `use_splash = true` (the default) each stream path serves a test pattern
until neolink has connected to the camera, so clients always find the path.

With `use_splash = false` the paths are not mounted until the camera first
connects after neolink starts: until then clients get a 404 (RTSP Not Found).
Once the camera has connected the stream stays mounted, and clients that
connect while the camera is offline wait for it instead. NVRs such as Frigate
(go2rtc, ffmpeg) retry after a 404, but **Blue Iris gives up on a camera for
good after a 404, so Blue Iris users should keep `use_splash = true`.**

## Discovery

When connecting by UID, the camera IP is discovered with these methods. Each
method implicitly enables all prior methods.

| Method | How it works | Requirements |
|---|---|---|
| `local` | UDP broadcast on local network | Network supports broadcasts |
| `remote` | Ask Reolink servers for camera IP, connect directly | Route to camera + camera can reach Reolink |
| `map` | Register our IP with Reolink, camera connects to us | Our IP and Reolink reachable from camera |
| `relay` | Reolink relays all traffic | Both sides can reach Reolink |
| `cellular` | Alias for map + relay only | For cellular cameras (skips local/remote) |

```toml
discovery = "relay"    # default (every method); use "cellular" for cellular cameras
```

If you know the camera's IP address, set `address` directly and skip discovery
entirely. Setting both `address` and `uid` tries the address first and falls
back to UID discovery, which suits cameras that move between networks.

Methods run concurrently and the first to connect wins. `relay_delay_ms`
(default 500) holds the relay back so the direct methods get a head start:

```toml
relay_delay_ms = 5000
```

### Cameras behind a VPN or travel router

`remote` and `map` work through a tunnel into neolink's LAN (for example a
travel router running UniFi Teleport or WireGuard): the camera connects to the
local address neolink registers with Reolink, and the handshake arrives from the
tunnel's address. Neolink registers the source IP of its default route; set the
`NEOLINK_LOCAL_IP` environment variable if that is not the address the camera
can reach. If a direct (remote/map) session completes its handshake but fails to log in,
neolink uses the relay for that camera for the next 10 minutes
(`NEOLINK_DIRECT_RETRY_SECS`) and then tries the direct path again.
`NEOLINK_TUNNEL_HANDSHAKE=0` restores the strict check that only
accepts the handshake from the camera's public address. Discovery traffic is
logged at debug level (`neolink_core::bc_protocol::connection::discovery=debug`).

## Pause & Idle Disconnect

Pause the stream when not needed to reduce bandwidth and camera load:

```toml
[cameras.pause]
on_motion = true     # pause when no motion detected
on_client = true     # pause when no RTSP client connected
timeout = 2.1        # seconds to wait after motion stops before pausing
```

### Battery Camera Idle Disconnect

For battery cameras, fully disconnect when idle to conserve power:

```toml
[[cameras]]
idle_disconnect = true    # disconnect 30s after last activity
```

Activity includes: active streams, motion detection, or MQTT commands.

The camera reconnects when a new RTSP client connects or an MQTT command is
received.

> **Note:** Push notification wakeup is no longer available (Google removed the
> required APIs).

## ONVIF PTZ

Neolink can expose an ONVIF device per camera so NVRs that drive PTZ over ONVIF
(such as Frigate) can pan, tilt and recall presets:

```toml
[[cameras]]
name = "Camera01"
onvif_port = 8001    # one port per camera; disabled when unset
ptz_speed = 32       # camera speed (1-64) for an ONVIF velocity of 0.5 (Frigate's arrows)
ptz_tilt_speed = 8   # optional, tilt speed when it should differ from ptz_speed
ptz_nudge_ms = 300   # optional (50-5000), stop each move after this long for small fixed steps
```

The service is at `http://<host>:<onvif_port>/onvif/device_service`. It runs
alongside the RTSP server, so only in the `rtsp` mode (also the default when no
subcommand is given) and the `mqtt-rtsp` mode. When `[[users]]` are configured
the same credentials (and `permitted_users`) are required as a WS-Security
UsernameToken.

Supported: ContinuousMove (pan/tilt; the camera moves one axis at a time, so the
larger of the two velocities wins), Stop, GetPresets, GotoPreset, GetStatus and
GetStreamUri (the main stream, or the sub stream when only `subStream` is
served). The Baichuan protocol reports no pan/tilt position, so relative or
absolute moves and autotracking are not offered. A ContinuousMove without a
Timeout stops after 10 seconds in case the Stop request is lost.

Frigate example:

```yaml
cameras:
  camera01:
    onvif:
      host: 192.168.1.20
      port: 8001
      user: frigate       # Frigate requires these; any value works without [[users]]
      password: secret
```

## MQTT

See [mqtt.md](mqtt.md) for full MQTT configuration and message reference.
