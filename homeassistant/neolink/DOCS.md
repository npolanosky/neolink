# Neolink

Proxies Reolink cameras that only speak the proprietary Baichuan protocol (port 9000,
battery cameras, E1, B800, etc.) to plain RTSP, so Frigate can use them like any other
camera. Optionally serves an ONVIF PTZ bridge per camera so Frigate's PTZ controls work.

## Configuration

Add one entry per camera under `cameras`:

| Option | Required | Notes |
|---|---|---|
| `name` | yes | Letters, digits, `_`, `-`. Becomes the RTSP path. |
| `username` / `password` | yes | Camera login (same as the Reolink app). |
| `address` | one of | `IP` or `IP:port`; port defaults to 9000. Use for cameras with a fixed IP. |
| `uid` | one of | Camera UID (Reolink app → device info). Works anywhere the camera has internet. |
| `stream` | no | `both`, `mainStream` or `subStream`. Unset: every stream the camera has (main, sub and, on some models, extern). Each stream used is a separate connection to the camera. |
| `discovery` | no | UID lookup: `local`, `remote`, `map`, `relay` (default), `cellular`. |
| `channel_id` | no | Channel on a Reolink NVR / hub, starting at 0. |
| `enable_audio` | no | Default `true`. Set `false` to save CPU. |
| `enable_low_latency` | no | Default `false`. |
| `idle_disconnect` | no | Battery cameras: disconnect when no client is watching. |
| `onvif_port` | no | Serve an ONVIF PTZ bridge for this camera on this port (one port per camera). |
| `ptz_speed` | no | Camera speed (1-64) for Frigate's PTZ arrows. Default 32; lower for finer control. |
| `ptz_tilt_speed` | no | Tilt speed if it should differ (tilt range is usually much smaller than pan). |
| `ptz_nudge_ms` | no | Stop every PTZ press after this many ms, giving small fixed steps (e.g. 300). Helps over the relay, where the Stop can arrive late. |

`address` and `uid` can both be set: neolink tries TCP to `address` first and falls back
to UID discovery when that fails. Useful for cameras that move between networks.

Global options:

| Option | Default | Notes |
|---|---|---|
| `rtsp_port` | 8654 | The add-on uses the host network and Frigate's go2rtc already owns 8554. |
| `log_level` | `info` | `RUST_LOG` syntax. For discovery troubleshooting: `info,neolink_core::bc_protocol::connection=debug` |
| `relay_delay_ms` | 5000 | UID discovery runs all methods at once and the first to connect wins. This holds the Reolink relay back so a direct path (remote/map) can win. |
| `use_splash` | `false` | Serve a "please stand by" test pattern while a camera is offline. Off means the stream is simply unavailable, so Frigate does not record or detect on the pattern. |
| `tunnel_handshake` | `true` | Accept a camera's direct handshake when it arrives through a VPN tunnel instead of from its public address. Turn off to restore upstream behaviour if a tunnelled camera fails to connect. |
| `local_ip` | (auto) | IP neolink registers with Reolink for cameras to connect back to. Defaults to the host's default-route IP. |

`users` (optional) password-protects the RTSP server and the ONVIF bridge. If empty,
anyone on the LAN can watch and move the cameras. `anyone` and `anonymous` are reserved
and cannot be used as user names.

### Advanced: own neolink.toml

Put a `neolink.toml` in `addon_configs/local_neolink/` (Samba / File editor). When present it
is used as-is and the options above are ignored. Set `bind_port = 8654` in it. See
`docs/configuration.md` in the repository for every option (MQTT, pause on motion, TLS, ...).

## Streams

For each camera:

- `rtsp://<HA-IP>:8654/<name>/main` – main stream
- `rtsp://<HA-IP>:8654/<name>/sub` – sub stream
- `rtsp://<HA-IP>:8654/<name>` – main (or sub when only `subStream` is enabled)

The first connect can take 10–30 s while neolink logs in to the camera.

## Cameras on the move (travel router, VPN)

With `uid` set, neolink finds the camera wherever it is. In the log:

- `TCP Discovery success` / `Local discovery success`: same network, direct.
- `Remote discovery success` / `Map success`: direct connection over the internet or a
  VPN tunnel. Full bandwidth.
- `Device initiated ... connection from <ip>`: the camera reached neolink through a
  tunnel (e.g. UniFi Teleport) rather than its public address. Direct path.
- `Relay success`: video flows through Reolink's servers. Works almost anywhere but
  bandwidth is limited (roughly 0.5–1 Mbps per stream) and Reolink can see the traffic.

## Offline cameras

When a camera is unreachable neolink keeps retrying in the background. With `use_splash`
off, a camera's stream paths return 404 until it first connects after the add-on starts:
Frigate shows the camera as offline and retries every `retry_interval` seconds, without
recording or detecting on anything. Once the camera has connected its paths stay up; if
it drops out later, clients that connect wait for the camera (up to their own timeout)
rather than getting a 404.

Blue Iris gives up on a camera for good after a 404, so turn `use_splash` on if you use it.

## Frigate

Restream through go2rtc so Frigate and the HA UI share one connection to neolink.
Replace `<HA-IP>` with the HA host IP and `cam1` with the camera name
(add `user:pass@` to the URLs if you set `users`):

```yaml
go2rtc:
  streams:
    cam1:
      - ffmpeg:rtsp://<HA-IP>:8654/cam1/main#video=copy#audio=aac

cameras:
  cam1:
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cam1
          input_args: preset-rtsp-restream
          roles: [record, detect, audio]
    detect:
      width: 640
      height: 360
    onvif:              # only with onvif_port set for the camera
      host: <HA-IP>
      port: 8001
      user: neolink     # required by Frigate's client; any value without add-on users
      password: neolink
```

Frigate's detect downscales the main stream itself, so a single stream per camera is
enough and keeps bandwidth to the camera down.

### PTZ through the ONVIF bridge

Frigate's ONVIF client fails with `bad argument type: NoneType(None)` when `user` and
`password` are missing. Set both; with no `users` in this add-on any values are accepted.

Frigate's live view gets pan/tilt arrows and the camera's presets. The camera moves one
axis at a time (no diagonals), reports no position, so autotracking is not available.
Presets are read from the camera; create them in the Reolink app.
