#!/bin/sh
# Builds neolink.toml from the add-on options and starts the RTSP server.
# A /config/neolink.toml (host addon_configs/local_neolink) is used as-is instead.
set -eu

OPTIONS=/data/options.json
CUSTOM=/config/neolink.toml
GENERATED=/data/neolink.toml

export RUST_LOG="$(jq -r '.log_level // "info"' "$OPTIONS")"
if [ "$(jq -r '.tunnel_handshake == false' "$OPTIONS")" = "true" ]; then
    export NEOLINK_TUNNEL_HANDSHAKE=0
fi
local_ip="$(jq -r '.local_ip // ""' "$OPTIONS")"
if [ -n "$local_ip" ]; then
    export NEOLINK_LOCAL_IP="$local_ip"
fi
ulimit -n 65535 2>/dev/null || true

if [ -f "$CUSTOM" ]; then
    echo "Using $CUSTOM (add-on camera options ignored)"
    CONFIG="$CUSTOM"
else
    count="$(jq '.cameras | length' "$OPTIONS")"
    if [ "$count" -eq 0 ]; then
        echo "No cameras configured. Add one in the add-on Configuration tab." >&2
        exit 1
    fi
    missing="$(jq -r '.cameras[] | select((.address // "") == "" and (.uid // "") == "") | .name' "$OPTIONS")"
    if [ -n "$missing" ]; then
        echo "Camera(s) need an address or a uid: $missing" >&2
        exit 1
    fi

    # tojson output is a valid TOML basic string
    jq -r '
      def s: tojson;
      def opt($k): if (.[$k] // "") != "" then "\($k) = \(.[$k] | s)" else empty end;
      def optraw($k): if .[$k] != null then "\($k) = \(.[$k])" else empty end;
      "bind = \"0.0.0.0\"",
      "bind_port = \(.rtsp_port)",
      "",
      (.users[]? | "[[users]]", "name = \(.name | s)", "pass = \(.password | s)", ""),
      . as $root |
      (.cameras[] |
        "[[cameras]]",
        "name = \(.name | s)",
        "username = \(.username | s)",
        "password = \(.password | s)",
        opt("address"),
        opt("uid"),
        opt("stream"),
        opt("discovery"),
        optraw("channel_id"),
        optraw("enable_audio"),
        optraw("enable_low_latency"),
        optraw("idle_disconnect"),
        optraw("onvif_port"),
        optraw("ptz_speed"),
        optraw("ptz_tilt_speed"),
        optraw("ptz_nudge_ms"),
        "relay_delay_ms = \($root.relay_delay_ms // 500)",
        "use_splash = \($root.use_splash // false)",
        "")
    ' "$OPTIONS" > "$GENERATED"
    chmod 600 "$GENERATED"
    CONFIG="$GENERATED"

    port="$(jq -r '.rtsp_port' "$OPTIONS")"
    jq -r --arg port "$port" '.cameras[] | "  rtsp://<HA-IP>:\($port)/\(.name)/main  and  /\(.name)/sub" + (if .onvif_port then "  (ONVIF :\(.onvif_port))" else "" end)' "$OPTIONS" \
        | { echo "Streams:"; cat; }
fi

exec /usr/local/bin/neolink rtsp --config "$CONFIG"
