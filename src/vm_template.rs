use anyhow::{Context, Result};
use serde_json::json;

use crate::config::{Config, LIMA_VERSION};

pub const IMAGE_URL: &str = "https://cloud-images.ubuntu.com/minimal/releases/noble/release-20260905/ubuntu-24.04-minimal-cloudimg-arm64.img";
pub const IMAGE_MIRROR_URL: &str = "https://mirrors.nju.edu.cn/ubuntu-cloud-images/minimal/releases/noble/release-20260905/ubuntu-24.04-minimal-cloudimg-arm64.img";
pub const IMAGE_SHA256: &str = "8b6e0e145ae2ce681d959b2b3aabcf724b027cbffff9ec8ba4d7dc789a3e6a98";
pub const DOCKER_VERSION: &str = "29.8.0";
pub const DOCKER_URL: &str =
    "https://download.docker.com/linux/static/stable/aarch64/docker-29.8.0.tgz";
pub const DOCKER_SHA256: &str = "1462a696be6029bd478d7d60d7f3c31cdd15affd1178a4a278aaf4a1d1b7f8b5";
pub const UBUNTU_SNAPSHOT: &str = "20260906T000000Z";

/// JSON is a YAML subset. Serializing paths preserves their literal characters.
pub fn render(config: &Config) -> Result<String> {
    let mounts = config
        .mounts
        .iter()
        .map(|mount| {
            let path = mount.path.to_str().context("mount path must be UTF-8")?;
            Ok(json!({
                "location": path,
                "mountPoint": path,
                "writable": mount.writable
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let dependencies = DEPENDENCIES.replace("@SNAPSHOT@", UBUNTU_SNAPSHOT);
    let install = INSTALL_DOCKER
        .replace("@VERSION@", DOCKER_VERSION)
        .replace("@URL@", DOCKER_URL)
        .replace("@SHA256@", DOCKER_SHA256);

    let document = json!({
        "minimumLimaVersion": LIMA_VERSION,
        "vmType": "vz",
        "arch": "aarch64",
        "os": "Linux",
        "images": [
            { "location": IMAGE_URL, "arch": "aarch64", "digest": format!("sha256:{IMAGE_SHA256}") },
            { "location": IMAGE_MIRROR_URL, "arch": "aarch64", "digest": format!("sha256:{IMAGE_SHA256}") }
        ],
        "cpus": config.cpus,
        "memory": format!("{}GiB", config.memory_gib),
        "disk": format!("{}GiB", config.disk_gib),
        "mountType": "virtiofs",
        // Lima forwards changes on writable mounts. Host deletions need polling.
        "mountInotify": true,
        "mounts": mounts,
        "containerd": { "system": false, "user": false },
        "upgradePackages": false,
        "ssh": { "loadDotSSHPubKeys": false, "forwardAgent": false, "overVsock": false },
        "vmOpts": { "vz": { "rosetta": { "enabled": false, "binfmt": false } } },
        "hostResolver": {
            "enabled": true,
            "ipv6": false,
            "hosts": {
                "host.docker.internal": "host.lima.internal",
                "gateway.docker.internal": "host.lima.internal"
            }
        },
        "propagateProxyEnv": true,
        "portForwards": [
            { "guestSocket": "/var/run/docker.sock", "hostSocket": "{{.Dir}}/sock/docker.sock" },
            { "guestIP": "127.0.0.1", "guestPortRange": [1, 65535], "hostIP": "127.0.0.1", "hostPortRange": [1, 65535], "proto": "any" }
        ],
        "provision": [
            { "mode": "dependency", "skipDefaultDependencyResolution": false, "script": dependencies },
            { "mode": "data", "path": "/etc/docker/daemon.json", "content": "{\"data-root\":\"/var/lib/docker\",\"log-driver\":\"local\",\"features\":{\"containerd-snapshotter\":true}}\n", "owner": "root:root", "permissions": "0600" },
            { "mode": "data", "path": "/etc/systemd/system/docker.socket", "content": DOCKER_SOCKET, "owner": "root:root", "permissions": "0644" },
            { "mode": "data", "path": "/etc/systemd/system/docker.service", "content": DOCKER_SERVICE, "owner": "root:root", "permissions": "0644" },
            { "mode": "system", "script": install }
        ],
        "probes": [{
            "mode": "readiness",
            "description": "Docker Engine API is ready",
            "script": "#!/bin/sh\nset -eu\nresponse=$(curl --fail --silent --show-error --max-time 5 --unix-socket /var/run/docker.sock http://localhost/_ping)\ntest \"$response\" = OK\n",
            "hint": "Inspect guest service logs with: luhmen shell sudo journalctl -u docker.service --no-pager"
        }]
    });
    serde_json::to_string_pretty(&document).context("serialize VM configuration")
}

// This stage runs before Lima installs its own guest prerequisites.
const DEPENDENCIES: &str = r#"#!/bin/sh
set -eu
export DEBIAN_FRONTEND=noninteractive
# Pin the source URL itself: snapshot autodetection needs an existing Release
# index, which this minimal image does not contain on its first boot.
cat > /etc/apt/sources.list.d/ubuntu.sources <<'SOURCES'
Types: deb
URIs: https://snapshot.ubuntu.com/ubuntu/@SNAPSHOT@
Suites: noble noble-updates noble-security
Components: main universe
Architectures: arm64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
Snapshot: no
SOURCES
rm -f /etc/apt/apt.conf.d/50-luhmen-snapshot
if ! command -v iptables >/dev/null 2>&1 || ! command -v nft >/dev/null 2>&1 || ! command -v rsync >/dev/null 2>&1; then
    apt-get update -o APT::Update::Error-Mode=any
    apt-get install -y --no-install-recommends iptables nftables rsync curl ca-certificates
fi
"#;

const DOCKER_SOCKET: &str = r#"[Unit]
Description=Docker Engine API socket

[Socket]
ListenStream=/var/run/docker.sock
SocketMode=0600
SocketUser={{.User}}
RemoveOnStop=true

[Install]
WantedBy=sockets.target
"#;

const DOCKER_SERVICE: &str = r#"[Unit]
Description=Docker Engine
Requires=docker.socket
After=network-online.target docker.socket
Wants=network-online.target

[Service]
Type=notify
Environment=PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin
EnvironmentFile=-/etc/environment
ExecStart=/usr/local/bin/dockerd --host=fd:// --config-file=/etc/docker/daemon.json
ExecReload=/bin/kill -s HUP $MAINPID
Restart=on-failure
RestartSec=2s
TimeoutStartSec=120s
TimeoutStopSec=120s
LimitNOFILE=infinity
TasksMax=infinity
Delegate=yes
KillMode=process
OOMScoreAdjust=-500

[Install]
WantedBy=multi-user.target
"#;

const INSTALL_DOCKER: &str = r#"#!/bin/sh
set -eu
install_root=/opt/luhmen/docker/@VERSION@
install -d -m 0755 /opt/luhmen/docker
for abandoned in /opt/luhmen/docker/.install.*; do
    if [ -d "$abandoned" ]; then
        rm -rf "$abandoned"
    fi
done
if [ ! -f "$install_root/.complete" ]; then
    if [ -e "$install_root" ]; then
        echo "Incomplete Docker installation at $install_root; inspect this directory before retrying" >&2
        exit 1
    fi
    stage=$(mktemp -d /opt/luhmen/docker/.install.XXXXXX)
    trap 'rm -rf "$stage"' EXIT HUP INT TERM
    curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 --connect-timeout 20 --max-time 600 --retry 3 '@URL@' -o "$stage/docker.tgz"
    printf '%s  %s\n' '@SHA256@' "$stage/docker.tgz" | sha256sum --check --status
    tar -xzf "$stage/docker.tgz" -C "$stage"
    for binary in docker dockerd containerd containerd-shim-runc-v2 ctr runc docker-init docker-proxy; do
        test -x "$stage/docker/$binary"
    done
    printf '%s\n' '@SHA256@' > "$stage/docker/.complete"
    mv "$stage/docker" "$install_root"
    rm -rf "$stage"
    trap - EXIT HUP INT TERM
fi
for binary in docker dockerd containerd containerd-shim-runc-v2 ctr runc docker-init docker-proxy; do
    ln -sfn "$install_root/$binary" "/usr/local/bin/$binary"
done
systemctl daemon-reload
systemctl enable --now docker.socket
systemctl enable docker.service
systemctl restart docker.service
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mount;
    use std::path::PathBuf;

    #[test]
    fn preserves_resource_settings_and_literal_mount_paths() {
        let path = PathBuf::from("/Volumes/Projects/a \"quoted\" directory: with spaces");
        let config = Config {
            schema_version: 1,
            cpus: 3,
            memory_gib: 5,
            disk_gib: 40,
            mounts: vec![Mount {
                path: path.clone(),
                writable: true,
            }],
        };
        let document: serde_json::Value = serde_json::from_str(&render(&config).unwrap()).unwrap();
        assert_eq!(document["cpus"], 3);
        assert_eq!(document["memory"], "5GiB");
        assert_eq!(document["disk"], "40GiB");
        assert_eq!(
            document["mounts"],
            json!([{
                "location": path.to_str().unwrap(),
                "mountPoint": path.to_str().unwrap(),
                "writable": true
            }])
        );
        assert_eq!(document["portForwards"][1]["hostIP"], "127.0.0.1");
        assert_eq!(
            document["images"],
            json!([
                { "location": IMAGE_URL, "arch": "aarch64", "digest": format!("sha256:{IMAGE_SHA256}") },
                { "location": IMAGE_MIRROR_URL, "arch": "aarch64", "digest": format!("sha256:{IMAGE_SHA256}") }
            ])
        );
    }

    #[test]
    fn no_mounts_means_no_host_directories_are_shared() {
        let config = Config {
            schema_version: 1,
            cpus: 2,
            memory_gib: 2,
            disk_gib: 20,
            mounts: vec![],
        };
        let document: serde_json::Value = serde_json::from_str(&render(&config).unwrap()).unwrap();
        assert_eq!(document["mounts"], json!([]));
        assert_eq!(document["ssh"]["loadDotSSHPubKeys"], false);
        assert_eq!(document["ssh"]["forwardAgent"], false);
        assert!(document.get("base").is_none());
    }
}
